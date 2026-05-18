use clap::{Parser, Subcommand};
use serde::Serialize;
use sha2::{Digest, Sha256};
use soldr_core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use soldr_fetch::VersionSpec;
use std::collections::BTreeSet;

mod cache;
mod doctor;
mod gc;
mod linker;
mod rust_plan;
mod self_relocate;
mod toolchain;

const TEST_CARGO_BIN_ENV_VAR: &str = "SOLDR_TEST_CARGO_BIN";
const TEST_RUSTC_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTC_BIN";
const TEST_RUSTUP_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTUP_BIN";
const TEST_ZCCACHE_BIN_ENV_VAR: &str = "SOLDR_TEST_ZCCACHE_BIN";
const TEST_FREE_DISK_BYTES_ENV_VAR: &str = "SOLDR_TEST_FREE_DISK_BYTES";
/// Overrides the default `nightly` toolchain used by `soldr gc cargo`
/// to invoke cargo's unstable `-Zgc clean gc`. The `--toolchain` flag
/// on `gc cargo` / `gc sweep` always takes precedence.
pub(crate) const SOLDR_GC_CARGO_TOOLCHAIN_ENV_VAR: &str = "SOLDR_GC_CARGO_TOOLCHAIN";
pub(crate) const JSON_SCHEMA_VERSION: u32 = 1;
const LOW_DISK_WARNING_THRESHOLD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const RUSTC_WRAPPER_OVERRIDE_ENV_VAR: &str = "SOLDR_RUSTC_WRAPPER";
const CARGO_PROFILE_DEV_DEBUG_ENV_VAR: &str = "CARGO_PROFILE_DEV_DEBUG";
const CARGO_PROFILE_TEST_DEBUG_ENV_VAR: &str = "CARGO_PROFILE_TEST_DEBUG";
/// Picks the linker injected for `soldr cargo ...` builds. See `linker` module
/// and `docs/API.md` for the supported values (`default | ld | mold | rust-lld
/// | fast`).
const LINKER_ENV_VAR: &str = "SOLDR_LINKER";
const REAL_TOOLCHAIN_BINARY_ENV_PREFIX: &str = "SOLDR_REAL_";
pub(crate) const TARGET_CACHE_MODE_ENV_VAR: &str = "SOLDR_TARGET_CACHE_MODE";
pub(crate) const TARGET_CACHE_BUNDLE_DIR_ENV_VAR: &str = "SOLDR_TARGET_CACHE_BUNDLE_DIR";
pub(crate) const TARGET_CACHE_BACKEND_ENV_VAR: &str = "SOLDR_TARGET_CACHE_BACKEND";
/// Selects which thin-slice pruning policy `soldr cargo` ships to zccache. See
/// `docs/THIN_TARGET_CACHE_PRUNING.md` for the rationale and rollout plan.
/// Values: `thin-v1` (legacy, default — keeps `.rlib`/`.rmeta`/proc-macro
/// outputs as a safety net) and `thin-v2` (fingerprint-aware aggressive prune;
/// drops library bytes and lets zccache's compilation cache repopulate them).
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

