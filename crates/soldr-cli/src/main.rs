// Mirror lib.rs: the bin tree compiles the same `fetch::*`,
// `cache_lib::*`, `core::*` modules independently of the lib tree.
// Items declared for external (lib) callers are dead from the bin's
// perspective and would trip `-D warnings` on CI. lib.rs has the same
// allow.
#![allow(dead_code, unused_imports)]

use clap::Parser;

mod archive_cmd;
mod binaries;
/// soldr#1012 PR 5 — blessed cross-compile sysroot prep called from
/// Commands::Build.
mod blessed_build;
mod bootstrap;
mod build_from_source_cmd;
mod cache;
mod cache_lib;
mod cargo_diagnostics;
mod cargo_front_door;
mod cargo_metadata_soldr;
/// soldr#1081 — shared `Request::Compile` dispatch with hang-safe
/// retry budget. Used by `wrapper.rs` and the `zccache-soldr` bin.
/// soldr#1059 — classify the `cargo` binary that `which cargo` would
/// resolve to. Used by `toolchain doctor` / `toolchain ensure` to warn
/// when a Chocolatey-style standalone shadows rustup's proxy, defeating
/// per-crate `rust-toolchain.toml` overrides for subprocess invocations.
mod cargo_path_check;
mod cli_args;
mod compile_dispatch;
mod cook;
mod core;
mod daemon;
mod defender;
mod defender_probe;
mod doctor;
/// soldr#938 — `soldr env --target` subcommand. Prints shell-eval /
/// shell-export / JSON env block for the given target.
mod env_cmd;
/// soldr#1059 — `soldr exec <cmd>` escape hatch for cargo extensions
/// like cargo-dylint that hard-code `"cargo"` and would otherwise pick
/// up a Chocolatey/scoop standalone instead of rustup's proxy.
mod exec_cmd;
mod fetch;
mod fuzzy_match;
mod gc;
mod install_shims;
mod linker;
/// soldr#820 — `soldr logs` discoverable runtime-log surface.
/// Phase 1 ships the `paths` verb; future PRs layer `list` / `show`
/// / `view` / `prune` on the same dispatch arm.
mod logs_cmd;
/// soldr#1079 — Windows MSVC host-toolchain auto-discovery. Runs
/// before cargo for MSVC targets so `link.exe` + `LIB` are set
/// without the user touching `$env:LIB`.
mod msvc_host;
mod native_cc;
mod optimize;
mod optimize_detect;
mod optimize_windows;
mod prepare_cmd;
/// soldr#939 — PyO3 auto-detection via cargo metadata. Used by the
/// cargo front door to inject PYO3_CROSS_* env vars when the
/// workspace pulls in PyO3 and target ≠ host.
mod pyo3_detect;
mod release_sidecar;
mod rust_plan;
mod save_load;
mod self_relocate;
mod shim_dir;
mod startup_profile;
/// soldr#997 — friendly target aliases + Rust-triple passthrough.
/// Bin tree mirrors the lib declaration; only one alias resolver
/// is reachable in either build mode.
mod target_alias;
mod toolchain;
mod toolchain_doctor;
mod toolchain_ensure;
mod toolchain_link;
mod trampoline;
mod trampoline_workspace;
mod wrapper;
mod wrapper_target;
mod zccache;
/// Issue #977 / #980 L1 — embedded zccache service. Mirrors
/// `lib.rs`: the bin tree compiles the same module independently.
/// Unconditionally compiled — the legacy fork-zccache.exe wrapper
/// path has been deleted, the embedded service is mandatory.
mod zccache_embedded;
mod zccache_lifecycle;

// Per-test watchdog (`timed_test!` macro + `run_with_watchdog`).
// Declared (without a cfg gate) so unit tests under `src/` that use
// `crate::timed_test!` see the matching `test_util` module — `$crate`
// in the macro resolves to the bin crate for unit tests. The module
// is tiny and never invoked outside `#[test]` paths, so the
// production-binary cost is negligible. Mirrors `lib.rs`.
mod test_util;

use cli_args::{
    is_cargo_builtin_verb, CacheSubcommand, Cli, Commands, DaemonBuildsSubcommand,
    DaemonSubcommand, DefenderExclusionsSubcommand, GcCargoArgs, GcListKind, GcSubcommand,
    GcSweepArgs, LogsSubcommand, ToolchainSubcommand, TrimProfileArg, ZccacheSourceArg,
    CARGO_BUILTIN_VERBS, SOLDR_BUILTIN_VERBS,
};

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use crate::fetch::VersionSpec;

#[allow(unused_imports)]
pub(crate) use binaries::{
    apply_implicit_toolchain_homes, cached_active_zccache, cached_active_zccache_runtime,
    current_soldr_binary, fetch_active_zccache, fetch_active_zccache_runtime, non_empty_env_path,
    parse_tool_spec, resolve_toolchain_binary, rustup_binary, rustup_resolution_failure,
    zccache_binary_override,
};

pub(crate) const TEST_CARGO_BIN_ENV_VAR: &str = "SOLDR_TEST_CARGO_BIN";
pub(crate) const TEST_RUSTC_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTC_BIN";
pub(crate) const TEST_RUSTUP_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTUP_BIN";
pub(crate) const TEST_ZCCACHE_BIN_ENV_VAR: &str = "SOLDR_TEST_ZCCACHE_BIN";
pub(crate) const TEST_FREE_DISK_BYTES_ENV_VAR: &str = "SOLDR_TEST_FREE_DISK_BYTES";
/// Overrides the default `nightly` toolchain used by `soldr gc cargo`
/// to invoke cargo's unstable `-Zgc clean gc`. The `--toolchain` flag
/// on `gc cargo` / `gc sweep` always takes precedence.
pub(crate) const SOLDR_GC_CARGO_TOOLCHAIN_ENV_VAR: &str = "SOLDR_GC_CARGO_TOOLCHAIN";
pub(crate) const JSON_SCHEMA_VERSION: u32 = 1;
pub(crate) const LOW_DISK_WARNING_THRESHOLD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub(crate) const RUSTC_WRAPPER_OVERRIDE_ENV_VAR: &str = "SOLDR_RUSTC_WRAPPER";
pub(crate) const CARGO_PROFILE_DEV_DEBUG_ENV_VAR: &str = "CARGO_PROFILE_DEV_DEBUG";
pub(crate) const CARGO_PROFILE_TEST_DEBUG_ENV_VAR: &str = "CARGO_PROFILE_TEST_DEBUG";
/// Picks the linker injected for `soldr cargo ...` builds. See `linker` module
/// and `docs/API.md` for the supported values (`default | ld | mold | rust-lld
/// | fast`).
pub(crate) const LINKER_ENV_VAR: &str = "SOLDR_LINKER";
pub(crate) const REAL_TOOLCHAIN_BINARY_ENV_PREFIX: &str = "SOLDR_REAL_";
pub(crate) const TARGET_CACHE_MODE_ENV_VAR: &str = "SOLDR_TARGET_CACHE_MODE";
pub(crate) const TARGET_CACHE_BUNDLE_DIR_ENV_VAR: &str = "SOLDR_TARGET_CACHE_BUNDLE_DIR";
pub(crate) const TARGET_CACHE_BACKEND_ENV_VAR: &str = "SOLDR_TARGET_CACHE_BACKEND";
/// Selects which thin-slice pruning policy `soldr cargo` ships to zccache. See
/// `docs/THIN_TARGET_CACHE_PRUNING.md` for the rationale and rollout plan.
/// Values: `thin-v1` (legacy opt-out — keeps `.rlib`/`.rmeta`/proc-macro
/// outputs; useful when pinned to managed zccache < 1.9.1) and `thin-v2`
/// (default since soldr v0.7.31 / issue #461; fingerprint-aware aggressive
/// prune that drops library bytes and lets zccache's compilation cache
/// repopulate them).
pub(crate) const TARGET_CACHE_PROFILE_ENV_VAR: &str = "SOLDR_TARGET_CACHE_PROFILE";
/// Reader-thread count for the target-cache tar walk in zccache (issue #272).
/// Forwarded to the `zccache rust-plan save/restore` subprocess via inherited
/// environment; soldr validates the value early so typos fail before cargo
/// metadata runs. Values: `auto` (default; zccache picks a vCPU-bounded count
/// capped at 8), `1` (disable parallelism — sequential tar walk), or any
/// positive integer for an explicit thread count. The actual parallel walk
/// lives in zccache; this constant exists so soldr can reject malformed values
/// at the front door.
pub(crate) const TARGET_CACHE_TAR_THREADS_ENV_VAR: &str = "SOLDR_TARGET_CACHE_TAR_THREADS";
/// Filename of the file-list manifest written next to the thin-slice bundle so
/// downstream tooling can prove what landed in the slice without unpacking it.
pub(crate) const THIN_MANIFEST_FILENAME: &str = "manifest.v2.json";
/// Flag controlling the warm-restore short-circuit (issue #229). Default-on:
/// after a successful `rust-plan save` soldr writes a sentinel describing the
/// plan/job, and on the next `soldr cargo ...` invocation the matching
/// `rust-plan restore` is skipped if the sentinel proves the `target/` tree
/// is already in the exact state restore would produce. This preserves
/// Cargo's mtime-based fingerprints across split CI steps. Set to a falsy
/// value (`0` / `false` / `no` / `off` / empty, case-insensitive) to opt out;
/// unset or any other value keeps the short-circuit enabled.
pub(crate) const SKIP_WARM_RESTORE_ENV_VAR: &str = "SOLDR_RUST_PLAN_SKIP_WARM_RESTORE";
/// Opt-in escape hatch for advanced workflows that intentionally inject
/// soldr/zccache workspace-pinned state into `soldr cargo ...`.
pub(crate) const TRUST_INHERITED_SOLDR_ENV_VAR: &str = "SOLDR_TRUST_INHERITED_ENV";
/// Filename of the sentinel written next to the thin-slice bundle root after
/// a successful `rust-plan save`. Read on the next invocation by
/// `should_skip_warm_restore` to decide whether `rust-plan restore` would be
/// a no-op-but-touches-mtimes operation against an already-warm `target/`.
pub(crate) const WARM_RESTORE_SENTINEL_FILENAME: &str = "last-save.json";
/// Maximum age of a warm-restore sentinel before it is treated as stale and
/// ignored. Five minutes comfortably covers a normal `cargo test --no-run`
/// followed by `cargo test` step pair on GitHub Actions while keeping the
/// short-circuit from kicking in on later, unrelated jobs.
pub(crate) const WARM_RESTORE_MAX_AGE_SECONDS: u64 = 5 * 60;

