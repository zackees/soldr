// Mirror lib.rs: the bin tree compiles the same `fetch::*`,
// `cache_lib::*`, `core::*` modules independently of the lib tree.
// Items declared for external (lib) callers are dead from the bin's
// perspective and would trip `-D warnings` on CI. lib.rs has the same
// allow.
#![allow(dead_code, unused_imports)]

use clap::Parser;

mod binaries;
mod bootstrap;
mod cache;
mod cache_lib;
mod cargo_diagnostics;
mod cargo_front_door;
mod cli_args;
mod cook;
mod core;
mod daemon;
mod defender_probe;
mod doctor;
mod fetch;
mod fuzzy_match;
mod gc;
mod linker;
mod native_cc;
mod optimize;
mod optimize_detect;
mod optimize_windows;
mod rust_plan;
mod save_load;
mod self_relocate;
mod shim_dir;
mod startup_profile;
mod toolchain;
mod toolchain_doctor;
mod toolchain_ensure;
mod toolchain_link;
mod trampoline;
mod trampoline_workspace;
mod wrapper;
mod wrapper_target;
mod zccache;
mod zccache_lifecycle;

// Per-test watchdog (`timed_test!` macro + `run_with_watchdog`).
// Declared (without a cfg gate) so unit tests under `src/` that use
// `crate::timed_test!` see the matching `test_util` module — `$crate`
// in the macro resolves to the bin crate for unit tests. The module
// is tiny and never invoked outside `#[test]` paths, so the
// production-binary cost is negligible. Mirrors `lib.rs`.
mod test_util;

use cli_args::{
    CacheSubcommand, Cli, Commands, DaemonBuildsSubcommand, DaemonSubcommand,
    DefenderExclusionsSubcommand, GcCargoArgs, GcListKind, GcSubcommand, GcSweepArgs,
    ToolchainSubcommand, TrimProfileArg, ZccacheSourceArg, SOLDR_BUILTIN_VERBS,
};

use crate::core::{suppress_windows_console_window, SoldrError};
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

    match cli.command {
        Commands::Cargo { args } => {
            std::process::exit(
                cargo_front_door::run_cargo_front_door(&args, cache_enabled, zccache_source)
                    .await?,
            );
        }
        Commands::Cook { args } => {
            std::process::exit(cook::run_cook(&args, cache_enabled, zccache_source).await?);
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
            run_daemon_command(command)?;
        }
        Commands::External(args) => {
            if args.is_empty() {
                eprintln!("usage: soldr <tool>[@version] [args...]");
                std::process::exit(1);
            }

            let (crate_name, version) = parse_tool_spec(&args[0]);
            let tool_args = &args[1..];

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
            let result = crate::fetch::fetch_tool(&crate_name, &version).await?;

            if result.cached {
                eprintln!("soldr: using cached {crate_name} v{}", result.version);
            } else {
                eprintln!("soldr: downloaded {crate_name} v{}", result.version);
            }

            let mut command = std::process::Command::new(&result.binary_path);
            command.args(tool_args);

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

fn run_daemon_command(command: DaemonSubcommand) -> Result<(), SoldrError> {
    use crate::daemon::client;
    use crate::daemon::lifecycle::{is_live, try_spawn_detached};
    use crate::daemon::server::{run as run_server, server_sock_path, ServerOptions};
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
                run_server(opts)
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
                if json {
                    let payload = serde_json::json!({
                        "running": true,
                        "version": info.version,
                        "pid": info.pid,
                        "uptime_secs": info.uptime_secs,
                        "request_count": info.request_count,
                        "linked_zccache": info.linked_zccache,
                    });
                    println!("{}", serde_json::to_string(&payload).unwrap_or_default());
                } else {
                    println!(
                        "soldr-daemon: pid={} uptime={}s requests={} version={}",
                        info.pid, info.uptime_secs, info.request_count, info.version
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