#[derive(Parser)]
#[command(name = "soldr", version, about = "Instant tools. Instant builds.")]
struct Cli {
    /// Disable soldr's compilation cache for this invocation
    #[arg(long)]
    no_cache: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run Cargo through soldr's front door
    Cargo {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run rustc from the active toolchain
    Rustc {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run rustfmt from the active toolchain
    Rustfmt {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run clippy-driver from the active toolchain
    #[command(name = "clippy-driver")]
    ClippyDriver {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run rustdoc from the active toolchain
    Rustdoc {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run rust-gdb from the active toolchain
    #[command(name = "rust-gdb")]
    RustGdb {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run rust-lldb from the active toolchain
    #[command(name = "rust-lldb")]
    RustLldb {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run rust-analyzer from the active toolchain
    #[command(name = "rust-analyzer")]
    RustAnalyzer {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show cache status and tool info
    Status {
        /// Emit the stable machine-facing JSON form for this command
        #[arg(long)]
        json: bool,
    },
    /// Clear the managed zccache build cache
    Clean,
    /// Purge all soldr-managed cache artifacts
    Purge,
    /// Show or set configuration
    Config,
    /// Inspect the compilation cache
    Cache {
        /// Emit the stable machine-facing JSON form for this command
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<CacheSubcommand>,
    },
    /// Show version
    Version {
        /// Emit the stable machine-facing JSON form for this command
        #[arg(long)]
        json: bool,
    },
    /// Review reclaimable Cargo `target/` directories tracked by
    /// the soldr registry (`~/.soldr/state.redb`).
    ///
    /// Aliases: `purge-targets` (matches issue #234's `soldr --purge`
    /// wording).
    #[command(alias = "purge-targets")]
    Gc {
        /// Deprecated: `soldr gc` is already a non-destructive summary.
        #[arg(long, hide = true)]
        dry_run: bool,
        /// Deprecated: use `soldr gc purge --all`.
        #[arg(long, hide = true)]
        all: bool,
        /// Minimum age before a `target/` is included in the summary
        /// (e.g. `10d`, `4w`).
        #[arg(long, default_value = "10d", value_name = "DURATION")]
        older_than: String,
        /// Minimum on-disk size before a `target/` is included in the
        /// summary (e.g. `256M`, `1GB`).
        #[arg(long, default_value = "256M", value_name = "SIZE")]
        larger_than: String,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<GcSubcommand>,
    },
    /// Drop-in passthrough to the system `rustup` binary.
    ///
    /// When the first non-flag positional argument is `target` or
    /// `component` and `rust-toolchain.toml` declares a `channel`,
    /// soldr automatically inserts `--toolchain <channel>` after the
    /// subcommand (unless the user already passed `--toolchain`).
    /// Every other invocation is forwarded verbatim.
    Rustup {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Read `rust-toolchain.toml` and install the declared channel /
    /// components / targets via `rustup`.
    Toolchain {
        #[command(subcommand)]
        subcommand: ToolchainSubcommand,
    },
    /// Diagnose drift between `rust-toolchain.toml` and the
    /// currently installed rustup state. Read-only — never mutates
    /// rustup. Exit code is `1` when drift is detected, `0` otherwise.
    Doctor {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Anything else is a tool to fetch and run
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Subcommand)]
enum ToolchainSubcommand {
    /// Install the channel declared in `rust-toolchain.toml`. No-op
    /// (exit 0 with a note) when the manifest is missing or omits
    /// `channel`.
    Install,
    /// Install the channel and every declared component / target from
    /// `rust-toolchain.toml`. Stops at the first nonzero rustup exit.
    Prepare,
}

#[derive(Subcommand)]
enum GcSubcommand {
    /// Delete eligible Cargo `target/` directories.
    Purge {
        /// Delete every eligible candidate without prompting.
        #[arg(long)]
        all: bool,
        /// Minimum age before a `target/` is considered stale
        /// (e.g. `10d`, `4w`).
        #[arg(long, default_value = "10d", value_name = "DURATION")]
        older_than: String,
        /// Minimum on-disk size before a `target/` is considered for
        /// reclamation (e.g. `256M`, `1GB`).
        #[arg(long, default_value = "256M", value_name = "SIZE")]
        larger_than: String,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// List every `target/` directory currently tracked in the soldr
    /// registry, without applying any age or size thresholds.
    List {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Run cargo's native `clean gc` against `$CARGO_HOME`. Requires
    /// a nightly toolchain because the command lives behind the
    /// unstable `-Zgc` flag.
    Cargo(Box<GcCargoArgs>),
    /// Read-only enumeration of every cache directory soldr knows
    /// about. Does not delete anything.
    Locations {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Orchestrator that runs `gc locations`, then conservative cargo
    /// `clean gc`, then the soldr target purge — and, with
    /// `--aggressive`, a second cargo GC pass with tighter ages.
    Sweep(Box<GcSweepArgs>),
}

#[derive(clap::Args)]
pub(crate) struct GcCargoArgs {
    /// Report the plan and exit without invoking cargo.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Override the nightly toolchain. Defaults to
    /// `$SOLDR_GC_CARGO_TOOLCHAIN` if set, else `nightly`.
    #[arg(long, value_name = "TOOLCHAIN")]
    pub(crate) toolchain: Option<String>,
    /// Forwarded directly to cargo `--max-src-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_src_age: Option<String>,
    /// Forwarded directly to cargo `--max-crate-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_crate_age: Option<String>,
    /// Forwarded directly to cargo `--max-index-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_index_age: Option<String>,
    /// Forwarded directly to cargo `--max-git-co-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_git_co_age: Option<String>,
    /// Forwarded directly to cargo `--max-git-db-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_git_db_age: Option<String>,
    /// Forwarded directly to cargo `--max-download-age`.
    #[arg(long, value_name = "DURATION")]
    pub(crate) max_download_age: Option<String>,
    /// Forwarded directly to cargo `--max-src-size`.
    #[arg(long, value_name = "SIZE")]
    pub(crate) max_src_size: Option<String>,
    /// Forwarded directly to cargo `--max-crate-size`.
    #[arg(long, value_name = "SIZE")]
    pub(crate) max_crate_size: Option<String>,
    /// Forwarded directly to cargo `--max-git-size`.
    #[arg(long, value_name = "SIZE")]
    pub(crate) max_git_size: Option<String>,
    /// Forwarded directly to cargo `--max-download-size`.
    #[arg(long, value_name = "SIZE")]
    pub(crate) max_download_size: Option<String>,
    /// Emit the stable machine-facing JSON form for this command.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(clap::Args)]
pub(crate) struct GcSweepArgs {
    /// Delete every eligible target/ candidate without prompting (used
    /// when the orchestrator runs the soldr target purge stage).
    #[arg(long)]
    pub(crate) all: bool,
    /// Plan and report without deleting anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Run cargo's `clean gc`. Default is on; pass `--no-cargo-gc` to
    /// skip cargo entirely. `--cargo-gc` is accepted but is the
    /// default.
    #[arg(long, conflicts_with = "no_cargo_gc")]
    pub(crate) cargo_gc: bool,
    /// Skip cargo's `clean gc` (e.g. on CI runners with no nightly).
    #[arg(long, conflicts_with = "cargo_gc")]
    pub(crate) no_cargo_gc: bool,
    /// After the standard pipeline, run cargo's `clean gc` again with
    /// tighter ages
    /// (`--max-src-age=7days --max-crate-age=14days --max-git-co-age=7days`).
    /// Floor: each value is clamped to `auto_gc.min_age_secs` before
    /// being forwarded.
    #[arg(long)]
    pub(crate) aggressive: bool,
    /// Emit the stable machine-facing JSON form for this command.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Subcommand)]
enum CacheSubcommand {
    /// Roll up the most recent compile-cache session into an
    /// AI-readable diagnosis document. Reads
    /// `<zccache_dir>/logs/last-session-stats.json` (written by soldr
    /// at session-end) and, when available, calls `zccache analyze` for
    /// per-tool/per-extension breakdown over the per-session journal.
    Report {
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
    /// Prune stale per-prefix build artifacts from a cargo `target/`
    /// directory, keeping only the newest entry per
    /// `(parent_dir, prefix)` bucket inside
    /// `target/<profile>/{deps, .fingerprint, incremental, build}/`.
    ///
    /// Defaults to a dry run for safety. Pass `--force` (or
    /// `--no-dry-run`) to actually delete entries.
    #[command(name = "prune-target")]
    PruneTarget {
        /// Path to the cargo `target/` directory to prune.
        path: std::path::PathBuf,
        /// Explicit dry-run mode (this is the default). Accepted for
        /// scriptability; mutually compatible with the default.
        #[arg(long, conflicts_with_all = ["force", "no_dry_run"])]
        dry_run: bool,
        /// Negate the dry-run default and actually delete entries.
        /// Equivalent to `--force`.
        #[arg(long = "no-dry-run", conflicts_with = "dry_run")]
        no_dry_run: bool,
        /// Actually delete entries. Equivalent to `--no-dry-run`.
        #[arg(long, conflicts_with = "dry_run")]
        force: bool,
        /// Emit the stable machine-facing JSON form for this command.
        #[arg(long)]
        json: bool,
    },
}

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

    if raw_args.len() > 1 && is_wrapper_invocation(&raw_args[1]) {
        if let Some(version) = soldr_as_env_pin() {
            if should_trampoline(&version) {
                std::process::exit(
                    run_trampoline(&version, &raw_args[1..])
                        .await
                        .unwrap_or_else(report_and_exit),
                );
            }
        }
        std::process::exit(run_rustc_wrapper(&raw_args).unwrap_or_else(report_and_exit));
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

    match cli.command {
        Commands::Cargo { args } => {
            std::process::exit(run_cargo_front_door(&args, cache_enabled).await?);
        }
        Commands::Rustc { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough("rustc", &args)?);
        }
        Commands::Rustfmt { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough("rustfmt", &args)?);
        }
        Commands::ClippyDriver { args } => {
            std::process::exit(toolchain::run_toolchain_passthrough("clippy-driver", &args)?);
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
            std::process::exit(toolchain::run_toolchain_passthrough("rust-analyzer", &args)?);
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
        },
        Commands::Doctor { json } => {
            std::process::exit(doctor::run_doctor(json)?);
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
            Some(CacheSubcommand::PruneTarget {
                path,
                dry_run,
                no_dry_run,
                force,
                json: prune_json,
            }) => {
                let effective_dry_run = !(force || no_dry_run);
                // Either flag pair maps onto the same boolean; `dry_run`
                // is the documented default so we accept it explicitly.
                let _ = dry_run;
                cache::run_cache_prune_target_command(
                    path,
                    effective_dry_run,
                    prune_json || json,
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
                }) => gc::GcInvocation {
                    mode: gc::GcMode::Purge { all },
                    older_than,
                    larger_than,
                    json,
                },
                Some(GcSubcommand::List { json }) => {
                    gc::run_gc_list_command(json)?;
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
        Commands::External(args) => {
            if args.is_empty() {
                eprintln!("usage: soldr <tool>[@version] [args...]");
                std::process::exit(1);
            }

            let (crate_name, version) = parse_tool_spec(&args[0]);
            let tool_args = &args[1..];

            eprintln!("soldr: fetching {crate_name}...");
            let result = soldr_fetch::fetch_tool(&crate_name, &version).await?;

            if result.cached {
                eprintln!("soldr: using cached {crate_name} v{}", result.version);
            } else {
                eprintln!("soldr: downloaded {crate_name} v{}", result.version);
            }

            let mut command = std::process::Command::new(&result.binary_path);
            command.args(tool_args);
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
            "--" => return false,
            arg if arg.starts_with('-') => idx += 1,
            "cargo" => {
                return cache_enabled
                    && cargo_args_are_cacheable(&args[idx + 1..])
                    && matches!(rustc_wrapper_mode(), RustcWrapperMode::ManagedZccache);
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
        soldr_fetch::fetch_tool("soldr", &VersionSpec::Exact(normalize_version(version))).await?;

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

/// Known toolchain binaries that cargo may invoke through RUSTC_WRAPPER
/// or RUSTC_WORKSPACE_WRAPPER. When soldr is set as a wrapper, cargo
/// passes: `soldr <toolchain-binary> <rustc-args...>`
const WRAPPER_PASSTHROUGH_TOOLS: &[&str] = &["rustc", "clippy-driver"];

fn is_wrapper_invocation(arg: &str) -> bool {
    let stem = std::path::Path::new(arg)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(arg);

    WRAPPER_PASSTHROUGH_TOOLS.contains(&stem)
}

fn run_rustc_wrapper(raw_args: &[String]) -> Result<i32, SoldrError> {
    let tool_arg = raw_args
        .get(1)
        .ok_or_else(|| SoldrError::Other("missing tool path in wrapper mode".into()))?;

    let tool_stem = std::path::Path::new(tool_arg.as_str())
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(tool_arg);

    // Per-build target/ tracking for `soldr gc`. Best-effort: if we
    // can't resolve a workspace target dir cheaply, or the redb
    // upsert fails for any reason, skip silently — never fail a build.
    if tool_stem == "rustc" {
        record_target_dir_in_registry(&raw_args[2..]);
    }

    // When the source argument is "-" (stdin), rustc reads the source from
    // the process's stdin. If we pass this invocation to zccache as-is,
    // zccache reads stdin to hash the source content, exhausting the pipe
    // before rustc is spawned. Rustc then receives an empty stdin, compiles
    // nothing, and exits 0 — masking any real compile error (e.g. E0554 from
    // build-script feature probes like rustix 0.37's `can_compile()`).
    //
    // Fix: spill stdin to a temp file so both zccache and rustc see a real
    // path. The temp file is created in the system temp directory and removed
    // after the child exits. This keeps zccache in the loop (it can hash the
    // file normally) while preserving the correct exit code.
    let stdin_tempfile = if raw_args[2..].iter().any(|a| a == "-") {
        Some(spill_stdin_to_tempfile()?)
    } else {
        None
    };

    // Build the effective arg list, replacing "-" with the temp file path.
    let effective_args: std::borrow::Cow<[String]> = if let Some(ref tmp) = stdin_tempfile {
        let tmp_str = tmp.path().to_string_lossy().into_owned();
        let replaced: Vec<String> = raw_args
            .iter()
            .cloned()
            .map(|a| if a == "-" { tmp_str.clone() } else { a })
            .collect();
        std::borrow::Cow::Owned(replaced)
    } else {
        std::borrow::Cow::Borrowed(raw_args)
    };

    // Only route through zccache for actual rustc invocations, not
    // clippy-driver or other workspace wrappers.
    if tool_stem == "rustc" && soldr_cache::cache_enabled_in_current_process() {
        if let Some(zccache) = zccache_binary_override() {
            return run_wrapper_through_zccache(&effective_args, &zccache);
        }
    }

    // Resolve the tool binary. If it's already a full path, use it
    // directly. Otherwise resolve via rustup.
    let tool_path: std::path::PathBuf = if std::path::Path::new(tool_arg.as_str()).is_absolute() {
        tool_arg.into()
    } else {
        resolve_toolchain_binary(tool_stem)?
    };

    let mut command = std::process::Command::new(tool_path);
    command.args(&effective_args[2..]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = command.status()?;

    Ok(status.code().unwrap_or(1))
}

/// Read all of stdin into a named temporary file and return the file.
///
/// The file has a `.rs` extension so rustc accepts it without flags, and
/// lives in the system temp directory. It is deleted when the returned
/// `NamedTempFile` value is dropped (i.e. after the child process exits).
fn spill_stdin_to_tempfile() -> Result<tempfile::NamedTempFile, SoldrError> {
    use std::io::{Read, Write as _};
    let mut tmp = tempfile::Builder::new()
        .prefix("soldr-stdin-")
        .suffix(".rs")
        .tempfile()
        .map_err(|e| SoldrError::Other(format!("failed to create stdin temp file: {e}")))?;
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .map_err(|e| SoldrError::Other(format!("failed to read stdin: {e}")))?;
    tmp.write_all(&buf)
        .map_err(|e| SoldrError::Other(format!("failed to write stdin temp file: {e}")))?;
    Ok(tmp)
}



fn run_wrapper_through_zccache(
    raw_args: &[String],
    zccache: &std::path::Path,
) -> Result<i32, SoldrError> {
    let mut command = std::process::Command::new(zccache);
    command.args(&raw_args[1..]);
    suppress_windows_console_window(&mut command);

    // Cargo's jobserver lives on numbered file descriptors that it inherits
    // into the RUSTC_WRAPPER, advertised via CARGO_MAKEFLAGS. On Unix,
    // exec'ing into zccache replaces the wrapper process in-place so those
    // FDs flow straight through to the inner rustc — rustc otherwise emits
    // "failed to connect to jobserver from environment variable
    // CARGO_MAKEFLAGS=...: cannot open file descriptor N" because spawning
    // a Rust child closes any FDs not explicitly inherited.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // TODO(#265): once exec() replaces this process there is nowhere to
        // observe zccache's stderr or retry on "unknown session:". The
        // Windows branch below performs that defensive retry. A Unix port
        // would need to spawn-with-piped-stderr instead of exec, while still
        // forwarding the cargo jobserver FDs (see issue #265 for context).
        let err = command.exec();
        Err(SoldrError::Other(format!(
            "failed to exec zccache at {}: {err}",
            zccache.display()
        )))
    }

    #[cfg(not(unix))]
    {
        run_wrapper_through_zccache_windows(raw_args, zccache)
    }
}

/// Windows-only wrapper invocation: spawn zccache with its stderr piped so we
/// can tee it to our own stderr live AND scan it after the process exits.
///
/// If zccache returns a non-zero exit and its stderr contains the literal
/// substring `unknown session:` (issue #265), the managed zccache daemon was
/// killed mid-build by something outside soldr's control (e.g. zccache-ci's
/// stop hook on older zccache, AV quarantine, or a Windows binary
/// replacement). We allocate a fresh session via `zccache session-start` and
/// retry the wrapper invocation exactly once with the new session id.
///
/// Retry budget is 1. On the retry's own failure we propagate that exit code
/// unchanged — we don't loop on a persistently broken daemon.
#[cfg(not(unix))]
fn run_wrapper_through_zccache_windows(
    raw_args: &[String],
    zccache: &std::path::Path,
) -> Result<i32, SoldrError> {
    use std::io::Read;
    use std::process::Stdio;

    let mut command = std::process::Command::new(zccache);
    command.args(&raw_args[1..]);
    command.stderr(Stdio::piped());
    suppress_windows_console_window(&mut command);

    let mut child = command.spawn()?;
    let stderr = child
        .stderr
        .take()
        .expect("stderr was configured as piped above");

    // Tee zccache stderr to soldr's stderr in real time AND buffer it for
    // post-exit inspection. A reader thread keeps the pipe drained so
    // zccache cannot block on a full pipe.
    let reader = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut reader = std::io::BufReader::new(stderr);
        let mut chunk = [0u8; 4096];
        loop {
            let n = reader.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            // Best-effort tee: if writing to our own stderr fails we still
            // want to keep draining the child pipe.
            let _ = std::io::Write::write_all(&mut std::io::stderr(), &chunk[..n]);
            buf.extend_from_slice(&chunk[..n]);
        }
        Ok(buf)
    });

    let status = child.wait()?;
    let stderr_bytes = reader
        .join()
        .map_err(|_| SoldrError::Other("zccache stderr reader thread panicked".into()))?
        .unwrap_or_default();

    let exit_code = status.code().unwrap_or(1);
    if status.success() || !stderr_indicates_unknown_session(&stderr_bytes) {
        return Ok(exit_code);
    }

    // Daemon told us our session id is gone. Allocate a fresh one and
    // retry the wrapper invocation once.
    let new_session_id = match allocate_replacement_session(zccache) {
        Ok(id) => id,
        Err(err) => {
            eprintln!(
                "soldr: zccache reported \"unknown session:\" but soldr could not allocate \
                 a replacement session ({err}); propagating original exit code"
            );
            return Ok(exit_code);
        }
    };

    eprintln!(
        "soldr: zccache session resync after \"unknown session:\"; retrying once with fresh session {new_session_id}"
    );

    let mut retry = std::process::Command::new(zccache);
    retry.args(&raw_args[1..]);
    retry.env(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR, &new_session_id);
    suppress_windows_console_window(&mut retry);
    let retry_status = retry.status()?;
    Ok(retry_status.code().unwrap_or(1))
}

/// Returns `true` iff `stderr` contains the literal substring
/// `unknown session:` somewhere in its bytes. Tolerates non-UTF-8 input.
///
/// Extracted as a pure helper so the retry trigger can be unit-tested
/// without spawning a real zccache.
#[cfg_attr(unix, allow(dead_code))]
fn stderr_indicates_unknown_session(stderr: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"unknown session:";
    if stderr.len() < NEEDLE.len() {
        return false;
    }
    stderr.windows(NEEDLE.len()).any(|w| w == NEEDLE)
}

/// Run `zccache session-start --stats --log <path> --journal <path>` against
/// the cache dir the wrapper invocation inherits from cargo, and return the
/// parsed session id. Mirrors the args used by `prepare_zccache_build`.
///
/// Used by the Windows wrapper retry path (issue #265): when the daemon
/// reports `unknown session:` the in-process session id is stale, so soldr
/// allocates a replacement before retrying the wrapper invocation once.
#[cfg(not(unix))]
fn allocate_replacement_session(zccache: &std::path::Path) -> Result<String, SoldrError> {
    let cache_dir = std::env::var_os(soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR)
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "{} is not set in the wrapper environment; cannot allocate replacement zccache session",
                soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR
            ))
        })?;

    let session_log_path = soldr_cache::session_log_path(&cache_dir);
    let session_log_path_arg = session_log_path.display().to_string();
    let journal_path = soldr_cache::session_journal_path(&cache_dir);
    let journal_path_arg = journal_path.display().to_string();
    let session_json = run_zccache_command_in_cache_dir(
        zccache,
        &[
            "session-start",
            "--stats",
            "--log",
            &session_log_path_arg,
            "--journal",
            &journal_path_arg,
        ],
        &cache_dir,
    )?;
    soldr_cache::parse_zccache_session_id(&session_json.stdout).ok_or_else(|| {
        SoldrError::Other(format!(
            "failed to parse zccache session id from output: {}",
            session_json.stdout.trim()
        ))
    })
}

/// Best-effort upsert of the workspace `target/` dir into the soldr
/// state registry on every wrapper invocation. Silent on failure.
///
/// `rustc_args` is the slice of args that follows the rustc binary
/// path in the wrapper invocation (i.e. `raw_args[2..]`).
fn record_target_dir_in_registry(rustc_args: &[String]) {
    let Some(target) = soldr_cache::target_registry::resolve_workspace_target_dir(rustc_args)
    else {
        return;
    };
    let Ok(paths) = SoldrPaths::new() else { return };
    let db_path = soldr_cache::data_db_path(&paths);
    let Ok(registry) = soldr_cache::target_registry::TargetRegistry::open(&db_path) else {
        return;
    };
    let _ = registry.upsert(&target);
}

async fn run_cargo_front_door(args: &[String], cache_enabled: bool) -> Result<i32, SoldrError> {
    if cargo_args_use_reserved_no_cache(args) {
        return Err(SoldrError::Other(
            "`--no-cache` must appear before `cargo`, as in `soldr --no-cache cargo build`".into(),
        ));
    }

    let cargo = resolve_toolchain_binary("cargo")?;
    let rustc = resolve_toolchain_binary("rustc")?;
    let cargo_bin_dir = cargo
        .parent()
        .ok_or_else(|| SoldrError::Other("failed to resolve cargo bin directory".into()))?
        .to_path_buf();
    let existing_path = std::env::var_os("PATH");
    let paths = SoldrPaths::new()?;
    paths.ensure_dirs()?;

    // If the user invoked a known ecosystem subcommand (e.g. `cargo nextest`),
    // fetch the corresponding `cargo-<sub>` binary and prepend its directory to
    // PATH so cargo's subcommand dispatch finds it.
    let extra_bin_dirs = ensure_known_subcommand_tool(args, &paths).await?;

    let mut command = std::process::Command::new(&cargo);
    command.args(args);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    // soldr cargo is the top of the invocation tree, so any inherited
    // MAKEFLAGS/CARGO_MAKEFLAGS points at jobserver fds that aren't open in
    // our process. Stripping them lets cargo start a fresh jobserver instead
    // of printing the "failed to connect to jobserver" warning (see #283).
    command.env_remove("MAKEFLAGS");
    command.env_remove("CARGO_MAKEFLAGS");
    command.env("RUSTC", &rustc);
    let build_like_cargo = cargo_args_are_cacheable(args);
    let cache_enabled_for_cargo = cache_enabled && build_like_cargo;
    let cargo_profile_debug_default = if build_like_cargo {
        maybe_apply_cargo_profile_debug_default(&mut command, args, &paths)?
    } else {
        None
    };

    command.env(
        soldr_cache::CACHE_ENABLED_ENV_VAR,
        soldr_cache::cache_enabled_env_value(cache_enabled_for_cargo),
    );
    if build_like_cargo {
        // Cargo front door only: keep startup/low-disk warnings off unrelated
        // commands and out of the rustc-wrapper hot path.
        gc::emit_startup_target_warning_if_due();
        // Best-effort auto-GC trigger (issue #323). Runs on a detached
        // background thread; never blocks the build.
        gc::maybe_kick_auto_gc(&paths);
    }
    let mut path_dirs: Vec<std::path::PathBuf> = Vec::with_capacity(1 + extra_bin_dirs.len());
    path_dirs.push(cargo_bin_dir);
    path_dirs.extend(extra_bin_dirs);
    command.env("PATH", prepend_paths(&path_dirs, existing_path.as_deref())?);
    let explicit_target = default_cargo_build_target(args)?;
    if let Some(target) = explicit_target.as_deref() {
        command.env("CARGO_BUILD_TARGET", target);
    }

    apply_linker_override(&mut command, args, explicit_target.as_deref(), &paths)?;

    let session = if cache_enabled_for_cargo {
        prepare_rustc_wrapper(&mut command, &paths).await?
    } else {
        None
    };

    let plan_ctx = if let Some(session) = session.as_ref() {
        rust_plan::maybe_prepare_rust_artifact_plan(
            &cargo,
            &rustc,
            args,
            session,
            cargo_profile_debug_default.as_ref(),
        )?
    } else {
        None
    };
    if build_like_cargo {
        let probe_path = plan_ctx
            .as_ref()
            .map(|plan| std::path::PathBuf::from(&plan.target_dir))
            .unwrap_or_else(|| cargo_disk_space_probe_path(args));
        maybe_emit_low_disk_warning(&probe_path);
    }
    if let Some(plan) = plan_ctx.as_ref() {
        if let Some(reason) = rust_plan::should_skip_warm_restore(plan) {
            eprintln!("{reason}");
        } else {
            rust_plan::run_zccache_rust_plan(plan, "restore", false)?;
        }
    }

    let status = command.status()?;
    if status.success() {
        if let Some(plan) = plan_ctx.as_ref() {
            rust_plan::run_zccache_rust_plan(plan, "save", true)?;
            rust_plan::write_warm_restore_sentinel(plan);
        }
    }
    if let Some(session) = session {
        finish_zccache_build(&session)?;
    }
    Ok(status.code().unwrap_or(1))
}


pub(crate) fn cargo_profile(args: &[String]) -> &str {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--release" {
            return "release";
        }
        if arg == "--profile" {
            return iter.next().map(String::as_str).unwrap_or("debug");
        }
        if let Some(value) = arg.strip_prefix("--profile=") {
            return value;
        }
    }
    "debug"
}

pub(crate) fn cargo_target_triple(args: &[String], host: &str) -> String {
    cargo_target_arg(args)
        .or_else(|| std::env::var("CARGO_BUILD_TARGET").ok())
        .unwrap_or_else(|| host.to_string())
}

fn cargo_target_arg(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--target" {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--target=") {
            return Some(value.to_string());
        }
    }
    None
}

pub(crate) fn cargo_feature_inputs(args: &[String]) -> Vec<String> {
    selected_cargo_args(
        args,
        &[
            "--features",
            "--all-features",
            "--no-default-features",
            "--package",
            "-p",
            "--workspace",
            "--exclude",
            "--all-targets",
            "--lib",
            "--bins",
            "--bin",
            "--examples",
            "--example",
            "--tests",
            "--test",
            "--benches",
            "--bench",
        ],
    )
}

pub(crate) fn selected_cargo_args(args: &[String], names: &[&str]) -> Vec<String> {
    let mut selected = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if names.contains(&arg.as_str()) {
            selected.push(arg.clone());
            if !matches!(
                arg.as_str(),
                "--all-features"
                    | "--no-default-features"
                    | "--workspace"
                    | "--all-targets"
                    | "--lib"
                    | "--bins"
                    | "--examples"
                    | "--tests"
                    | "--benches"
            ) {
                if let Some(value) = iter.next() {
                    selected.push(value.clone());
                }
            }
            continue;
        }
        if names
            .iter()
            .any(|name| arg.starts_with(&format!("{name}=")))
        {
            selected.push(arg.clone());
        }
    }
    selected
}

pub(crate) fn rustflags_inputs() -> Vec<(String, String)> {
    sorted_env_vars(|name| {
        name == "RUSTFLAGS"
            || name == "CARGO_ENCODED_RUSTFLAGS"
            || (name.starts_with("CARGO_TARGET_") && name.ends_with("_RUSTFLAGS"))
    })
}

pub(crate) fn build_env_inputs(
    cargo_profile_debug_default: Option<&CargoProfileDebugDefault>,
) -> Vec<(String, String)> {
    let mut vars = sorted_env_vars(|name| {
        name == "CARGO_BUILD_TARGET"
            || name == "CARGO_TARGET_DIR"
            || name.starts_with("CARGO_PROFILE_")
            || name.starts_with("CARGO_CFG_")
    });
    if let Some(default) = cargo_profile_debug_default {
        if !vars.iter().any(|(name, _)| name == default.env_var) {
            vars.push((default.env_var.to_string(), "false".to_string()));
        }
        vars.sort_by(|a, b| a.0.cmp(&b.0));
    }
    vars
}

fn sorted_env_vars<F>(include: F) -> Vec<(String, String)>
where
    F: Fn(&str) -> bool,
{
    let mut vars = std::env::vars()
        .filter(|(name, _)| include(name))
        .collect::<Vec<_>>();
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    vars
}

pub(crate) fn workspace_manifest_hashes(workspace_root: &std::path::Path) -> Result<Vec<String>, SoldrError> {
    let mut hashes = Vec::new();
    collect_manifest_hashes(workspace_root, workspace_root, &mut hashes)?;
    hashes.sort();
    Ok(hashes)
}

fn collect_manifest_hashes(
    workspace_root: &std::path::Path,
    dir: &std::path::Path,
    hashes: &mut Vec<String>,
) -> Result<(), SoldrError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if matches!(
                entry.file_name().to_str(),
                Some(".git" | "target" | ".soldr" | "node_modules")
            ) {
                continue;
            }
            collect_manifest_hashes(workspace_root, &path, hashes)?;
        } else if file_type.is_file() && entry.file_name() == std::ffi::OsStr::new("Cargo.toml") {
            let relative = path
                .strip_prefix(workspace_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            hashes.push(format!("{relative}:{}", file_hash_or_missing(&path)?));
        }
    }
    Ok(())
}

pub(crate) fn cargo_config_hash(workspace_root: &std::path::Path) -> Result<String, SoldrError> {
    let mut inputs = Vec::new();
    for relative in [".cargo/config.toml", ".cargo/config"] {
        let path = workspace_root.join(relative);
        if path.exists() {
            inputs.push(format!("{relative}:{}", file_hash_or_missing(&path)?));
        }
    }
    Ok(stable_hash_json(&inputs))
}

pub(crate) fn file_hash_or_missing(path: &std::path::Path) -> Result<String, SoldrError> {
    if !path.exists() {
        return Ok("missing".to_string());
    }
    Ok(sha256_bytes(&std::fs::read(path)?))
}

pub(crate) fn stable_hash_json<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    sha256_bytes(&bytes)
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

pub(crate) fn path_string(path: &std::path::Path) -> String {
    path.display().to_string()
}

fn default_cargo_build_target(args: &[String]) -> Result<Option<String>, SoldrError> {
    if !cfg!(windows) {
        return Ok(None);
    }
    if cargo_args_specify_target(args) || std::env::var_os("CARGO_BUILD_TARGET").is_some() {
        return Ok(None);
    }

    Ok(Some(soldr_core::TargetTriple::detect()?.triple()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CargoProfileDebugDefault {
    pub(crate) profile: &'static str,
    pub(crate) env_var: &'static str,
}

impl CargoProfileDebugDefault {
    fn for_profile(profile: &str) -> Option<Self> {
        match profile {
            "dev" | "debug" => Some(Self {
                profile: "dev",
                env_var: CARGO_PROFILE_DEV_DEBUG_ENV_VAR,
            }),
            "test" => Some(Self {
                profile: "test",
                env_var: CARGO_PROFILE_TEST_DEBUG_ENV_VAR,
            }),
            _ => None,
        }
    }

    fn lookup_profiles(self) -> &'static [&'static str] {
        match self.profile {
            "test" => &["test", "dev"],
            _ => &["dev"],
        }
    }
}

fn maybe_apply_cargo_profile_debug_default(
    command: &mut std::process::Command,
    args: &[String],
    paths: &SoldrPaths,
) -> Result<Option<CargoProfileDebugDefault>, SoldrError> {
    let Some(default) = cargo_profile_debug_default_for_args(args) else {
        return Ok(None);
    };
    if cargo_profile_debug_is_specified(args, default)? {
        return Ok(None);
    }

    command.env(default.env_var, "false");
    let repo_path = cargo_debug_warning_repo_path(args);
    if should_emit_cargo_debug_default_warning(paths, &repo_path) {
        eprintln!(
            "soldr: warning: Cargo profile.{}.debug is unspecified for {}; setting {}=false for this invocation. Add `debug = true` or `debug = false` under `[profile.{}]` in Cargo.toml or .cargo/config.toml to make this explicit.",
            default.profile,
            repo_path.display(),
            default.env_var,
            default.profile
        );
    }

    Ok(Some(default))
}

fn cargo_profile_debug_default_for_args(args: &[String]) -> Option<CargoProfileDebugDefault> {
    let subcommand = first_cargo_subcommand(args)?;

    if subcommand == "nextest" {
        return if cargo_args_contain_release(args) {
            None
        } else {
            CargoProfileDebugDefault::for_profile("test")
        };
    }

    if cargo_args_contain_release(args) {
        return None;
    }

    if let Some(profile) = cargo_profile_arg_value(args) {
        return CargoProfileDebugDefault::for_profile(&profile);
    }

    match subcommand {
        "t" | "test" => CargoProfileDebugDefault::for_profile("test"),
        "install" if cargo_install_args_contain_debug(args) => {
            CargoProfileDebugDefault::for_profile("dev")
        }
        "install" | "bench" => None,
        "b" | "build" | "c" | "check" | "d" | "doc" | "r" | "run" | "rustc" | "clippy" | "fix" => {
            CargoProfileDebugDefault::for_profile("dev")
        }
        _ => None,
    }
}

fn cargo_profile_debug_is_specified(
    args: &[String],
    default: CargoProfileDebugDefault,
) -> Result<bool, SoldrError> {
    let profiles = default.lookup_profiles();
    if profiles.iter().any(|profile| {
        cargo_profile_debug_env_var(profile)
            .is_some_and(|env_var| std::env::var_os(env_var).is_some())
    }) {
        return Ok(true);
    }

    if cargo_config_args_specify_profile_debug(args, profiles)? {
        return Ok(true);
    }

    let start_dir = cargo_profile_lookup_start_dir(args)?;
    if cargo_manifest_specifies_profile_debug(&start_dir, profiles) {
        return Ok(true);
    }
    if cargo_config_files_specify_profile_debug(&start_dir, profiles) {
        return Ok(true);
    }

    Ok(false)
}

fn cargo_profile_debug_env_var(profile: &str) -> Option<&'static str> {
    match profile {
        "dev" => Some(CARGO_PROFILE_DEV_DEBUG_ENV_VAR),
        "test" => Some(CARGO_PROFILE_TEST_DEBUG_ENV_VAR),
        _ => None,
    }
}

fn cargo_args_contain_release(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--release")
}

fn cargo_profile_arg_value(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--profile" {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--profile=") {
            return Some(value.to_string());
        }
    }
    None
}

fn cargo_install_args_contain_debug(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--debug")
}

fn cargo_config_args_specify_profile_debug(
    args: &[String],
    profiles: &[&str],
) -> Result<bool, SoldrError> {
    let cwd = std::env::current_dir()?;
    for value in cargo_config_arg_values(args) {
        if cargo_config_arg_specifies_profile_debug(&value, &cwd, profiles) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn cargo_config_arg_values(args: &[String]) -> Vec<String> {
    let mut values = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--config" {
            if let Some(value) = iter.next() {
                values.push(value.clone());
            }
            continue;
        }
        if let Some(value) = arg.strip_prefix("--config=") {
            values.push(value.to_string());
        }
    }
    values
}

fn cargo_config_arg_specifies_profile_debug(
    value: &str,
    cwd: &std::path::Path,
    profiles: &[&str],
) -> bool {
    let raw = value.trim();
    if raw.is_empty() {
        return false;
    }

    let path = std::path::Path::new(raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if path.is_file() {
        return toml_file_specifies_profile_debug(&path, profiles);
    }

    toml_text_specifies_profile_debug(raw, profiles)
        .unwrap_or_else(|| raw_may_specify_profile_debug(raw, profiles))
}

fn raw_may_specify_profile_debug(raw: &str, profiles: &[&str]) -> bool {
    let lowered = raw.to_ascii_lowercase();
    profiles.iter().any(|profile| {
        lowered.contains(&format!("profile.{profile}.debug"))
            || (lowered.contains(&format!("[profile.{profile}]")) && lowered.contains("debug"))
    })
}

fn cargo_manifest_specifies_profile_debug(start_dir: &std::path::Path, profiles: &[&str]) -> bool {
    find_workspace_manifest_path(start_dir)
        .is_some_and(|manifest| toml_file_specifies_profile_debug(&manifest, profiles))
}

fn cargo_config_files_specify_profile_debug(
    start_dir: &std::path::Path,
    profiles: &[&str],
) -> bool {
    cargo_config_paths(start_dir)
        .iter()
        .any(|path| toml_file_specifies_profile_debug(path, profiles))
}

fn cargo_config_paths(start_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths = BTreeSet::new();
    let mut current = Some(start_dir.to_path_buf());
    while let Some(dir) = current {
        for relative in [".cargo/config.toml", ".cargo/config"] {
            let path = dir.join(relative);
            if path.is_file() {
                paths.insert(path);
            }
        }
        current = dir.parent().map(std::path::Path::to_path_buf);
    }

    if let Some(cargo_home) = cargo_home_dir_for_config() {
        for name in ["config.toml", "config"] {
            let path = cargo_home.join(name);
            if path.is_file() {
                paths.insert(path);
            }
        }
    }

    paths.into_iter().collect()
}

fn cargo_home_dir_for_config() -> Option<std::path::PathBuf> {
    std::env::var_os(soldr_core::CARGO_HOME_ENV_VAR)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            soldr_core::user_home_dir()
                .ok()
                .map(|home| home.join(".cargo"))
        })
}

fn toml_file_specifies_profile_debug(path: &std::path::Path, profiles: &[&str]) -> bool {
    match std::fs::read_to_string(path) {
        Ok(text) => toml_text_specifies_profile_debug(&text, profiles).unwrap_or(true),
        Err(_) => true,
    }
}

fn toml_text_specifies_profile_debug(text: &str, profiles: &[&str]) -> Option<bool> {
    let value: toml::Value = text.parse().ok()?;
    let Some(profile_table) = value.get("profile") else {
        return Some(false);
    };
    Some(profiles.iter().any(|profile| {
        profile_table
            .get(*profile)
            .and_then(|section| section.get("debug"))
            .is_some()
    }))
}

fn cargo_profile_lookup_start_dir(args: &[String]) -> Result<std::path::PathBuf, SoldrError> {
    let cwd = std::env::current_dir()?;
    let Some(manifest_path) = cargo_manifest_path_arg(args) else {
        return Ok(cwd);
    };
    let manifest_path = if manifest_path.is_absolute() {
        manifest_path
    } else {
        cwd.join(manifest_path)
    };
    let parent = manifest_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or(cwd);
    Ok(parent)
}

fn cargo_manifest_path_arg(args: &[String]) -> Option<std::path::PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--manifest-path" {
            return iter.next().map(std::path::PathBuf::from);
        }
        if let Some(value) = arg.strip_prefix("--manifest-path=") {
            return Some(std::path::PathBuf::from(value));
        }
    }
    None
}

fn find_workspace_manifest_path(start_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = start_dir.to_path_buf();
    let mut nearest_manifest = None;
    let mut workspace_manifest = None;

    loop {
        let candidate = current.join("Cargo.toml");
        if candidate.is_file() {
            if nearest_manifest.is_none() {
                nearest_manifest = Some(candidate.clone());
            }
            if cargo_manifest_declares_workspace(&candidate) {
                workspace_manifest = Some(candidate);
            }
        }
        if !current.pop() {
            break;
        }
    }

    workspace_manifest.or(nearest_manifest)
}

fn cargo_manifest_declares_workspace(path: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = text.parse::<toml::Value>() else {
        return false;
    };
    value.get("workspace").is_some()
}

fn cargo_debug_warning_repo_path(args: &[String]) -> std::path::PathBuf {
    let start_dir = cargo_profile_lookup_start_dir(args)
        .or_else(|_| std::env::current_dir().map_err(SoldrError::from))
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    find_git_root(&start_dir)
        .or_else(|| {
            find_workspace_manifest_path(&start_dir)
                .and_then(|manifest| manifest.parent().map(std::path::Path::to_path_buf))
        })
        .unwrap_or(start_dir)
}

fn find_git_root(start_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = start_dir.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn should_emit_cargo_debug_default_warning(
    paths: &SoldrPaths,
    repo_path: &std::path::Path,
) -> bool {
    let db_path = soldr_cache::state_db_path(paths);
    soldr_cache::state_db::StateDb::open(&db_path)
        .and_then(|db| db.should_emit_cargo_debug_default_warning(repo_path))
        .unwrap_or(true)
}

/// Apply the `SOLDR_LINKER` / `config.toml linker = ...` override (issue
/// #285) to the cargo subprocess command.
///
/// The active target triple is resolved in the same order as cargo:
/// 1. an explicit `CARGO_BUILD_TARGET` injected by `default_cargo_build_target`,
/// 2. a `CARGO_BUILD_TARGET` already in the parent env,
/// 3. an `--target` flag inside `args`,
/// 4. the auto-detected host triple from `TargetTriple::detect()`.
fn apply_linker_override(
    command: &mut std::process::Command,
    args: &[String],
    explicit_target: Option<&str>,
    paths: &SoldrPaths,
) -> Result<(), SoldrError> {
    let config = paths.load_config();
    let choice = linker::from_env_and_config(
        std::env::var_os(LINKER_ENV_VAR).as_deref(),
        config.linker.as_deref(),
    )?;
    if matches!(choice, linker::LinkerChoice::Default) {
        // Fast-path: skip target detection entirely when there is nothing
        // to inject. Keeps `soldr cargo` no-ops on platforms where target
        // detection might fail or be slow.
        return Ok(());
    }

    let target = resolve_active_target_triple(args, explicit_target)?;
    let injection = linker::resolve_for_target(choice, &target)?;
    let prefix = linker::cargo_target_env_prefix(&target);
    if let Some(linker_path) = injection.linker {
        command.env(format!("CARGO_TARGET_{prefix}_LINKER"), linker_path);
    }
    if let Some(rustflags) = injection.rustflags {
        command.env(format!("CARGO_TARGET_{prefix}_RUSTFLAGS"), rustflags);
    }
    Ok(())
}

fn resolve_active_target_triple(
    args: &[String],
    explicit_target: Option<&str>,
) -> Result<String, SoldrError> {
    if let Some(target) = explicit_target {
        return Ok(target.to_string());
    }
    if let Some(target) = std::env::var_os("CARGO_BUILD_TARGET") {
        if let Some(s) = target.to_str() {
            let s = s.trim();
            if !s.is_empty() {
                return Ok(s.to_string());
            }
        }
    }
    if let Some(target) = cargo_args_target_value(args) {
        return Ok(target);
    }
    Ok(soldr_core::TargetTriple::detect()?.triple())
}

fn cargo_args_target_value(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--target" {
            return iter.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix("--target=") {
            return Some(rest.to_string());
        }
    }
    None
}

fn cargo_args_specify_target(args: &[String]) -> bool {
    for arg in args {
        if arg == "--" {
            break;
        }
        if arg == "--target" {
            return true;
        }
        if arg.starts_with("--target=") {
            return true;
        }
    }
    false
}

fn cargo_args_use_reserved_no_cache(args: &[String]) -> bool {
    for arg in args {
        if arg == "--" {
            break;
        }
        if arg == "--no-cache" {
            return true;
        }
    }
    false
}

fn cargo_args_are_cacheable(args: &[String]) -> bool {
    let Some(subcommand) = first_cargo_subcommand(args) else {
        return false;
    };

    matches!(
        subcommand,
        "b" | "build"
            | "c"
            | "check"
            | "t"
            | "test"
            | "bench"
            | "d"
            | "doc"
            | "r"
            | "run"
            | "rustc"
            | "clippy"
            | "fix"
            | "install"
            | "nextest"
    )
}

fn maybe_emit_low_disk_warning(path: &std::path::Path) {
    if let Some(message) =
        low_disk_warning_for_path(path, stderr_should_use_color(), available_space)
    {
        eprintln!("{message}");
    }
}

fn low_disk_warning_for_path<F>(
    path: &std::path::Path,
    use_color: bool,
    available_space: F,
) -> Option<String>
where
    F: FnOnce(&std::path::Path) -> std::io::Result<u64>,
{
    let probe_path = existing_filesystem_probe_path(path);
    let free_bytes = available_space(&probe_path).ok()?;
    low_disk_warning_for_free_bytes(free_bytes, use_color)
}

fn low_disk_warning_for_free_bytes(free_bytes: u64, use_color: bool) -> Option<String> {
    if free_bytes >= LOW_DISK_WARNING_THRESHOLD_BYTES {
        return None;
    }
    let warning = if use_color {
        "\x1b[33mwarning\x1b[0m"
    } else {
        "warning"
    };
    Some(format!(
        "soldr: {warning}: disk space is low ({} free). Run `soldr gc` to review reclaimable Rust target directories.",
        soldr_cache::target_registry::human_size(free_bytes),
    ))
}

fn stderr_should_use_color() -> bool {
    use std::io::IsTerminal;

    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}

pub(crate) fn available_space(path: &std::path::Path) -> std::io::Result<u64> {
    if let Some(raw) = std::env::var_os(TEST_FREE_DISK_BYTES_ENV_VAR) {
        let raw = raw.to_string_lossy();
        if raw.eq_ignore_ascii_case("error") {
            return Err(std::io::Error::other("test disk-space failure"));
        }
        return raw.parse::<u64>().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid {TEST_FREE_DISK_BYTES_ENV_VAR}: {e}"),
            )
        });
    }
    fs2::available_space(path)
}