/// Pin a specific soldr version to handle this invocation. Explicit
/// `--as <version>` flag takes precedence over this env var.
const SOLDR_AS_ENV_VAR: &str = "SOLDR_AS";
/// Sentinel that the currently-running soldr was itself invoked by another
/// soldr through `--as`. Prevents infinite hand-offs.
const SOLDR_TRAMPOLINING_ENV_VAR: &str = "SOLDR_TRAMPOLINING";

#[tokio::main]
async fn main() {
    // RUSTC_WRAPPER mode: cargo passes `soldr /path/to/rustc <args...>`
    // Must be checked before clap parsing.
    let raw_args: Vec<String> = std::env::args().collect();
    if should_self_relocate_for_invocation(&raw_args) {
        match self_relocate::maybe_reexec_from_runtime(&raw_args) {
            Ok(Some(code)) => std::process::exit(code),
            Ok(None) => {}
            Err(error) => std::process::exit(report_and_exit(error)),
        }
    }

    if raw_args.len() > 1 && wrapper::is_wrapper_invocation(&raw_args[1]) {
        // Per-phase startup timing for #440. `WrapperProfile::new()` is a
        // cheap branch + one `var_os` syscall when SOLDR_PROFILE_STARTUP
        // is unset, so the dominant production path pays effectively
        // nothing. When set, the profile captures `Instant::now()` at
        // each boundary down to the exec call.
        let mut profile = startup_profile::WrapperProfile::new();
        profile.mark("args_collected");
        if let Some(version) = soldr_as_env_pin() {
            if should_trampoline(&version) {
                std::process::exit(
                    run_trampoline(&version, &raw_args[1..])
                        .await
                        .unwrap_or_else(report_and_exit),
                );
            }
        }
        profile.mark("pin_check_done");
        std::process::exit(
            wrapper::run_rustc_wrapper(&raw_args, profile).unwrap_or_else(report_and_exit),
        );
    }

    // `--as <version>` trampoline. Peeled off before clap so the fetched
    // older soldr parses its own argv on its own terms.
    let (pinned_version, trampoline_args) = match extract_as_pin(&raw_args[1..]) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("soldr: {e}");
            std::process::exit(1);
        }
    };
    let pinned_version = pinned_version.or_else(soldr_as_env_pin);

    if let Some(version) = pinned_version {
        if should_trampoline(&version) {
            std::process::exit(
                run_trampoline(&version, &trampoline_args)
                    .await
                    .unwrap_or_else(report_and_exit),
            );
        }
        // Short-circuit: requested version == current. Continue with args
        // that have `--as <ver>` stripped.
        std::process::exit(
            run_with_args(&raw_args[0], &trampoline_args)
                .await
                .unwrap_or_else(report_and_exit),
        );
    }

    let rc = run_with_args(&raw_args[0], &raw_args[1..])
        .await
        .unwrap_or_else(report_and_exit);
    std::process::exit(rc);
}