pub(crate) fn existing_filesystem_probe_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut cursor = if path.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    } else {
        path.to_path_buf()
    };
    loop {
        if cursor.exists() {
            return cursor;
        }
        if !cursor.pop() {
            return std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        }
    }
}

fn cargo_disk_space_probe_path(args: &[String]) -> std::path::PathBuf {
    if let Some(target_dir) = cargo_arg_value(args, "--target-dir") {
        return absolutize_path(std::path::PathBuf::from(target_dir));
    }
    if let Some(target_dir) = non_empty_env_path("CARGO_TARGET_DIR") {
        return absolutize_path(target_dir);
    }
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn cargo_arg_value(args: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == flag {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix(&prefix) {
            return Some(value.to_string());
        }
    }
    None
}

fn absolutize_path(path: std::path::PathBuf) -> std::path::PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    }
}

fn prepend_paths(
    dirs: &[std::path::PathBuf],
    existing_path: Option<&std::ffi::OsStr>,
) -> Result<std::ffi::OsString, SoldrError> {
    let mut paths: Vec<std::path::PathBuf> = dirs.to_vec();
    if let Some(existing_path) = existing_path {
        paths.extend(std::env::split_paths(existing_path));
    }
    std::env::join_paths(paths).map_err(|e| SoldrError::Other(format!("invalid PATH: {e}")))
}