fn soldr_as_env_pin() -> Option<String> {
    std::env::var(SOLDR_AS_ENV_VAR)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

async fn run_with_args(prog: &str, args: &[String]) -> Result<i32, SoldrError> {
    let mut argv: Vec<String> = Vec::with_capacity(args.len() + 1);
    argv.push(prog.to_string());
    argv.extend(args.iter().cloned());
    // Use parse_from (not try_parse_from) so clap handles --help / --version /
    // usage errors with its built-in exit(0) / exit(2), matching the original
    // invocation path's UX exactly.
    let cli = Cli::parse_from(argv);
    run_cli(cli).await.map(|_| 0)
}

async fn run_cli(cli: Cli) -> Result<(), SoldrError> {
    let cache_enabled = !cli.no_cache;
    let zccache_source = cli.zccache;
    let trust_inherited_soldr_env = cli.trust_inherited_soldr_env;

    match cli.command {
        Commands::Build { args } => {
            // soldr#1012 PR 1 + PR 5. The `soldr build` surface
            // routes through `blessed_build::prepare` for any
            // canonical target triple it can identify in argv (looks
            // for `--target X` / `--target=X`). On MSVC targets that
            // step materializes the xwin-cache from the soldr-
            // toolchain catalogue, installs the soldr-clang-shim
            // ahead of system clang on PATH, and sets the cc-rs +
            // cargo target-specific env vars. The cargo front door is
            // then invoked with the same args + the prep env applied.
            //
            // Targets with no prep need (linux musl, linux gnu) get
            // a no-op prep + the standard cargo front door behavior.
            // `SOLDR_USE_LEGACY_XWIN=1` opts out of the blessed path
            // and falls through to the unchanged cargo-xwin flow.
            let mut full_args = Vec::with_capacity(args.len() + 1);
            full_args.push("build".to_string());
            full_args.extend(args);

            // Try to recognize a target from argv so we can prep
            // before invoking cargo. If the user didn't pass `--target`,
            // only the host-side prep (managed cmake/ninja) runs and we
            // forward otherwise unchanged.
            if let Some(target_triple) = extract_target_from_args(&full_args) {
                let paths = crate::core::SoldrPaths::new()?;
                let prep = crate::blessed_build::prepare(&paths, &target_triple).await?;
                let cargo_args = prep.cargo_args.clone();
                // Apply prep env onto the current process env so the
                // child cargo invocation (and its sub-rustc + build
                // scripts) inherit them.
                for (k, v) in &prep.env {
                    std::env::set_var(k, v);
                }
                if let Some(shim_dir) = prep.shim_path_dir.as_ref() {
                    prepend_to_path_env(shim_dir);
                }
                for dir in &prep.path_dirs {
                    prepend_to_path_env(dir);
                }

                // soldr#882: auto-dispatch cargo subcommand based on
                // target. *-pc-windows-msvc routes through `cargo xwin
                // build`, *-apple-darwin / *-unknown-linux-musl /
                // cross-arch *-unknown-linux-gnu route through
                // `cargo zigbuild`. Opt-out via SOLDR_USE_LEGACY_{XWIN,
                // ZIGBUILD}=1 (same env vars blessed_build::prepare
                // already honors for sysroot prep). cfg-gated to linux
                // hosts — native msvc/darwin host builds keep using
                // plain cargo build.
                if let Some(subcmd) = pick_cross_subcommand(&target_triple) {
                    full_args = rewrite_build_args_for_subcommand(full_args, subcmd);
                }
                full_args = insert_cargo_config_args(full_args, &cargo_args);
            } else {
                // Native host build (no --target): the cross-compile
                // sysroot prep doesn't apply, but the managed cmake +
                // ninja injection does — cmake-based *-sys build
                // scripts run on the host regardless of target, and
                // "use whatever cmake/make PATH serves" is exactly the
                // failure mode soldr exists to remove (a pip-installed
                // MSYS make + "MSYS Makefiles" generator broke native
                // libz-ng-sys builds — see fetch::cmake_tools).
                let paths = crate::core::SoldrPaths::new()?;
                let mut prep = crate::blessed_build::BlessedPrep::default();
                crate::blessed_build::inject_cmake_tooling(&paths, &mut prep).await;
                for (k, v) in &prep.env {
                    std::env::set_var(k, v);
                }
                for dir in &prep.path_dirs {
                    prepend_to_path_env(dir);
                }
            }

            // soldr#1079: ensure native Windows MSVC builds get LIB /
            // INCLUDE / PATH (link.exe) injected from the host VS
            // install, so users invoking `soldr build` from a plain
            // PowerShell don't have to set `$env:LIB` themselves.
            // No-op when not on Windows, when the user opted out via
            // `SOLDR_MSVC_DISCOVERY=off`, when LIB is already set, or
            // when the resolved target is non-MSVC.
            ensure_msvc_host_env_for_native(&full_args);

            std::process::exit(
                cargo_front_door::run_cargo_front_door(
                    &full_args,
                    cache_enabled,
                    zccache_source,
                    trust_inherited_soldr_env,
                )
                .await?,
            );
        }
        Commands::Cargo { args } => {
            // soldr#1079: same MSVC host env injection that
            // `Commands::Build` does, so `soldr cargo build` /
            // `soldr cargo test` on a native Windows MSVC target also
            // succeed from a plain PowerShell without `$env:LIB`.
            ensure_msvc_host_env_for_native(&args);
            std::process::exit(
                cargo_front_door::run_cargo_front_door(
                    &args,
                    cache_enabled,
                    zccache_source,
                    trust_inherited_soldr_env,
                )
                .await?,
            );
        }
        Commands::Cook { args } => {
            std::process::exit(cook::run_cook(&args, cache_enabled, zccache_source).await?);
        }
        Commands::Exec { args } => {
            std::process::exit(exec_cmd::run_exec(&args)?);
        }
        Commands::Rustc { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough("rustc", &args)?);
        }
        Commands::Rustfmt { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough("rustfmt", &args)?);
        }
        Commands::ClippyDriver { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough(
                "clippy-driver",
                &args,
            )?);
        }
        Commands::Rustdoc { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough("rustdoc", &args)?);
        }
        Commands::RustGdb { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough("rust-gdb", &args)?);
        }
        Commands::RustLldb { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough("rust-lldb", &args)?);
        }
        Commands::RustAnalyzer { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough(
                "rust-analyzer",
                &args,
            )?);
        }
        Commands::Rustup { args } => {
            std::process::exit(toolchain::run_rustup_passthrough(&args)?);
        }
        Commands::Toolchain { subcommand } => match subcommand {
            ToolchainSubcommand::Install => {
                std::process::exit(toolchain::run_toolchain_install()?);
            }
            ToolchainSubcommand::Prepare => {
                std::process::exit(toolchain::run_toolchain_prepare()?);
            }
            ToolchainSubcommand::Ensure { json } => {
                std::process::exit(toolchain_ensure::run_toolchain_ensure(json).await?);
            }
            ToolchainSubcommand::Link {
                shim_dir,
                json,
                force,
            } => {
                std::process::exit(toolchain_link::run_toolchain_link(
                    toolchain_link::LinkArgs {
                        shim_dir,
                        json,
                        force,
                    },
                )?);
            }
            ToolchainSubcommand::Doctor { json } => {
                std::process::exit(toolchain_doctor::run_toolchain_doctor(json)?);
            }
            ToolchainSubcommand::Catalogue { json } => {
                std::process::exit(
                    crate::fetch::manifest_lookup::run_toolchain_catalogue(json).await?,
                );
            }
        },
        Commands::Bootstrap { json } => {
            std::process::exit(bootstrap::run_bootstrap(json).await?);
        }
        Commands::Doctor {
            json,
            refresh_defender_probe,
        } => {
            std::process::exit(doctor::run_doctor(json, refresh_defender_probe)?);
        }
        Commands::Optimize(args) => {
            std::process::exit(optimize::run_optimize(args)?);
        }
        Commands::Shims { json } => {
            let paths = SoldrPaths::new()?;
            std::process::exit(install_shims::run_shims(&paths, json)?);
        }
        Commands::DefenderExclusions { subcommand } => {
            std::process::exit(optimize::run_defender_exclusions(subcommand)?);
        }
        Commands::InstallZccache {
            source,
            remove,
            status,
            json,
        } => {
            cache::run_install_zccache(source, remove, status, json).await?;
        }
        Commands::Save(args) => {
            std::process::exit(save_load::run_save(args));
        }
        Commands::Load(args) => {
            std::process::exit(save_load::run_load(args));
        }
        Commands::Archive { target, output } => {
            archive_cmd::run(target, output)?;
        }
        Commands::Prepare {
            target,
            github_env,
            save,
            restore,
        } => {
            // `--target` accepts three shapes — see
            // `prepare_cmd::parse_target_arg` for the parser:
            //   - `all`         → every triple under
            //                     `[workspace.metadata.soldr].targets`
            //                     (needs a workspace context — #914).
            //   - `<a>,<b>,<c>` → an explicit comma-separated list.
            //                     Useful for docker-image bake steps
            //                     where no Cargo.toml is mounted yet.
            //   - `<triple>`    → a single triple (legacy default).
            let targets: Vec<String> = match prepare_cmd::parse_target_arg(&target)? {
                prepare_cmd::ParsedTargetArg::All => cargo_metadata_soldr::resolve_all_targets()?,
                prepare_cmd::ParsedTargetArg::Explicit(list) => list,
            };
            // soldr#940 — run per-target preparations concurrently with
            // a bounded worker pool. `--target all` previously serialized
            // 8 cold downloads on top of each other; now they overlap.
            // Per-target dispatch (zig + Apple SDK, LLVM + xwin, …) is
            // also internally parallelized — see `prepare_cmd::run`.
            //
            // Concurrency cap: min(num_cpus, num_targets, 4). 4 is the
            // GitHub-runner-friendly ceiling — beyond that the
            // contention on the NIC dominates the parallelism win.
            let cpu_cap = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(2);
            let concurrency = cpu_cap.min(targets.len()).clamp(1, 4);
            if targets.len() > 1 {
                eprintln!(
                    "soldr prepare: parallelizing {} targets with {} workers (soldr#940)",
                    targets.len(),
                    concurrency,
                );
            }
            let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
            let mut handles = Vec::with_capacity(targets.len());
            for triple in &targets {
                let triple_owned = triple.clone();
                let github_env_clone = github_env.clone();
                let save_clone = save.clone();
                let restore_clone = restore.clone();
                let sem_clone = std::sync::Arc::clone(&sem);
                handles.push(tokio::spawn(async move {
                    let _permit = sem_clone
                        .acquire_owned()
                        .await
                        .expect("semaphore not closed");
                    eprintln!("soldr prepare: ===== target {triple_owned} =====");
                    let result = prepare_cmd::run(
                        triple_owned.clone(),
                        github_env_clone,
                        save_clone,
                        restore_clone,
                    )
                    .await;
                    (triple_owned, result)
                }));
            }
            let mut failures: Vec<(String, String)> = Vec::new();
            for handle in handles {
                let (triple, result) = handle
                    .await
                    .map_err(|e| SoldrError::Other(format!("prepare worker join: {e}")))?;
                if let Err(e) = result {
                    eprintln!("soldr prepare: target {triple} failed: {e}");
                    failures.push((triple, e.to_string()));
                }
            }
            if !failures.is_empty() {
                let summary = failures
                    .iter()
                    .map(|(t, e)| format!("  {t}: {e}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(SoldrError::Other(format!(
                    "soldr prepare: {} of {} target(s) failed:\n{summary}",
                    failures.len(),
                    targets.len()
                )));
            }
        }
        Commands::BuildFromSource {
            tool,
            target,
            version,
        } => {
            build_from_source_cmd::run(&tool, target, version)?;
        }
        Commands::Env {
            target,
            shell_export,
            json,
        } => {
            std::process::exit(env_cmd::run_env_command(&target, shell_export, json)?);
        }
        Commands::Status { json } => {
            let output = cache::collect_status_output(cache_enabled)?;
            if json {
                cache::print_json(&output)?;
            } else {
                cache::print_status_output(&output);
            }
        }
        Commands::Clean => {
            cache::clear_zccache_cache()?;
        }
        Commands::Purge => {
            cache::purge_soldr_cache()?;
        }
        Commands::Config => {
            println!("(config not yet implemented)");
        }
        Commands::Logs { command } => match command {
            // soldr#820 phase 1 — `paths` is the only implemented verb today.
            Some(LogsSubcommand::Paths { json }) => {
                std::process::exit(logs_cmd::run_logs_paths(json)?);
            }
            None => {
                // Bare `soldr logs` with no subcommand: print the help-shaped
                // overview from the issue's design + a hint that today only
                // `paths` is implemented. Other verbs are planned follow-ups.
                eprintln!("soldr logs — inspect soldr's runtime activity (issue #820)");
                eprintln!();
                eprintln!("Subcommands:");
                eprintln!("  soldr logs paths    Print every directory soldr writes logs into");
                eprintln!();
                eprintln!("Planned follow-up verbs (not implemented yet):");
                eprintln!("  soldr logs list                List recent launches");
                eprintln!("  soldr logs show <launch-id>    Session summary + log paths");
                eprintln!("  soldr logs view <launch-id>    Stream a launch's JSONL journal");
                eprintln!("  soldr logs prune --keep N      Bounded retention sweep");
                eprintln!();
                eprintln!("Run `soldr logs paths --json` for a machine-readable form.");
                std::process::exit(0);
            }
        },
        Commands::Cache { json, command } => match command {
            Some(CacheSubcommand::Report { json: report_json }) => {
                cache::run_cache_report_command(report_json || json)?;
            }
            Some(CacheSubcommand::Shutdown {
                archive_logs,
                no_depgraph_save,
                shutdown_timeout_seconds,
                no_wait,
                json: shutdown_json,
            }) => {
                cache::run_cache_shutdown_command(
                    archive_logs,
                    no_depgraph_save,
                    shutdown_timeout_seconds,
                    !no_wait,
                    shutdown_json || json,
                )
                .await?;
            }
            Some(CacheSubcommand::Flush { json: flush_json }) => {
                cache::run_cache_flush_command(flush_json || json).await?;
            }
            Some(CacheSubcommand::PruneTarget {
                path,
                dry_run,
                no_dry_run,
                force,
                keep_latest,
                json: prune_json,
            }) => {
                let effective_dry_run = !(force || no_dry_run);
                // Either flag pair maps onto the same boolean; `dry_run`
                // is the documented default so we accept it explicitly.
                let _ = dry_run;
                cache::run_cache_prune_target_command(
                    path,
                    effective_dry_run,
                    keep_latest,
                    prune_json || json,
                )?;
            }
            Some(CacheSubcommand::TrimTarget {
                path,
                profile,
                dry_run,
                no_dry_run,
                force,
                json: trim_json,
            }) => {
                let effective_dry_run = !(force || no_dry_run);
                let _ = dry_run;
                let trim_profile = match profile {
                    TrimProfileArg::Local => cache::TrimProfile::Local,
                    TrimProfileArg::Ci => cache::TrimProfile::Ci,
                };
                cache::run_cache_trim_target_command(
                    path,
                    trim_profile,
                    effective_dry_run,
                    trim_json || json,
                )?;
            }
            Some(CacheSubcommand::ReleaseWorktree {
                path,
                json: rw_json,
            }) => {
                cache::run_cache_release_worktree_command(path, rw_json || json)?;
            }
            Some(CacheSubcommand::SweepTrash { json: st_json }) => {
                cache::run_cache_sweep_trash_command(st_json || json)?;
            }
            None => {
                let output = cache::collect_cache_output()?;
                if json {
                    cache::print_json(&output)?;
                } else {
                    cache::print_cache_output(&output);
                }
            }
        },
        Commands::Version { json } => {
            let output = cache::version_output();
            if json {
                cache::print_json(&output)?;
            } else {
                println!("soldr {}", output.soldr_version);
            }
        }
        Commands::Gc {
            dry_run,
            all,
            older_than,
            larger_than,
            json,
            command,
        } => {
            if all {
                return Err(SoldrError::Other(
                    "`soldr gc --all` no longer deletes targets; use `soldr gc purge --all`".into(),
                ));
            }
            if dry_run && command.is_some() {
                return Err(SoldrError::Other(
                    "`soldr gc --dry-run` is a summary alias; use `soldr gc` or `soldr gc purge`"
                        .into(),
                ));
            }
            let invocation = match command {
                Some(GcSubcommand::Purge {
                    all,
                    older_than,
                    larger_than,
                    json,
                    kind,
                    registry_src,
                    git_checkouts,
                    target_incremental,
                    build_scripts,
                    doc,
                    subcommand_caches,
                }) => {
                    // #323 slice 2: --registry-src is a shorthand for
                    // --kind cargo_registry_src; clap already enforces
                    // mutual exclusion.
                    // #323 slice 3: --git-checkouts is a shorthand for
                    // --kind cargo_git_checkouts.
                    // #323 slice 4: in-target subtree shorthands map to
                    // their explicit taxonomy kinds.
                    let effective_kind = if registry_src {
                        Some(GcListKind::CargoRegistrySrc)
                    } else if git_checkouts {
                        Some(GcListKind::CargoGitCheckouts)
                    } else if target_incremental {
                        Some(GcListKind::CargoTargetIncremental)
                    } else if build_scripts {
                        Some(GcListKind::CargoTargetBuildScriptBinaries)
                    } else if doc {
                        Some(GcListKind::CargoTargetDoc)
                    } else if subcommand_caches {
                        Some(GcListKind::CargoTargetSubcommandCaches)
                    } else {
                        kind
                    };
                    match effective_kind {
                        Some(GcListKind::CargoRegistrySrc) => {
                            gc::run_gc_purge_registry_src_command(all, json)?;
                            return Ok(());
                        }
                        Some(GcListKind::CargoGitCheckouts) => {
                            gc::run_gc_purge_git_checkouts_command(all, json)?;
                            return Ok(());
                        }
                        Some(
                            GcListKind::CargoTargetIncremental
                            | GcListKind::CargoTargetBuildScriptBinaries
                            | GcListKind::CargoTargetDoc
                            | GcListKind::CargoTargetSubcommandCaches,
                        ) => {
                            gc::run_gc_purge_target_subtree_command(
                                effective_kind.expect("matched Some").into(),
                                all,
                                json,
                            )?;
                            return Ok(());
                        }
                        Some(
                            GcListKind::CargoRegistryCache
                            | GcListKind::CargoGitDb
                            | GcListKind::CargoInstalledBinaries
                            | GcListKind::RustupToolchain,
                        ) => {
                            let kind_name = match effective_kind.expect("matched Some") {
                                GcListKind::CargoRegistryCache => "cargo_registry_cache",
                                GcListKind::CargoGitDb => "cargo_git_db",
                                GcListKind::CargoInstalledBinaries => "cargo_installed_binaries",
                                GcListKind::RustupToolchain => "rustup_toolchain",
                                _ => "selected kind",
                            };
                            return Err(SoldrError::Other(format!(
                                "gc purge --kind {kind_name} is report-only; cargo/rustup own deletion for this primary cache"
                            )));
                        }
                        Some(GcListKind::CargoTarget) | None => gc::GcInvocation {
                            mode: gc::GcMode::Purge { all },
                            older_than,
                            larger_than,
                            json,
                        },
                    }
                }
                Some(GcSubcommand::List { json, kind }) => {
                    gc::run_gc_list_command(json, kind.map(Into::into))?;
                    return Ok(());
                }
                Some(GcSubcommand::Cargo(args)) => {
                    gc::run_gc_cargo_command(*args)?;
                    return Ok(());
                }
                Some(GcSubcommand::Locations { json }) => {
                    gc::run_gc_locations_command(json)?;
                    return Ok(());
                }
                Some(GcSubcommand::Sweep(args)) => {
                    gc::run_gc_sweep_command(*args)?;
                    return Ok(());
                }
                Some(GcSubcommand::Target(args)) => {
                    gc::run_gc_target_command(*args)?;
                    return Ok(());
                }
                None => gc::GcInvocation {
                    mode: gc::GcMode::Summary,
                    older_than,
                    larger_than,
                    json,
                },
            };
            gc::run_gc_command(invocation)?;
        }
        Commands::SessionStart {
            id,
            log,
            journal,
            json,
        } => {
            cache::run_session_start_command(id, log, journal, json).await?;
        }
        Commands::SessionEnd { id, clear, json } => {
            cache::run_session_end_command(id, clear, json)?;
        }
        Commands::Daemon { command } => {
            run_daemon_command(command).await?;
        }
        Commands::External(args) => {
            if args.is_empty() {
                eprintln!("usage: soldr <tool>[@version] [args...]");
                std::process::exit(1);
            }

            let (crate_name, version) = parse_tool_spec(&args[0]);
            let tool_args = &args[1..];

            // Issue #683 (parent #682, phase 1): bare cargo-subcommand
            // shorthand. When the typed verb (sans `@version`) is one
            // soldr already prebuilds as a cargo subcommand
            // (`KNOWN_TOOLS::lookup_by_cargo_subcommand`), route through
            // the cargo front door — `soldr nextest run` becomes
            // `soldr cargo nextest run`. This avoids the doomed
            // crates.io fetch for a literally-named `nextest` crate.
            // Version-pinned forms (`soldr nextest@0.9.x`) keep the
            // existing External path; cargo-subcommand pins are
            // managed in the soldr registry and the front door has no
            // per-invocation knob.
            if matches!(version, VersionSpec::Latest)
                && crate::fetch::lookup_by_cargo_subcommand(&crate_name).is_some()
            {
                let mut cargo_args = Vec::with_capacity(args.len());
                cargo_args.push(crate_name.clone());
                cargo_args.extend(tool_args.iter().cloned());
                std::process::exit(
                    cargo_front_door::run_cargo_front_door(
                        &cargo_args,
                        cache_enabled,
                        zccache_source,
                        trust_inherited_soldr_env,
                    )
                    .await?,
                );
            }

            // Issue #685 (parent #682, phase 2): bare cargo built-in
            // shorthand. When the typed verb is one of cargo's own
            // first-party verbs (`build`, `test`, `check`, `clippy`,
            // `fmt`, ...), route through the cargo front door —
            // `soldr build --release` becomes `soldr cargo build
            // --release`. The collision verbs `clean` / `config` /
            // `version` are captured by clap before reaching this
            // arm; see `is_cargo_builtin_verb` for the explicit
            // exclusion list. Version-pinned forms keep the existing
            // External fetch path so `soldr build@1.0` parses
            // exactly like `soldr <unknown-tool>@1.0` does today.
            if matches!(version, VersionSpec::Latest) && is_cargo_builtin_verb(&crate_name) {
                let mut cargo_args = Vec::with_capacity(args.len());
                cargo_args.push(crate_name.clone());
                cargo_args.extend(tool_args.iter().cloned());
                // soldr#1105: bare-verb dispatch must also pre-inject
                // the host MSVC env so `soldr check` / `soldr build` /
                // `soldr test` on Windows behave the same as the
                // explicit `soldr cargo ...` forms with respect to
                // rust-lld's `LIB` requirement.
                ensure_msvc_host_env_for_native(&cargo_args);
                std::process::exit(
                    cargo_front_door::run_cargo_front_door(
                        &cargo_args,
                        cache_enabled,
                        zccache_source,
                        trust_inherited_soldr_env,
                    )
                    .await?,
                );
            }

            if should_use_managed_zccache_external(&crate_name, &version) {
                eprintln!("soldr: fetching managed zccache...");
                let paths = SoldrPaths::new()?;
                let result = fetch_active_zccache(&paths).await?;
                if result.cached {
                    eprintln!("soldr: using cached zccache v{}", result.version);
                } else {
                    eprintln!("soldr: downloaded zccache v{}", result.version);
                }

                let mut command = std::process::Command::new(&result.binary_path);
                command.args(tool_args);
                suppress_windows_console_window(&mut command);
                let status = command.status()?;
                std::process::exit(status.code().unwrap_or(1));
            }

            // Issue #412: when the user typed a verb that LOOKS like
            // a typo or a renamed built-in (e.g. `update-zccacheee`,
            // `installzccache`), emit a "did you mean?" hint before
            // we fire the network fetch. The fetch still runs — the
            // suggestion is advisory.
            if let Some(suggestion) =
                fuzzy_match::suggest_close_match(&crate_name, SOLDR_BUILTIN_VERBS)
            {
                eprintln!("soldr: '{crate_name}' is not a known built-in soldr verb.");
                eprintln!("soldr: did you mean: {suggestion}?");
            }

            eprintln!("soldr: fetching {crate_name}...");
            // soldr#1264 follow-on: maturin gets a provisioning ladder
            // instead of the bare fetch — prebuilt binary from GitHub
            // Releases first, manual uv-provisioned isolated env as
            // the fallback (SOLDR_MATURIN_PROVISIONER=auto|binary|uv).
            // Everything else keeps the plain fetch_tool path.
            let result = if crate_name == "maturin" {
                fetch_maturin_with_provisioner(&version).await?
            } else {
                crate::fetch::fetch_tool(&crate_name, &version).await?
            };

            if result.cached {
                eprintln!("soldr: using cached {crate_name} v{}", result.version);
            } else {
                eprintln!("soldr: downloaded {crate_name} v{}", result.version);
            }

            let mut command = std::process::Command::new(&result.binary_path);
            command.args(tool_args);

            // soldr#1264: `soldr maturin ...` is the engine behind the
            // PEP 517 build backend (src/soldr/__init__.py). maturin
            // spawns `cargo` itself, and on Windows the #493 `.cmd`
            // PATH shims below are invisible to Rust-spawned children
            // (CreateProcess resolves only `cargo.exe`, never `.cmd`),
            // so on a PATH-poisoned machine (e.g. a chocolatey GNU
            // cargo ahead of rustup's proxies) maturin silently builds
            // the wrong toolchain and cmake-based *-sys deps explode
            // in "MSYS Makefiles" flag mangling. Pin the child's
            // toolchain + build tools before exec:
            //   * `CARGO` → soldr's resolved rustup cargo (honors
            //     rust-toolchain.toml + MSVC-on-Windows). maturin
            //     reads `CARGO` before falling back to bare PATH
            //     lookup. A caller-provided CARGO always wins.
            //   * managed cmake/ninja env (`CMAKE`,
            //     `CMAKE_GENERATOR=Ninja`, PATH prepends) via the same
            //     `inject_cmake_tooling` the blessed `soldr build`
            //     surface uses (#1257). Same opt-outs apply.
            if crate_name == "maturin" {
                if std::env::var_os("CARGO").is_none() {
                    match resolve_toolchain_binary("cargo") {
                        Ok(cargo) => {
                            // A direct (non-rustup-proxy) toolchain
                            // cargo spawns `rustc` from PATH — on the
                            // poisoned-fixture machine that's the GNU
                            // standalone, which lacks the msvc std and
                            // dies with E0463. Pin RUSTC to the
                            // sibling rustc of the resolved cargo so
                            // cargo and rustc always come from the
                            // same toolchain; fall back to the
                            // resolver when there is no sibling.
                            if std::env::var_os("RUSTC").is_none() {
                                let sibling = cargo.parent().map(|dir| {
                                    dir.join(if cfg!(windows) { "rustc.exe" } else { "rustc" })
                                });
                                match sibling.filter(|p| p.is_file()) {
                                    Some(rustc) => {
                                        command.env("RUSTC", rustc);
                                    }
                                    None => match resolve_toolchain_binary("rustc") {
                                        Ok(rustc) => {
                                            command.env("RUSTC", rustc);
                                        }
                                        Err(err) => eprintln!(
                                            "soldr warning: could not resolve \
                                             toolchain rustc for maturin: {err}"
                                        ),
                                    },
                                }
                            }
                            command.env("CARGO", &cargo);
                        }
                        Err(err) => eprintln!(
                            "soldr warning: could not resolve toolchain cargo for \
                             maturin; child falls back to PATH lookup: {err}"
                        ),
                    }
                }
                // CARGO alone is not enough: resolve_toolchain_binary's
                // last-resort probe is a PATH lookup, and on the
                // poisoned-fixture machine a GNU-host rustup resolves
                // the pinned channel to its GNU variant. Force the
                // TARGET too — same runtime MSVC-default policy the
                // cargo front door applies via CARGO_BUILD_TARGET
                // (Windows-only; explicit user env always wins). Both
                // cargo and maturin honor CARGO_BUILD_TARGET, so even
                // a wrong-host cargo emits the right-target wheel.
                if cfg!(windows) && std::env::var_os("CARGO_BUILD_TARGET").is_none() {
                    match crate::core::TargetTriple::detect() {
                        Ok(triple) => {
                            command.env("CARGO_BUILD_TARGET", triple.triple());
                        }
                        Err(err) => eprintln!(
                            "soldr warning: could not detect default target for \
                             maturin; child builds for its cargo's host: {err}"
                        ),
                    }
                }
                let paths = SoldrPaths::new()?;
                let mut prep = crate::blessed_build::BlessedPrep::default();
                crate::blessed_build::inject_cmake_tooling(&paths, &mut prep).await;
                // Mutate our own env (inherited by the child) so the
                // shim-dir PATH prepend below composes on top.
                for (k, v) in &prep.env {
                    std::env::set_var(k, v);
                }
                for dir in &prep.path_dirs {
                    prepend_to_path_env(dir);
                }
            }

            // Issue #493: when the user runs `soldr <external-tool>`,
            // install a transient PATH shim so any nested `cargo` /
            // `rustc` / `rustdoc` / `rustfmt` / `clippy-driver` spawned
            // by the tool routes back through soldr (and therefore
            // zccache and the managed toolchain home). The guard's
            // Drop removes the shim dir after the child exits.
            let _shim_guard = if shim_dir::should_install_shims() {
                match shim_dir::build_shim_dir() {
                    Ok(guard) => {
                        shim_dir::apply_to_command(&mut command, &guard.path);
                        Some(guard)
                    }
                    Err(err) => {
                        eprintln!(
                            "soldr warning: failed to build child shim dir; \
                             nested cargo/rustc calls will bypass soldr: {err}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            suppress_windows_console_window(&mut command);
            let status = command.status()?;

            std::process::exit(status.code().unwrap_or(1));
        }
    }

    Ok(())
}

fn should_use_managed_zccache_external(crate_name: &str, version: &VersionSpec) -> bool {
    crate_name == "zccache" && matches!(version, VersionSpec::Latest)
}

/// soldr#1264 follow-on: maturin provisioning ladder. `auto` (default)
/// tries the prebuilt GitHub-Releases binary and falls back to the
/// manual uv-provisioned isolated env; `binary` / `uv` force one rung.
/// See `fetch::maturin_env` for the env-var contract.
async fn fetch_maturin_with_provisioner(
    version: &VersionSpec,
) -> Result<crate::fetch::FetchResult, SoldrError> {
    use crate::fetch::maturin_env::MaturinProvisioner;

    let pinned = match version {
        VersionSpec::Exact(v) => v.clone(),
        VersionSpec::Latest => crate::fetch::MANAGED_MATURIN_VERSION.to_string(),
    };

    match MaturinProvisioner::from_env() {
        MaturinProvisioner::Binary => crate::fetch::fetch_tool("maturin", version).await,
        MaturinProvisioner::Uv => provisioned_maturin_fetch_result(&pinned).await,
        MaturinProvisioner::Auto => match crate::fetch::fetch_tool("maturin", version).await {
            Ok(result) => Ok(result),
            Err(err) => {
                eprintln!("soldr: prebuilt maturin fetch failed: {err}");
                eprintln!("soldr: falling back to the uv-provisioned maturin env...");
                provisioned_maturin_fetch_result(&pinned).await
            }
        },
    }
}

async fn provisioned_maturin_fetch_result(
    version: &str,
) -> Result<crate::fetch::FetchResult, SoldrError> {
    let paths = SoldrPaths::new()?;
    let cached = crate::fetch::maturin_env::env_is_complete(
        &crate::fetch::maturin_env::env_dir_for(&paths, version),
    );
    let binary_path = crate::fetch::maturin_env::provision_maturin_via_uv(&paths, version).await?;
    Ok(crate::fetch::FetchResult {
        binary_path,
        version: version.to_string(),
        cached,
    })
}

fn report_and_exit(error: SoldrError) -> i32 {
    eprintln!("soldr: {error}");
    1
}

fn should_self_relocate_for_invocation(raw_args: &[String]) -> bool {
    let user_args = raw_args.get(1..).unwrap_or(&[]);
    let Ok((_, args)) = extract_as_pin(user_args) else {
        return false;
    };

    let mut cache_enabled = true;
    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--no-cache" => {
                cache_enabled = false;
                idx += 1;
            }
            "--zccache" => {
                // Value lives in the next token; skip both so the
                // subcommand check below lands on `cargo` instead of
                // the value.
                idx += 2;
            }
            "--" => return false,
            arg if arg.starts_with('-') => idx += 1,
            "cargo" => {
                return cache_enabled
                    && cargo_front_door::cargo_args_are_cacheable(&args[idx + 1..])
                    && matches!(
                        zccache::rustc_wrapper_mode(),
                        zccache::RustcWrapperMode::ManagedZccache
                    );
            }
            _ => return false,
        }
    }

    false
}