/// Return the first positional argument (skipping flags) of the cargo
/// front-door args, which is conventionally the cargo subcommand.
pub(crate) fn first_cargo_subcommand(args: &[String]) -> Option<&str> {
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            break;
        }
        if arg.starts_with('+') && arg.len() > 1 {
            continue;
        }
        if cargo_global_arg_takes_value(arg) {
            skip_next = !arg.contains('=');
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.as_str());
    }
    None
}

fn cargo_global_arg_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-C" | "-Z"
            | "-j"
            | "--color"
            | "--config"
            | "--jobs"
            | "--manifest-path"
            | "--message-format"
            | "--target-dir"
    ) || arg.starts_with("-C=")
        || arg.starts_with("-Z=")
        || arg.starts_with("-j=")
        || arg.starts_with("--color=")
        || arg.starts_with("--config=")
        || arg.starts_with("--jobs=")
        || arg.starts_with("--manifest-path=")
        || arg.starts_with("--message-format=")
        || arg.starts_with("--target-dir=")
}

async fn ensure_known_subcommand_tool(
    args: &[String],
    paths: &SoldrPaths,
) -> Result<Vec<std::path::PathBuf>, SoldrError> {
    let Some(sub) = first_cargo_subcommand(args) else {
        return Ok(Vec::new());
    };
    let Some(spec) = soldr_fetch::lookup_by_cargo_subcommand(sub) else {
        return Ok(Vec::new());
    };

    eprintln!("soldr: fetching {}...", spec.crate_name);
    let result =
        soldr_fetch::fetch_tool_with_paths(spec.crate_name, &VersionSpec::Latest, paths).await?;

    if result.cached {
        eprintln!(
            "soldr: using cached {} v{}",
            spec.crate_name, result.version
        );
    } else {
        eprintln!("soldr: downloaded {} v{}", spec.crate_name, result.version);
    }

    let dir = result
        .binary_path
        .parent()
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "failed to resolve bin dir for fetched {}",
                spec.crate_name
            ))
        })?
        .to_path_buf();
    Ok(vec![dir])
}