/// Extract `--as <version>` or `--as=<version>` from the leading flag
/// region of the user's argv. Stops scanning at the first non-flag
/// positional (conventionally the subcommand), so a `--as` appearing
/// after `cargo` belongs to cargo and is left alone.
/// soldr#1012 PR 5 — scan `args` for `--target X` (two-arg form) or
/// `--target=X` (single-arg form). Returns the FIRST occurrence; if
/// both forms are present the single-arg form wins by virtue of
/// appearing first in a left-to-right scan (cargo behavior is
/// "last --target wins", but for prep purposes any match is enough
/// because the prep is target-keyed and cargo handles the final
/// dispatch).
fn extract_target_from_args(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(triple) = arg.strip_prefix("--target=") {
            if !triple.is_empty() {
                return Some(triple.to_string());
            }
        }
        if arg == "--target" {
            if let Some(next) = iter.next() {
                if !next.is_empty() {
                    return Some(next.clone());
                }
            }
        }
    }
    None
}

/// soldr#882: pick the cargo subcommand to dispatch for a given
/// cross-target. Only fires on Linux hosts — native macos/windows
/// host builds keep using plain `cargo build`.
///
/// Returns:
/// * `Some("xwin")` for `*-pc-windows-msvc` (unless `SOLDR_USE_LEGACY_XWIN`
///   is set in env — escape hatch for callers who want the plain
///   `cargo build` fallback)
/// * `Some("zigbuild")` for `*-apple-darwin`, `*-unknown-linux-musl`,
///   and aarch64 `*-unknown-linux-gnu` (cross from x86_64), unless
///   `SOLDR_USE_LEGACY_ZIGBUILD` is set
/// * `None` for everything else
fn pick_cross_subcommand(target_triple: &str) -> Option<&'static str> {
    if !cfg!(target_os = "linux") {
        return None;
    }

    let legacy_xwin = std::env::var_os(crate::blessed_build::USE_LEGACY_XWIN_ENV_VAR)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    let legacy_zigbuild = std::env::var_os(crate::blessed_build::USE_LEGACY_ZIGBUILD_ENV_VAR)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);

    if target_triple.ends_with("-pc-windows-msvc") {
        return if legacy_xwin { None } else { Some("xwin") };
    }
    // soldr#1081 follow-up: `*-apple-darwin` no longer routes through
    // cargo-zigbuild. The blessed-build apple-darwin arm in
    // `blessed_build.rs` now exports the COMPLETE Apple SDK to cc-rs +
    // rustc's linker, so plain `cargo build --target X` produces a
    // Mach-O binary from a Linux host without the
    // tikv-jemalloc-sys/zig-minimal-sysroot mismatch that broke the
    // release lane. `SOLDR_USE_LEGACY_ZIGBUILD=1` re-routes darwin
    // through zigbuild for diagnostic comparison.
    if target_triple.ends_with("-apple-darwin") {
        return if legacy_zigbuild {
            Some("zigbuild")
        } else {
            None
        };
    }
    if target_triple.ends_with("-unknown-linux-musl") {
        return if legacy_zigbuild {
            None
        } else {
            Some("zigbuild")
        };
    }
    // Cross from x86_64 host to aarch64 linux — needs zigbuild for
    // the bundled libc.
    if target_triple == "aarch64-unknown-linux-gnu" && cfg!(target_arch = "x86_64") {
        return if legacy_zigbuild {
            None
        } else {
            Some("zigbuild")
        };
    }
    None
}