pub(crate) fn resolve_toolchain_binary(tool: &str) -> Result<std::path::PathBuf, SoldrError> {
    if let Some(path) = toolchain_binary_override(tool) {
        return Ok(path);
    }

    let start_dir = std::env::current_dir().ok();
    if let Some(path) = probe_direct_toolchain_binary(tool, start_dir.as_deref()) {
        return Ok(path);
    }

    let mut command = std::process::Command::new(rustup_binary());
    command.args(["which", tool]);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command.output();

    match output {
        Ok(output) if output.status.success() => {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(path.into());
            }
        }
        Ok(output) => {
            if let Some(path) = soldr_core::probe_toolchain_binary(tool, start_dir.as_deref()) {
                return Ok(path);
            }
            return Err(rustup_resolution_failure(tool, &output.stderr));
        }
        Err(err) => {
            if let Some(path) = soldr_core::probe_toolchain_binary(tool, start_dir.as_deref()) {
                return Ok(path);
            }
            return Err(SoldrError::Other(format!(
                "failed to invoke rustup while resolving {tool}: {err}"
            )));
        }
    }

    if let Some(path) = soldr_core::probe_toolchain_binary(tool, start_dir.as_deref()) {
        return Ok(path);
    }

    Err(SoldrError::Other(format!(
        "rustup did not return a path for {tool}"
    )))
}

fn probe_direct_toolchain_binary(
    tool: &str,
    start_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    if std::env::var_os("RUSTUP_TOOLCHAIN").is_some_and(|value| !value.is_empty()) {
        return None;
    }

    explicit_rustup_toolchain_binary(tool)
        .or_else(|| repo_local_rustup_toolchain_binary(tool, start_dir))
        .or_else(|| explicit_cargo_home_binary(tool))
        .or_else(|| repo_local_cargo_home_binary(tool, start_dir))
}

fn explicit_cargo_home_binary(tool: &str) -> Option<std::path::PathBuf> {
    non_empty_env_path("CARGO_HOME").and_then(|path| executable_in_dir(&path.join("bin"), tool))
}

fn repo_local_cargo_home_binary(
    tool: &str,
    start_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    find_ancestor_dir(start_dir, ".cargo")
        .and_then(|path| executable_in_dir(&path.join("bin"), tool))
}

fn explicit_rustup_toolchain_binary(tool: &str) -> Option<std::path::PathBuf> {
    non_empty_env_path("RUSTUP_HOME")
        .and_then(|path| rustup_home_single_toolchain_binary(&path, tool))
}

fn repo_local_rustup_toolchain_binary(
    tool: &str,
    start_dir: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    find_ancestor_dir(start_dir, ".rustup")
        .and_then(|path| rustup_home_single_toolchain_binary(&path, tool))
}