/// soldr#882: rewrite the args vector for the picked cargo subcommand.
///
/// For `zigbuild`: cargo-zigbuild IS the build verb — replace the
/// leading `build` with `zigbuild`. So `["build", "--target", X, ...]`
/// becomes `["zigbuild", "--target", X, ...]`.
///
/// For `xwin`: cargo-xwin uses `xwin build ...` as a subcommand
/// pair — prepend `xwin` keeping the `build` verb. So
/// `["build", "--target", X, ...]` becomes
/// `["xwin", "build", "--target", X, ...]`.
fn rewrite_build_args_for_subcommand(mut args: Vec<String>, subcmd: &str) -> Vec<String> {
    match subcmd {
        "zigbuild" => {
            if let Some(first) = args.first_mut() {
                if first == "build" {
                    *first = "zigbuild".to_string();
                }
            }
            args
        }
        "xwin" => {
            args.insert(0, "xwin".to_string());
            args
        }
        _ => args,
    }
}

fn insert_cargo_config_args(mut args: Vec<String>, cargo_config_args: &[String]) -> Vec<String> {
    if cargo_config_args.is_empty() {
        return args;
    }

    let insert_at = if args.first().is_some_and(|arg| arg == "xwin")
        && args.get(1).is_some_and(|arg| arg == "build")
    {
        2
    } else if args.is_empty() {
        0
    } else {
        1
    };
    args.splice(insert_at..insert_at, cargo_config_args.iter().cloned());
    args
}

/// soldr#1079 — bridge between the cargo dispatcher and the MSVC
/// host-discovery module. Resolves the target triple (explicit
/// `--target` first, then `TargetTriple::detect()` for the implicit
/// native default) and asks [`crate::msvc_host`] to inject the
/// vcvars-equivalent env vars when relevant.
///
/// All branches are silent on success / no-op. Discovery errors are
/// printed as a single warning line so the user gets a hint about
/// `SOLDR_MSVC_DISCOVERY=off` if the auto-probe trips on an unusual
/// install — the underlying cargo invocation still runs and emits
/// its own (better) error if it actually needs the env.
fn ensure_msvc_host_env_for_native(args: &[String]) {
    if !cfg!(target_os = "windows") {
        return;
    }
    let target = match extract_target_from_args(args) {
        Some(t) => t,
        None => match crate::core::TargetTriple::detect() {
            Ok(t) => t.triple(),
            Err(_) => return,
        },
    };
    match crate::msvc_host::ensure_msvc_env_for_native(&target) {
        Ok(true) => {
            tracing::debug!(
                target: "soldr::msvc_host",
                target_triple = %target,
                "injected host MSVC env (LIB/INCLUDE/PATH/LIBPATH) for native build"
            );
        }
        Ok(false) => {
            // Skipped — non-windows, non-msvc, opt-out, or already-set.
            // Nothing to log; this is the steady-state branch in dev
            // command prompts.
        }
        Err(err) => {
            eprintln!(
                "soldr: MSVC host discovery failed for {target}: {err}\n\
                 soldr: cargo will still run; set SOLDR_MSVC_DISCOVERY=off to silence this probe."
            );
        }
    }
}