fn rustup_home_single_toolchain_binary(
    rustup_home: &std::path::Path,
    tool: &str,
) -> Option<std::path::PathBuf> {
    let mut candidates = std::fs::read_dir(rustup_home.join("toolchains"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("bin"))
        .filter_map(|dir| executable_in_dir(&dir, tool))
        .collect::<Vec<_>>();
    if candidates.len() == 1 {
        candidates.pop()
    } else {
        None
    }
}

fn find_ancestor_dir(
    start_dir: Option<&std::path::Path>,
    relative: &str,
) -> Option<std::path::PathBuf> {
    let mut current = start_dir?.to_path_buf();
    loop {
        let candidate = current.join(relative);
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn executable_in_dir(dir: &std::path::Path, tool: &str) -> Option<std::path::PathBuf> {
    let candidate = dir.join(tool);
    if candidate.is_file() {
        return Some(candidate);
    }
    #[cfg(windows)]
    {
        for suffix in windows_path_exts() {
            let candidate = dir.join(format!("{tool}{suffix}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn windows_path_exts() -> Vec<String> {
    std::env::var_os("PATHEXT")
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .collect()
}

pub(crate) fn apply_implicit_toolchain_homes(command: &mut std::process::Command) {
    let start_dir = std::env::current_dir().ok();
    soldr_core::apply_implicit_toolchain_homes(command, start_dir.as_deref());
}

fn rustup_resolution_failure(tool: &str, stderr: &[u8]) -> SoldrError {
    let raw_failure = String::from_utf8_lossy(stderr).trim().to_string();
    SoldrError::Other(format!(
        "failed to resolve {tool} via rustup: {raw_failure}\n\
CI hint: if this repository pins Rust in rust-toolchain.toml, preinstall that exact channel instead of a generic stable toolchain.\n\
CI hint: export RUSTUP_TOOLCHAIN to that exact channel for later cargo, rustc, and soldr cargo steps, or use the documented setup-soldr action path (uses: zackees/soldr@<ref> or uses: ./)."
    ))
}

fn parse_tool_spec(spec: &str) -> (String, VersionSpec) {
    if let Some((name, version)) = spec.split_once('@') {
        (name.to_string(), VersionSpec::parse(version))
    } else {
        (spec.to_string(), VersionSpec::Latest)
    }
}

pub(crate) struct ZccacheBuildSession {
    pub(crate) binary_path: std::path::PathBuf,
    pub(crate) cache_dir: std::path::PathBuf,
    pub(crate) session_id: String,
    pub(crate) session_log_path: std::path::PathBuf,
    pub(crate) journal_path: std::path::PathBuf,
    pub(crate) session_stats_path: std::path::PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RustcWrapperMode {
    ManagedZccache,
    Custom(std::ffi::OsString),
    Disabled,
}

fn rustc_wrapper_mode_from_env_var(value: Option<&std::ffi::OsStr>) -> RustcWrapperMode {
    match value.and_then(std::ffi::OsStr::to_str) {
        None => value
            .map(|value| RustcWrapperMode::Custom(value.to_os_string()))
            .unwrap_or(RustcWrapperMode::ManagedZccache),
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
                RustcWrapperMode::Disabled
            } else {
                RustcWrapperMode::Custom(trimmed.into())
            }
        }
    }
}

fn rustc_wrapper_mode() -> RustcWrapperMode {
    rustc_wrapper_mode_from_env_var(std::env::var_os(RUSTC_WRAPPER_OVERRIDE_ENV_VAR).as_deref())
}

async fn prepare_rustc_wrapper(
    cargo: &mut std::process::Command,
    paths: &SoldrPaths,
) -> Result<Option<ZccacheBuildSession>, SoldrError> {
    match rustc_wrapper_mode() {
        RustcWrapperMode::ManagedZccache => prepare_zccache_build(cargo, paths).await.map(Some),
        RustcWrapperMode::Custom(wrapper) => {
            if is_sccache_wrapper(&wrapper) && std::env::var_os("SCCACHE_DIR").is_none() {
                let sccache_dir = soldr_cache::sccache_dir(paths);
                std::fs::create_dir_all(&sccache_dir)?;
                cargo.env("SCCACHE_DIR", sccache_dir);
            }
            cargo.env("RUSTC_WRAPPER", wrapper);
            cargo.env_remove(soldr_cache::ZCCACHE_BINARY_ENV_VAR);
            cargo.env_remove(soldr_cache::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR);
            cargo.env_remove(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR);
            Ok(None)
        }
        RustcWrapperMode::Disabled => {
            cargo.env_remove("RUSTC_WRAPPER");
            cargo.env_remove(soldr_cache::ZCCACHE_BINARY_ENV_VAR);
            cargo.env_remove(soldr_cache::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR);
            cargo.env_remove(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR);
            Ok(None)
        }
    }
}

fn is_sccache_wrapper(wrapper: &std::ffi::OsStr) -> bool {
    std::path::Path::new(wrapper)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|stem| stem.eq_ignore_ascii_case("sccache"))
}

async fn prepare_zccache_build(
    cargo: &mut std::process::Command,
    paths: &SoldrPaths,
) -> Result<ZccacheBuildSession, SoldrError> {
    let zccache_dir = managed_zccache_cache_dir(paths)?;
    std::fs::create_dir_all(&zccache_dir)?;
    std::fs::create_dir_all(zccache_dir.join("logs"))?;
    let fetch = fetch_managed_zccache(paths).await?;
    if fetch.cached {
        eprintln!(
            "soldr: using managed zccache {}",
            soldr_fetch::MANAGED_ZCCACHE_VERSION
        );
    } else {
        eprintln!(
            "soldr: fetched managed zccache {}",
            soldr_fetch::MANAGED_ZCCACHE_VERSION
        );
    }

    start_zccache_with_recovery(&fetch.binary_path, &zccache_dir)?;

    let session_log_path = soldr_cache::session_log_path(&zccache_dir);
    let session_log_path_arg = session_log_path.display().to_string();
    let journal_path = soldr_cache::session_journal_path(&zccache_dir);
    let journal_path_arg = journal_path.display().to_string();
    let session_stats_path = soldr_cache::session_stats_path(&zccache_dir);
    let session_json = run_zccache_command_in_cache_dir(
        &fetch.binary_path,
        &[
            "session-start",
            "--stats",
            "--log",
            &session_log_path_arg,
            "--journal",
            &journal_path_arg,
        ],
        &zccache_dir,
    )?;
    let session_id =
        soldr_cache::parse_zccache_session_id(&session_json.stdout).ok_or_else(|| {
            SoldrError::Other(format!(
                "failed to parse zccache session id from output: {}",
                session_json.stdout.trim()
            ))
        })?;

    cargo.env("RUSTC_WRAPPER", current_soldr_binary()?);
    cargo.env(soldr_cache::ZCCACHE_BINARY_ENV_VAR, &fetch.binary_path);
    cargo.env(soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR, &zccache_dir);
    cargo.env(soldr_cache::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR, &zccache_dir);
    cargo.env(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR, &session_id);

    Ok(ZccacheBuildSession {
        binary_path: fetch.binary_path,
        cache_dir: zccache_dir,
        session_id,
        session_log_path,
        journal_path,
        session_stats_path,
    })
}

fn finish_zccache_build(session: &ZccacheBuildSession) -> Result<(), SoldrError> {
    let output = run_zccache_command_raw_in_cache_dir(
        &session.binary_path,
        &["session-end", &session.session_id, "--json"],
        &session.cache_dir,
    )?;
    if session.session_log_path.exists() {
        eprintln!(
            "soldr: zccache session log: {}",
            session.session_log_path.display()
        );
    }
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stats_json = stdout.trim();
        if !stats_json.is_empty() {
            write_zccache_session_stats_json(session, stats_json)?;
            let stats = parse_zccache_session_stats_json(stats_json)?;
            print_zccache_session_stats(&stats, &session.session_stats_path);
        }
        return Ok(());
    }

    if zccache_json_flag_unsupported(&output) {
        eprintln!(
            "soldr: zccache JSON session summary unavailable; falling back to text session-end"
        );
        finish_zccache_build_text_fallback(session)?;
        return Ok(());
    }

    Err(SoldrError::Other(zccache_command_failure_message(
        &["session-end", &session.session_id, "--json"],
        &output,
    )))
}

fn finish_zccache_build_text_fallback(session: &ZccacheBuildSession) -> Result<(), SoldrError> {
    let output = run_zccache_command_in_cache_dir(
        &session.binary_path,
        &["session-end", &session.session_id],
        &session.cache_dir,
    )?;
    let stdout = output.stdout.trim();
    if !stdout.is_empty() {
        eprintln!("soldr: zccache session summary");
        eprintln!("{stdout}");
    }
    Ok(())
}

fn write_zccache_session_stats_json(
    session: &ZccacheBuildSession,
    stats_json: &str,
) -> Result<(), SoldrError> {
    if let Some(parent) = session.session_stats_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&session.session_stats_path, stats_json)?;
    Ok(())
}

fn parse_zccache_session_stats_json(stats_json: &str) -> Result<serde_json::Value, SoldrError> {
    serde_json::from_str(stats_json).map_err(|err| {
        SoldrError::Other(format!(
            "failed to parse zccache JSON session summary: {err}"
        ))
    })
}

fn print_zccache_session_stats(stats: &serde_json::Value, stats_path: &std::path::Path) {
    eprintln!("soldr: zccache session summary");
    eprintln!("  stats file: {}", stats_path.display());
    match stats.get("status").and_then(serde_json::Value::as_str) {
        Some("ok") => {
            let hits = json_u64(stats, "hits").unwrap_or(0);
            let misses = json_u64(stats, "misses").unwrap_or(0);
            let non_cacheable = json_u64(stats, "non_cacheable").unwrap_or(0);
            let errors = json_u64(stats, "errors").unwrap_or(0);
            let compilations = json_u64(stats, "compilations").unwrap_or(hits + misses);
            eprintln!(
                "  compilations: {compilations}; hits: {hits}; misses: {misses}; non-cacheable: {non_cacheable}; errors: {errors}"
            );
            if let Some(hit_rate) = json_f64(stats, "hit_rate") {
                eprintln!("  hit rate: {:.1}%", hit_rate * 100.0);
            } else {
                eprintln!("  hit rate: n/a");
            }
            let unique_sources = json_u64(stats, "unique_sources").unwrap_or(0);
            let bytes_read = json_u64(stats, "bytes_read").unwrap_or(0);
            let bytes_written = json_u64(stats, "bytes_written").unwrap_or(0);
            eprintln!(
                "  unique sources: {unique_sources}; bytes read: {bytes_read}; bytes written: {bytes_written}"
            );
            let time_saved_ms = json_u64(stats, "time_saved_ms").unwrap_or(0);
            let duration_ms = json_u64(stats, "duration_ms").unwrap_or(0);
            eprintln!("  time saved: {time_saved_ms} ms; duration: {duration_ms} ms");
        }
        Some("unavailable") => {
            let reason = stats
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            eprintln!("  status: unavailable ({reason})");
        }
        Some("error") => {
            let error = stats
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            eprintln!("  status: error ({error})");
        }
        Some(status) => eprintln!("  status: {status}"),
        None => eprintln!("  status: unknown"),
    }
}

fn json_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(serde_json::Value::as_u64)
}

fn json_f64(value: &serde_json::Value, key: &str) -> Option<f64> {
    value.get(key).and_then(serde_json::Value::as_f64)
}

fn zccache_json_flag_unsupported(output: &std::process::Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("unexpected argument")
        || stderr.contains("unrecognized option")
        || stderr.contains("found argument")
}


pub(crate) struct CommandOutput {
    pub(crate) stdout: String,
}

pub(crate) fn managed_zccache_cache_dir(paths: &SoldrPaths) -> Result<std::path::PathBuf, SoldrError> {
    let zccache_dir = normalize_path_for_compare(&soldr_cache::zccache_dir(paths))?;
    let inherited_soldr_managed_dir =
        non_empty_env_path(soldr_cache::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR)
            .map(|path| normalize_path_for_compare(&path))
            .transpose()?;
    if let Some(explicit) = non_empty_env_path(soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR) {
        let explicit = normalize_path_for_compare(&explicit)?;
        if explicit != zccache_dir && inherited_soldr_managed_dir.as_ref() != Some(&explicit) {
            return Err(SoldrError::Other(format!(
                "{} is managed by soldr for managed zccache builds. Unset it, set SOLDR_CACHE_DIR to choose soldr's cache root, or set SOLDR_RUSTC_WRAPPER to use a custom wrapper.",
                soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR
            )));
        }
    }
    Ok(zccache_dir)
}

pub(crate) fn normalize_path_for_compare(path: &std::path::Path) -> Result<std::path::PathBuf, SoldrError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

pub(crate) fn run_zccache_command_in_cache_dir(
    binary: &std::path::Path,
    args: &[&str],
    cache_dir: &std::path::Path,
) -> Result<CommandOutput, SoldrError> {
    run_zccache_command_with_env(
        binary,
        args,
        &[(
            soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR,
            cache_dir.as_os_str(),
        )],
    )
}

pub(crate) fn run_zccache_command_strings_in_cache_dir(
    binary: &std::path::Path,
    args: &[String],
    cache_dir: &std::path::Path,
) -> Result<CommandOutput, SoldrError> {
    let output = run_zccache_command_raw_strings_with_env(
        binary,
        args,
        &[(
            soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR,
            cache_dir.as_os_str(),
        )],
    )?;
    if !output.status.success() {
        return Err(SoldrError::Other(zccache_command_failure_message_strings(
            args, &output,
        )));
    }

    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
    })
}

fn start_zccache_with_recovery(
    binary: &std::path::Path,
    cache_dir: &std::path::Path,
) -> Result<(), SoldrError> {
    let start = run_zccache_command_raw_in_cache_dir(binary, &["start"], cache_dir)?;
    if start.status.success() {
        return Ok(());
    }

    let initial_stderr = command_stderr(&start);
    if !is_stale_zccache_daemon_start_failure(&initial_stderr) {
        return Err(SoldrError::Other(zccache_command_failure_message(
            &["start"],
            &start,
        )));
    }

    eprintln!(
        "soldr: zccache start reported an unresponsive daemon; stopping stale state and retrying"
    );
    let stop_diagnostic = match run_zccache_command_raw_in_cache_dir(binary, &["stop"], cache_dir) {
        Ok(stop) if stop.status.success() => None,
        Ok(stop) => Some(zccache_command_failure_message(&["stop"], &stop)),
        Err(err) => Some(format!("failed to invoke zccache stop: {err}")),
    };

    match run_zccache_command_raw_in_cache_dir(binary, &["start"], cache_dir) {
        Ok(retry) if retry.status.success() => Ok(()),
        Ok(retry) => {
            let mut message = format!(
                "zccache start failed after stale daemon recovery retry: {}",
                command_stderr(&retry)
            );
            message.push_str(&format!(
                "\ninitial zccache start failure: {}",
                initial_stderr
            ));
            if let Some(stop_diagnostic) = stop_diagnostic {
                message.push_str(&format!("\nzccache stop diagnostic: {stop_diagnostic}"));
            }
            Err(SoldrError::Other(message))
        }
        Err(err) => {
            let mut message =
                format!("failed to invoke zccache start during stale daemon recovery retry: {err}");
            message.push_str(&format!(
                "\ninitial zccache start failure: {}",
                initial_stderr
            ));
            if let Some(stop_diagnostic) = stop_diagnostic {
                message.push_str(&format!("\nzccache stop diagnostic: {stop_diagnostic}"));
            }
            Err(SoldrError::Other(message))
        }
    }
}

fn is_stale_zccache_daemon_start_failure(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("not accepting connections")
        || (stderr.contains("daemon process") && stderr.contains("exists"))
}

pub(crate) fn run_zccache_command_raw_in_cache_dir(
    binary: &std::path::Path,
    args: &[&str],
    cache_dir: &std::path::Path,
) -> Result<std::process::Output, SoldrError> {
    run_zccache_command_raw_with_env(
        binary,
        args,
        &[(
            soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR,
            cache_dir.as_os_str(),
        )],
    )
}

fn run_zccache_command_with_env(
    binary: &std::path::Path,
    args: &[&str],
    envs: &[(&str, &std::ffi::OsStr)],
) -> Result<CommandOutput, SoldrError> {
    let output = run_zccache_command_raw_with_env(binary, args, envs)?;
    if !output.status.success() {
        return Err(SoldrError::Other(zccache_command_failure_message(
            args, &output,
        )));
    }

    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
    })
}

fn run_zccache_command_raw_with_env(
    binary: &std::path::Path,
    args: &[&str],
    envs: &[(&str, &std::ffi::OsStr)],
) -> Result<std::process::Output, SoldrError> {
    let mut command = std::process::Command::new(binary);
    command.args(args);
    for &(name, value) in envs {
        command.env(name, value);
    }
    suppress_windows_console_window(&mut command);
    Ok(command.output()?)
}

fn run_zccache_command_raw_strings_with_env(
    binary: &std::path::Path,
    args: &[String],
    envs: &[(&str, &std::ffi::OsStr)],
) -> Result<std::process::Output, SoldrError> {
    let mut command = std::process::Command::new(binary);
    command.args(args);
    for &(name, value) in envs {
        command.env(name, value);
    }
    suppress_windows_console_window(&mut command);
    Ok(command.output()?)
}

fn zccache_command_failure_message(args: &[&str], output: &std::process::Output) -> String {
    format!(
        "zccache {} failed: {}",
        args.join(" "),
        command_stderr(output)
    )
}

fn zccache_command_failure_message_strings(
    args: &[String],
    output: &std::process::Output,
) -> String {
    format!(
        "zccache {} failed: {}",
        args.join(" "),
        command_stderr(output)
    )
}

pub(crate) fn command_stderr(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("exit status {}", output.status)
    } else {
        stderr
    }
}

fn toolchain_binary_override(tool: &str) -> Option<std::path::PathBuf> {
    let env_var = match tool {
        "cargo" => TEST_CARGO_BIN_ENV_VAR,
        "rustc" => TEST_RUSTC_BIN_ENV_VAR,
        _ => return real_toolchain_binary_override(tool),
    };
    non_empty_env_path(env_var).or_else(|| real_toolchain_binary_override(tool))
}

fn real_toolchain_binary_override(tool: &str) -> Option<std::path::PathBuf> {
    non_empty_env_path(&real_toolchain_binary_env_var(tool))
}

fn real_toolchain_binary_env_var(tool: &str) -> String {
    let mut value = String::from(REAL_TOOLCHAIN_BINARY_ENV_PREFIX);
    for ch in tool.chars() {
        if ch.is_ascii_alphanumeric() {
            value.push(ch.to_ascii_uppercase());
        } else {
            value.push('_');
        }
    }
    value
}

pub(crate) fn rustup_binary() -> std::path::PathBuf {
    non_empty_env_path(TEST_RUSTUP_BIN_ENV_VAR).unwrap_or_else(|| "rustup".into())
}

fn zccache_binary_override() -> Option<std::path::PathBuf> {
    non_empty_env_path(TEST_ZCCACHE_BIN_ENV_VAR)
        .or_else(|| non_empty_env_path(soldr_cache::ZCCACHE_BINARY_ENV_VAR))
}

pub(crate) fn non_empty_env_path(env_var: &str) -> Option<std::path::PathBuf> {
    let value = std::env::var_os(env_var)?;
    if value.is_empty() {
        return None;
    }
    Some(value.into())
}

fn current_soldr_binary() -> Result<std::path::PathBuf, SoldrError> {
    std::env::current_exe().map_err(SoldrError::from)
}

async fn fetch_managed_zccache(paths: &SoldrPaths) -> Result<soldr_fetch::FetchResult, SoldrError> {
    if let Some(binary_path) = non_empty_env_path(TEST_ZCCACHE_BIN_ENV_VAR) {
        return Ok(soldr_fetch::FetchResult {
            binary_path,
            version: soldr_fetch::MANAGED_ZCCACHE_VERSION.to_string(),
            cached: true,
        });
    }

    soldr_fetch::fetch_zccache_with_paths(paths).await
}

pub(crate) fn cached_managed_zccache(
    paths: &SoldrPaths,
) -> Result<Option<soldr_fetch::FetchResult>, SoldrError> {
    if let Some(binary_path) = non_empty_env_path(TEST_ZCCACHE_BIN_ENV_VAR) {
        return Ok(Some(soldr_fetch::FetchResult {
            binary_path,
            version: soldr_fetch::MANAGED_ZCCACHE_VERSION.to_string(),
            cached: true,
        }));
    }

    soldr_fetch::cached_zccache_binary(paths)
}

#[cfg(test)]
mod tests;