/// soldr#1012 PR 5 — prepend `dir` to the current process's `PATH`
/// env var. Idempotent in the sense that if `dir` is already first
/// on PATH, the value is unchanged (PATH stays clean).
fn prepend_to_path_env(dir: &std::path::Path) {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut existing: Vec<std::path::PathBuf> = std::env::split_paths(&current).collect();
    if existing.first().is_some_and(|p| p == dir) {
        return;
    }
    existing.insert(0, dir.to_path_buf());
    if let Ok(joined) = std::env::join_paths(existing) {
        std::env::set_var("PATH", joined);
    }
}

fn extract_as_pin(args: &[String]) -> Result<(Option<String>, Vec<String>), SoldrError> {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut version: Option<String> = None;
    let mut before_subcommand = true;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !before_subcommand {
            out.push(arg.clone());
            continue;
        }
        if arg == "--as" {
            let value = iter.next().ok_or_else(|| {
                SoldrError::Other("--as requires a version argument, e.g. --as 0.5.2".into())
            })?;
            if version.is_some() {
                return Err(SoldrError::Other("--as specified more than once".into()));
            }
            if value.is_empty() {
                return Err(SoldrError::Other(
                    "--as version argument must not be empty".into(),
                ));
            }
            version = Some(value.clone());
            continue;
        }
        if let Some(value) = arg.strip_prefix("--as=") {
            if version.is_some() {
                return Err(SoldrError::Other("--as specified more than once".into()));
            }
            if value.is_empty() {
                return Err(SoldrError::Other(
                    "--as= requires a version, e.g. --as=0.5.2".into(),
                ));
            }
            version = Some(value.to_string());
            continue;
        }
        if arg == "--" {
            before_subcommand = false;
            out.push(arg.clone());
            continue;
        }
        if arg.starts_with('-') {
            out.push(arg.clone());
            continue;
        }
        before_subcommand = false;
        out.push(arg.clone());
    }
    Ok((version, out))
}

/// True when the requested version is different from this binary's. A match
/// short-circuits the trampoline so the current in-process soldr handles it.
fn should_trampoline(requested: &str) -> bool {
    let current = env!("CARGO_PKG_VERSION");
    normalize_version(requested) != normalize_version(current)
}

fn normalize_version(v: &str) -> String {
    v.trim().trim_start_matches('v').to_string()
}

async fn run_trampoline(version: &str, args: &[String]) -> Result<i32, SoldrError> {
    if let Ok(prior) = std::env::var(SOLDR_TRAMPOLINING_ENV_VAR) {
        return Err(SoldrError::Other(format!(
            "refusing to trampoline again: this process was already reached via `--as` from soldr {prior}. Drop the inner --as flag."
        )));
    }

    eprintln!("soldr: trampolining to soldr@{version}...");
    let result =
        crate::fetch::fetch_tool("soldr", &VersionSpec::Exact(normalize_version(version))).await?;

    if result.cached {
        eprintln!(
            "soldr: using cached soldr v{} at {}",
            result.version,
            result.binary_path.display()
        );
    } else {
        eprintln!(
            "soldr: downloaded soldr v{} to {}",
            result.version,
            result.binary_path.display()
        );
    }

    let mut command = std::process::Command::new(&result.binary_path);
    command
        .args(args)
        .env(SOLDR_TRAMPOLINING_ENV_VAR, env!("CARGO_PKG_VERSION"));
    suppress_windows_console_window(&mut command);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let err = command.exec();
        Err(SoldrError::Other(format!(
            "failed to exec soldr v{} at {}: {err}",
            result.version,
            result.binary_path.display()
        )))
    }

    #[cfg(not(unix))]
    {
        let status = command.status().map_err(|e| {
            SoldrError::Other(format!(
                "failed to exec soldr v{} at {}: {e}",
                result.version,
                result.binary_path.display()
            ))
        })?;

        Ok(status.code().unwrap_or(1))
    }
}

fn render_builds(
    result: Result<Vec<crate::daemon::protocol::BuildRecord>, crate::daemon::client::ClientError>,
    json: bool,
) -> Result<(), SoldrError> {
    use crate::daemon::client::ClientError;
    match result {
        Ok(rows) => {
            if json {
                let payload = serde_json::json!({
                    "builds": rows.iter().map(|r| serde_json::json!({
                        "session_id": r.session_id,
                        "repo_root": r.repo_root,
                        "started_at_ms": r.started_at_ms,
                        "ended_at_ms": r.ended_at_ms,
                        "exit_code": r.exit_code,
                        "total_wall_ms": r.total_wall_ms,
                        "crate_count": r.crate_count,
                        "slowest_crate_us": r.slowest_crate_us,
                        "slowest_crate_name": r.slowest_crate_name,
                    })).collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string(&payload).unwrap_or_default());
            } else if rows.is_empty() {
                println!("(no recorded builds)");
            } else {
                for r in rows {
                    let wall = r
                        .total_wall_ms
                        .map(|m| format!("{m}ms"))
                        .unwrap_or_else(|| "running".into());
                    let exit = r
                        .exit_code
                        .map(|c| format!("exit={c}"))
                        .unwrap_or_else(|| "exit=?".into());
                    let slowest = r.slowest_crate_name.as_deref().unwrap_or("(none)");
                    println!(
                        "session_id={} repo={} wall={} {} crates={} slowest={}",
                        r.session_id, r.repo_root, wall, exit, r.crate_count, slowest
                    );
                }
            }
            Ok(())
        }
        Err(ClientError::NotRunning) => {
            if json {
                println!("{}", serde_json::json!({"running": false, "builds": []}));
            } else {
                println!("soldr-daemon: not running");
            }
            Ok(())
        }
        Err(e) => Err(SoldrError::Other(format!("daemon builds failed: {e:?}"))),
    }
}

async fn run_daemon_command(command: DaemonSubcommand) -> Result<(), SoldrError> {
    use crate::daemon::client;
    use crate::daemon::lifecycle::{is_live, try_spawn_detached};
    use crate::daemon::server::{run_async, server_sock_path, ServerOptions};
    use core::SoldrPaths;
    use std::time::Duration;

    let paths = SoldrPaths::new()?;
    let sock = server_sock_path(&paths);

    match command {
        DaemonSubcommand::Start {
            foreground,
            idle_timeout,
        } => {
            if foreground {
                let opts = ServerOptions {
                    idle_timeout: if idle_timeout == 0 {
                        Duration::from_secs(u64::MAX / 2)
                    } else {
                        Duration::from_secs(idle_timeout)
                    },
                };
                // We're already inside main()'s #[tokio::main] runtime
                // (run_with_args is async, called from main's runtime).
                // Calling the sync `server::run` here would have it build
                // ANOTHER multi-thread runtime + block_on, which panics
                // with "Cannot start a runtime from within a runtime"
                // (the symptom on the CI perf-matrix run in soldr#985
                // diagnosis). Reach run_async directly instead — it
                // does the same work without building a runtime.
                run_async(opts)
                    .await
                    .map_err(|e| SoldrError::Other(format!("soldr-daemon failed: {e:?}")))?;
                Ok(())
            } else {
                if is_live(&paths).is_some() {
                    println!("soldr-daemon already running");
                    return Ok(());
                }
                try_spawn_detached().map_err(|e| {
                    SoldrError::Other(format!("failed to spawn soldr-daemon: {e:?}"))
                })?;
                println!("soldr-daemon: spawn requested");
                Ok(())
            }
        }
        DaemonSubcommand::Stop => match client::shutdown(&sock) {
            Ok(()) => {
                println!("soldr-daemon: shutdown requested");
                Ok(())
            }
            Err(client::ClientError::NotRunning) => {
                println!("soldr-daemon: not running");
                Ok(())
            }
            Err(e) => Err(SoldrError::Other(format!("daemon stop failed: {e:?}"))),
        },
        DaemonSubcommand::Status { json } => match client::status(&sock) {
            Ok(info) => {
                // Cook-index aggregate stats (issue #576). Older
                // daemons would emit `cook_stats: None` — render as
                // zero so the surface is stable.
                let cook = info.cook_stats_or_zero();
                if json {
                    let payload = serde_json::json!({
                        "running": true,
                        "version": info.version,
                        "pid": info.pid,
                        "uptime_secs": info.uptime_secs,
                        "request_count": info.request_count,
                        "linked_zccache": info.linked_zccache,
                        "cook": {
                            "entries": cook.entries,
                            "total_bytes": cook.total_bytes,
                            "hits_this_session": cook.hits_this_session,
                        },
                    });
                    println!("{}", serde_json::to_string(&payload).unwrap_or_default());
                } else {
                    println!(
                        "soldr-daemon: pid={} uptime={}s requests={} version={}",
                        info.pid, info.uptime_secs, info.request_count, info.version
                    );
                    println!(
                        "  cook: entries={} total_bytes={} hits_this_session={}",
                        cook.entries, cook.total_bytes, cook.hits_this_session
                    );
                }
                Ok(())
            }
            Err(client::ClientError::NotRunning) => {
                if json {
                    println!("{}", serde_json::json!({"running": false}));
                } else {
                    println!("soldr-daemon: not running");
                }
                Ok(())
            }
            Err(e) => Err(SoldrError::Other(format!("daemon status failed: {e:?}"))),
        },
        DaemonSubcommand::InstallServiceDef {
            daemon_binary,
            json,
        } => {
            let installed = match daemon_binary {
                Some(path) => crate::daemon::service_definition::install_service_definition(&path),
                None => crate::daemon::service_definition::install_default_service_definition(),
            }
            .map_err(|e| SoldrError::Other(format!("failed to install servicedef: {e}")))?;
            if json {
                let payload = serde_json::json!({
                    "path": installed.path,
                    "service_name": installed.definition.service_name,
                    "binary_path": installed.definition.binary_path,
                    "per_version_binary_dir": installed.definition.per_version_binary_dir,
                    "min_version": installed.definition.min_version,
                    "version_allow_list": installed.definition.version_allow_list,
                    "isolation": "SHARED_BROKER",
                    "deferred": crate::daemon::service_definition::SOLDR_DAEMON_SERVICE_DEF_DEFERRED,
                });
                println!("{}", serde_json::to_string(&payload).unwrap_or_default());
            } else {
                println!(
                    "soldr-daemon servicedef installed at {}",
                    installed.path.display()
                );
            }
            Ok(())
        }
        DaemonSubcommand::Builds { command } => match command {
            DaemonBuildsSubcommand::List {
                limit,
                since_ms,
                json,
            } => render_builds(client::list_builds(&sock, limit, since_ms), json),
            DaemonBuildsSubcommand::Slow {
                threshold_ms,
                limit,
                json,
            } => render_builds(client::list_slow_builds(&sock, threshold_ms, limit), json),
        },
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
