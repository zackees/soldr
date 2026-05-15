use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use soldr_core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use soldr_fetch::VersionSpec;
use std::collections::BTreeSet;
use std::io::Write;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

mod linker;
mod self_relocate;

const TEST_CARGO_BIN_ENV_VAR: &str = "SOLDR_TEST_CARGO_BIN";
const TEST_RUSTC_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTC_BIN";
const TEST_RUSTUP_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTUP_BIN";
const TEST_ZCCACHE_BIN_ENV_VAR: &str = "SOLDR_TEST_ZCCACHE_BIN";
const TEST_FREE_DISK_BYTES_ENV_VAR: &str = "SOLDR_TEST_FREE_DISK_BYTES";
/// Overrides the default `nightly` toolchain used by `soldr gc cargo`
/// to invoke cargo's unstable `-Zgc clean gc`. The `--toolchain` flag
/// on `gc cargo` / `gc sweep` always takes precedence.
const SOLDR_GC_CARGO_TOOLCHAIN_ENV_VAR: &str = "SOLDR_GC_CARGO_TOOLCHAIN";
const JSON_SCHEMA_VERSION: u32 = 1;
const LOW_DISK_WARNING_THRESHOLD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const RUSTC_WRAPPER_OVERRIDE_ENV_VAR: &str = "SOLDR_RUSTC_WRAPPER";
const CARGO_PROFILE_DEV_DEBUG_ENV_VAR: &str = "CARGO_PROFILE_DEV_DEBUG";
const CARGO_PROFILE_TEST_DEBUG_ENV_VAR: &str = "CARGO_PROFILE_TEST_DEBUG";
/// Picks the linker injected for `soldr cargo ...` builds. See `linker` module
/// and `docs/API.md` for the supported values (`default | ld | mold | rust-lld
/// | fast`).
const LINKER_ENV_VAR: &str = "SOLDR_LINKER";
const REAL_TOOLCHAIN_BINARY_ENV_PREFIX: &str = "SOLDR_REAL_";
const TARGET_CACHE_MODE_ENV_VAR: &str = "SOLDR_TARGET_CACHE_MODE";
const TARGET_CACHE_BUNDLE_DIR_ENV_VAR: &str = "SOLDR_TARGET_CACHE_BUNDLE_DIR";
const TARGET_CACHE_BACKEND_ENV_VAR: &str = "SOLDR_TARGET_CACHE_BACKEND";
/// Selects which thin-slice pruning policy `soldr cargo` ships to zccache. See
/// `docs/THIN_TARGET_CACHE_PRUNING.md` for the rationale and rollout plan.
/// Values: `thin-v1` (legacy, default — keeps `.rlib`/`.rmeta`/proc-macro
/// outputs as a safety net) and `thin-v2` (fingerprint-aware aggressive prune;
/// drops library bytes and lets zccache's compilation cache repopulate them).
const TARGET_CACHE_PROFILE_ENV_VAR: &str = "SOLDR_TARGET_CACHE_PROFILE";
/// Reader-thread count for the target-cache tar walk in zccache (issue #272).
/// Forwarded to the `zccache rust-plan save/restore` subprocess via inherited
/// environment; soldr validates the value early so typos fail before cargo
/// metadata runs. Values: `auto` (default; zccache picks a vCPU-bounded count
/// capped at 8), `1` (disable parallelism — sequential tar walk), or any
/// positive integer for an explicit thread count. The actual parallel walk
/// lives in zccache; this constant exists so soldr can reject malformed values
/// at the front door.
const TARGET_CACHE_TAR_THREADS_ENV_VAR: &str = "SOLDR_TARGET_CACHE_TAR_THREADS";
/// Filename of the file-list manifest written next to the thin-slice bundle so
/// downstream tooling can prove what landed in the slice without unpacking it.
const THIN_MANIFEST_FILENAME: &str = "manifest.v2.json";
/// Flag controlling the warm-restore short-circuit (issue #229). Default-on:
/// after a successful `rust-plan save` soldr writes a sentinel describing the
/// plan/job, and on the next `soldr cargo ...` invocation the matching
/// `rust-plan restore` is skipped if the sentinel proves the `target/` tree
/// is already in the exact state restore would produce. This preserves
/// Cargo's mtime-based fingerprints across split CI steps. Set to a falsy
/// value (`0` / `false` / `no` / `off` / empty, case-insensitive) to opt out;
/// unset or any other value keeps the short-circuit enabled.
const SKIP_WARM_RESTORE_ENV_VAR: &str = "SOLDR_RUST_PLAN_SKIP_WARM_RESTORE";
/// Filename of the sentinel written next to the thin-slice bundle root after
/// a successful `rust-plan save`. Read on the next invocation by
/// `should_skip_warm_restore` to decide whether `rust-plan restore` would be
/// a no-op-but-touches-mtimes operation against an already-warm `target/`.
const WARM_RESTORE_SENTINEL_FILENAME: &str = "last-save.json";
/// Maximum age of a warm-restore sentinel before it is treated as stale and
/// ignored. Five minutes comfortably covers a normal `cargo test --no-run`
/// followed by `cargo test` step pair on GitHub Actions while keeping the
/// short-circuit from kicking in on later, unrelated jobs.
const WARM_RESTORE_MAX_AGE_SECONDS: u64 = 5 * 60;

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
    /// Anything else is a tool to fetch and run
    #[command(external_subcommand)]
    External(Vec<String>),
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
struct GcCargoArgs {
    /// Report the plan and exit without invoking cargo.
    #[arg(long)]
    dry_run: bool,
    /// Override the nightly toolchain. Defaults to
    /// `$SOLDR_GC_CARGO_TOOLCHAIN` if set, else `nightly`.
    #[arg(long, value_name = "TOOLCHAIN")]
    toolchain: Option<String>,
    /// Forwarded directly to cargo `--max-src-age`.
    #[arg(long, value_name = "DURATION")]
    max_src_age: Option<String>,
    /// Forwarded directly to cargo `--max-crate-age`.
    #[arg(long, value_name = "DURATION")]
    max_crate_age: Option<String>,
    /// Forwarded directly to cargo `--max-index-age`.
    #[arg(long, value_name = "DURATION")]
    max_index_age: Option<String>,
    /// Forwarded directly to cargo `--max-git-co-age`.
    #[arg(long, value_name = "DURATION")]
    max_git_co_age: Option<String>,
    /// Forwarded directly to cargo `--max-git-db-age`.
    #[arg(long, value_name = "DURATION")]
    max_git_db_age: Option<String>,
    /// Forwarded directly to cargo `--max-download-age`.
    #[arg(long, value_name = "DURATION")]
    max_download_age: Option<String>,
    /// Forwarded directly to cargo `--max-src-size`.
    #[arg(long, value_name = "SIZE")]
    max_src_size: Option<String>,
    /// Forwarded directly to cargo `--max-crate-size`.
    #[arg(long, value_name = "SIZE")]
    max_crate_size: Option<String>,
    /// Forwarded directly to cargo `--max-git-size`.
    #[arg(long, value_name = "SIZE")]
    max_git_size: Option<String>,
    /// Forwarded directly to cargo `--max-download-size`.
    #[arg(long, value_name = "SIZE")]
    max_download_size: Option<String>,
    /// Emit the stable machine-facing JSON form for this command.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct GcSweepArgs {
    /// Delete every eligible target/ candidate without prompting (used
    /// when the orchestrator runs the soldr target purge stage).
    #[arg(long)]
    all: bool,
    /// Plan and report without deleting anything.
    #[arg(long)]
    dry_run: bool,
    /// Run cargo's `clean gc`. Default is on; pass `--no-cargo-gc` to
    /// skip cargo entirely. `--cargo-gc` is accepted but is the
    /// default.
    #[arg(long, conflicts_with = "no_cargo_gc")]
    cargo_gc: bool,
    /// Skip cargo's `clean gc` (e.g. on CI runners with no nightly).
    #[arg(long, conflicts_with = "cargo_gc")]
    no_cargo_gc: bool,
    /// After the standard pipeline, run cargo's `clean gc` again with
    /// tighter ages
    /// (`--max-src-age=7days --max-crate-age=14days --max-git-co-age=7days`).
    /// Floor: each value is clamped to `auto_gc.min_age_secs` before
    /// being forwarded.
    #[arg(long)]
    aggressive: bool,
    /// Emit the stable machine-facing JSON form for this command.
    #[arg(long)]
    json: bool,
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
            std::process::exit(run_toolchain_passthrough("rustc", &args)?);
        }
        Commands::Rustfmt { args } => {
            std::process::exit(run_toolchain_passthrough("rustfmt", &args)?);
        }
        Commands::ClippyDriver { args } => {
            std::process::exit(run_toolchain_passthrough("clippy-driver", &args)?);
        }
        Commands::Rustdoc { args } => {
            std::process::exit(run_toolchain_passthrough("rustdoc", &args)?);
        }
        Commands::RustGdb { args } => {
            std::process::exit(run_toolchain_passthrough("rust-gdb", &args)?);
        }
        Commands::RustLldb { args } => {
            std::process::exit(run_toolchain_passthrough("rust-lldb", &args)?);
        }
        Commands::RustAnalyzer { args } => {
            std::process::exit(run_toolchain_passthrough("rust-analyzer", &args)?);
        }
        Commands::Status { json } => {
            let output = collect_status_output(cache_enabled)?;
            if json {
                print_json(&output)?;
            } else {
                print_status_output(&output);
            }
        }
        Commands::Clean => {
            clear_zccache_cache()?;
        }
        Commands::Purge => {
            purge_soldr_cache()?;
        }
        Commands::Config => {
            println!("(config not yet implemented)");
        }
        Commands::Cache { json, command } => match command {
            Some(CacheSubcommand::Report { json: report_json }) => {
                run_cache_report_command(report_json || json)?;
            }
            None => {
                let output = collect_cache_output()?;
                if json {
                    print_json(&output)?;
                } else {
                    print_cache_output(&output);
                }
            }
        },
        Commands::Version { json } => {
            let output = version_output();
            if json {
                print_json(&output)?;
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
                }) => GcInvocation {
                    mode: GcMode::Purge { all },
                    older_than,
                    larger_than,
                    json,
                },
                Some(GcSubcommand::List { json }) => {
                    run_gc_list_command(json)?;
                    return Ok(());
                }
                Some(GcSubcommand::Cargo(args)) => {
                    run_gc_cargo_command(*args)?;
                    return Ok(());
                }
                Some(GcSubcommand::Locations { json }) => {
                    run_gc_locations_command(json)?;
                    return Ok(());
                }
                Some(GcSubcommand::Sweep(args)) => {
                    run_gc_sweep_command(*args)?;
                    return Ok(());
                }
                None => GcInvocation {
                    mode: GcMode::Summary,
                    older_than,
                    larger_than,
                    json,
                },
            };
            run_gc_command(invocation)?;
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

/// Run a rustup-managed toolchain binary with pass-through args.
fn run_toolchain_passthrough(tool: &str, args: &[String]) -> Result<i32, SoldrError> {
    let binary = resolve_toolchain_binary(tool)?;
    let mut command = std::process::Command::new(binary);
    command.args(args);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
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
        emit_startup_target_warning_if_due();
        // Best-effort auto-GC trigger (issue #323). Runs on a detached
        // background thread; never blocks the build.
        maybe_kick_auto_gc(&paths);
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

    let rust_plan = if let Some(session) = session.as_ref() {
        maybe_prepare_rust_artifact_plan(
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
        let probe_path = rust_plan
            .as_ref()
            .map(|plan| std::path::PathBuf::from(&plan.target_dir))
            .unwrap_or_else(|| cargo_disk_space_probe_path(args));
        maybe_emit_low_disk_warning(&probe_path);
    }
    if let Some(plan) = rust_plan.as_ref() {
        if let Some(reason) = should_skip_warm_restore(plan) {
            eprintln!("{reason}");
        } else {
            run_zccache_rust_plan(plan, "restore", false)?;
        }
    }

    let status = command.status()?;
    if status.success() {
        if let Some(plan) = rust_plan.as_ref() {
            run_zccache_rust_plan(plan, "save", true)?;
            write_warm_restore_sentinel(plan);
        }
    }
    if let Some(session) = session {
        finish_zccache_build(&session)?;
    }
    Ok(status.code().unwrap_or(1))
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoMetadataPackage>,
    workspace_members: Vec<String>,
    workspace_root: std::path::PathBuf,
    target_directory: std::path::PathBuf,
}

#[derive(Debug, Deserialize)]
struct CargoMetadataPackage {
    id: String,
    source: Option<String>,
}

/// Returns `true` when `cache_profile` should be omitted from the serialized
/// plan to keep wire compatibility with zccache builds that do not yet know
/// about the `cache_profile` field (e.g. v1.4.0, which uses
/// `#[serde(deny_unknown_fields)]` on `RustArtifactPlanV1`).
///
/// We keep the value in-memory so internal consumers can still branch on it,
/// but we hide it on the wire for everything except the `thin-v2` opt-in.
fn skip_legacy_cache_profile(value: &Option<&'static str>) -> bool {
    !matches!(value, Some("thin-v2"))
}

#[derive(Debug, Serialize)]
struct RustArtifactPlan {
    schema_version: u32,
    mode: String,
    /// Thin-slice pruning policy in effect, e.g. `thin-v1` (legacy) or
    /// `thin-v2` (fingerprint-aware prune). Only emitted on the wire when
    /// it carries new information (i.e. `thin-v2`). Omitted entirely for
    /// `thin-v1` and `mode == "full"` so zccache builds with
    /// `#[serde(deny_unknown_fields)]` (e.g. v1.4.0) can still parse the
    /// plan unchanged.
    #[serde(skip_serializing_if = "skip_legacy_cache_profile")]
    cache_profile: Option<&'static str>,
    workspace_root: String,
    target_dir: String,
    toolchain: RustToolchainIdentity,
    target_triple: String,
    profile: String,
    inputs: RustPlanInputs,
    packages: RustPlanPackages,
    allowed_artifact_classes: Vec<&'static str>,
    /// Categories soldr explicitly drops from the slice. zccache may use this
    /// to short-circuit walks for files it would otherwise consider keeping.
    /// Empty for legacy `thin-v1` and `full` modes — preserves backwards
    /// compatibility with zccache builds that do not yet understand it.
    /// Skipped from the JSON entirely when empty so older zccache builds
    /// with `#[serde(deny_unknown_fields)]` keep accepting the plan.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    dropped_artifact_classes: Vec<&'static str>,
    cache_schema_version: u32,
    journal_log_path: Option<String>,
}

#[derive(Debug, Serialize)]
struct RustToolchainIdentity {
    rustc: String,
    cargo: String,
    channel: String,
    host: String,
}

#[derive(Debug, Serialize)]
struct RustPlanInputs {
    features_hash: String,
    rustflags_hash: String,
    env_hash: String,
    lockfile_hash: String,
    cargo_config_hash: String,
    manifest_hashes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RustPlanPackages {
    selected_package_ids: Vec<String>,
    workspace_package_ids: Vec<String>,
    excluded_path_package_ids: Vec<String>,
}

struct RustArtifactPlanContext {
    path: std::path::PathBuf,
    zccache_binary: std::path::PathBuf,
    cache_dir: std::path::PathBuf,
    zccache_daemon_cache_dir: std::path::PathBuf,
    session_id: String,
    journal_path: std::path::PathBuf,
    backend: String,
    /// Active thin-slice pruning policy. Only `Some` for thin modes; `None`
    /// for `full` so the manifest emitter can short-circuit.
    cache_profile: Option<&'static str>,
    /// Stable digest over the plan inputs (toolchain, lockfile, manifests,
    /// features, env, cargo config, target triple, profile, packages). Used
    /// by the warm-restore sentinel (issue #229) to prove that a previous
    /// step in the same job left `target/` in the exact state `restore`
    /// would produce, so the next `restore` can be skipped.
    plan_inputs_hash: String,
    /// Absolute target dir from the active plan, mirrored into the warm-
    /// restore sentinel so step 2 can verify it is being asked to restore
    /// into the same tree step 1 saved.
    target_dir: String,
}

fn maybe_prepare_rust_artifact_plan(
    cargo: &std::path::Path,
    rustc: &std::path::Path,
    args: &[String],
    session: &ZccacheBuildSession,
    cargo_profile_debug_default: Option<&CargoProfileDebugDefault>,
) -> Result<Option<RustArtifactPlanContext>, SoldrError> {
    let Some(mode) = rust_artifact_cache_mode_from_env()? else {
        return Ok(None);
    };

    if matches!(first_cargo_subcommand(args), Some("install")) {
        eprintln!("soldr: rust artifact cache plan skipped for cargo install");
        return Ok(None);
    }

    let profile = if mode == "thin" {
        Some(rust_artifact_cache_profile_from_env()?)
    } else {
        None
    };

    // Reject a malformed SOLDR_TARGET_CACHE_TAR_THREADS before we kick off
    // cargo metadata. zccache also validates, but failing here keeps the
    // error close to the user's typo and avoids spending seconds resolving
    // the workspace just to die on a one-character env mistake.
    rust_artifact_cache_tar_threads_from_env()?;

    let metadata = cargo_metadata(cargo, args)?;
    let toolchain = rust_toolchain_identity(cargo, rustc)?;
    let plan = build_rust_artifact_plan(
        &metadata,
        &toolchain,
        args,
        &mode,
        profile,
        session,
        cargo_profile_debug_default,
    )?;
    let plan_dir = session.cache_dir.join("plans");
    std::fs::create_dir_all(&plan_dir)?;
    let plan_path = plan_dir.join("last-rust-artifact-plan.json");
    let plan_json = serde_json::to_string_pretty(&plan)
        .map_err(|e| SoldrError::Other(format!("failed to serialize Rust artifact plan: {e}")))?;
    std::fs::write(&plan_path, plan_json)?;

    let plan_inputs_hash = compute_plan_inputs_hash(&plan);
    let target_dir = plan.target_dir.clone();

    Ok(Some(RustArtifactPlanContext {
        path: plan_path,
        zccache_binary: session.binary_path.clone(),
        cache_dir: rust_artifact_plan_cache_dir(session)?,
        zccache_daemon_cache_dir: session.cache_dir.clone(),
        session_id: session.session_id.clone(),
        journal_path: session.journal_path.clone(),
        backend: rust_artifact_cache_backend_from_env()?,
        cache_profile: profile,
        plan_inputs_hash,
        target_dir,
    }))
}

/// Stable digest summarising every plan field cargo would consult to decide
/// whether the cached `target/` tree is still valid. Used by the warm-restore
/// sentinel (issue #229) to prove that an in-job repeat of `soldr cargo ...`
/// is asking to restore into the same tree it just saved.
///
/// We hash a tuple of (toolchain identity, target triple, profile, mode,
/// cache profile, plan inputs, package selection) rather than the whole
/// `RustArtifactPlan` so the sentinel does not falsely diverge on cosmetic
/// fields (`schema_version`, `journal_log_path`, etc.).
fn compute_plan_inputs_hash(plan: &RustArtifactPlan) -> String {
    let payload = serde_json::json!({
        "toolchain": {
            "rustc": plan.toolchain.rustc,
            "cargo": plan.toolchain.cargo,
            "channel": plan.toolchain.channel,
            "host": plan.toolchain.host,
        },
        "target_triple": plan.target_triple,
        "profile": plan.profile,
        "mode": plan.mode,
        "cache_profile": plan.cache_profile,
        "inputs": {
            "features_hash": plan.inputs.features_hash,
            "rustflags_hash": plan.inputs.rustflags_hash,
            "env_hash": plan.inputs.env_hash,
            "lockfile_hash": plan.inputs.lockfile_hash,
            "cargo_config_hash": plan.inputs.cargo_config_hash,
            "manifest_hashes": plan.inputs.manifest_hashes,
        },
        "packages": {
            "selected_package_ids": plan.packages.selected_package_ids,
            "workspace_package_ids": plan.packages.workspace_package_ids,
            "excluded_path_package_ids": plan.packages.excluded_path_package_ids,
        },
        "allowed_artifact_classes": plan.allowed_artifact_classes,
        "dropped_artifact_classes": plan.dropped_artifact_classes,
    });
    stable_hash_json(&payload)
}

/// Sentinel written by `soldr cargo ...` after a successful `rust-plan save`.
/// Read on the next invocation by [`should_skip_warm_restore`] to decide
/// whether the matching `restore` would be a no-op-but-touches-mtimes
/// operation against an already-warm `target/` tree.
///
/// All fields are required for a sentinel match; missing fields cause the
/// short-circuit to bail out and fall through to the normal restore.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct WarmRestoreSentinel {
    /// Format version; bump if any field semantics change so older sentinels
    /// are treated as stale.
    schema_version: u32,
    /// Hash from [`compute_plan_inputs_hash`]. Must match the current plan.
    plan_inputs_hash: String,
    /// Absolute target dir the previous save wrote into. Must match the
    /// current plan's target dir, since restore is per-target-dir.
    target_dir: String,
    /// `GITHUB_RUN_ID` at save time. Empty string outside Actions; matched
    /// strictly so the short-circuit does not leak between runs.
    github_run_id: String,
    /// `GITHUB_JOB` at save time. Same matching rule as run id.
    github_job: String,
    /// `GITHUB_RUN_ATTEMPT` at save time. Re-runs of the same job get a new
    /// attempt id, so a prior attempt's sentinel is correctly treated as
    /// stale.
    github_run_attempt: String,
    /// zccache session id from the saving invocation. Recorded for log
    /// correlation; not used in the match decision.
    session_id: String,
    /// Wall-clock seconds since the unix epoch at save time. Compared
    /// against [`WARM_RESTORE_MAX_AGE_SECONDS`] to bound how stale the
    /// sentinel may be.
    saved_at_unix_seconds: u64,
}

/// Returns the path of the warm-restore sentinel for this plan's bundle dir.
fn warm_restore_sentinel_path(plan: &RustArtifactPlanContext) -> std::path::PathBuf {
    plan.cache_dir.join(WARM_RESTORE_SENTINEL_FILENAME)
}

/// Returns whether the warm-restore short-circuit is enabled for this
/// invocation. The flag is default-on after #229 validation completed:
///
/// - Unset → `true` (default-on).
/// - Set to a falsy value (`0` / `false` / `no` / `off` / empty,
///   case-insensitive after trimming) → `false` (explicit opt-out).
/// - Set to any other value, including the historical truthy values
///   (`1` / `true` / `yes` / `on`) → `true`. Unrecognised values are
///   tolerated as "enabled" rather than silently disabling the feature.
fn warm_restore_skip_enabled() -> bool {
    match std::env::var(SKIP_WARM_RESTORE_ENV_VAR) {
        Ok(value) => {
            let trimmed = value.trim().to_ascii_lowercase();
            !matches!(trimmed.as_str(), "0" | "false" | "no" | "off" | "")
        }
        Err(_) => true,
    }
}

/// Read `name` from the environment, returning an empty string when absent
/// or the value cannot be UTF-8 decoded. Used to canonicalise the GitHub
/// Actions identifiers stored in the warm-restore sentinel so a
/// missing-vs-empty distinction does not produce false negatives.
fn env_string_or_empty(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// Returns the current wall-clock as seconds since the unix epoch, or `0`
/// if the system clock is before the epoch (which would only happen on a
/// badly-misconfigured host).
fn current_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Decide whether the current invocation can skip `rust-plan restore`
/// because a previous invocation in the same CI job already populated
/// `target/` with the exact contents restore would produce.
///
/// Returns `Some(reason)` when the restore should be skipped (caller
/// should log `reason` for operator visibility). Returns `None` when the
/// caller should proceed with the normal restore — either because the
/// short-circuit is disabled, the sentinel is missing/stale, or any of
/// the match fields disagree.
///
/// Pure function over its inputs so it can be unit-tested without touching
/// the filesystem; the IO-bound caller passes the loaded sentinel and
/// current env snapshot in.
/// Bundle of "current invocation" inputs that
/// [`evaluate_warm_restore_skip`] compares against a loaded sentinel.
///
/// Internal-only — exists solely to keep the function's argument count
/// under clippy's `too_many_arguments` threshold without changing
/// behavior. Field names mirror the previous parameter names so call
/// sites stay legible.
struct WarmRestoreSkipInputs<'a> {
    plan_inputs_hash: &'a str,
    plan_target_dir: &'a str,
    github_run_id: &'a str,
    github_job: &'a str,
    github_run_attempt: &'a str,
    now_unix_seconds: u64,
    max_age_seconds: u64,
}

fn evaluate_warm_restore_skip(
    sentinel: Option<&WarmRestoreSentinel>,
    inputs: &WarmRestoreSkipInputs<'_>,
) -> Option<String> {
    let sentinel = sentinel?;
    if sentinel.schema_version != 1 {
        return None;
    }
    if sentinel.plan_inputs_hash != inputs.plan_inputs_hash {
        return None;
    }
    if sentinel.target_dir != inputs.plan_target_dir {
        return None;
    }
    // CI scoping: only short-circuit when both invocations are inside the
    // same GitHub Actions run + job + attempt. Locally these are all empty
    // strings on both sides, which still matches and lets the local repro
    // benefit from the same path. The intent of the issue, however, is the
    // CI case — hence the explicit attempt scoping.
    if sentinel.github_run_id != inputs.github_run_id {
        return None;
    }
    if sentinel.github_job != inputs.github_job {
        return None;
    }
    if sentinel.github_run_attempt != inputs.github_run_attempt {
        return None;
    }
    let age = inputs
        .now_unix_seconds
        .saturating_sub(sentinel.saved_at_unix_seconds);
    if age > inputs.max_age_seconds {
        return None;
    }
    Some(format!(
        "soldr: skipping rust-plan restore; target dir {} was warmed by this job {} seconds ago (session {})",
        sentinel.target_dir, age, sentinel.session_id,
    ))
}

/// Filesystem-backed wrapper around [`evaluate_warm_restore_skip`]. Reads
/// the sentinel for this plan's bundle dir, gathers the current env, and
/// returns the skip reason when the short-circuit should fire.
///
/// Errors from sentinel parsing are deliberately swallowed (return `None`)
/// — a corrupt sentinel must never break the build, only forfeit the
/// optimisation.
fn should_skip_warm_restore(plan: &RustArtifactPlanContext) -> Option<String> {
    if !warm_restore_skip_enabled() {
        return None;
    }
    let sentinel_path = warm_restore_sentinel_path(plan);
    let raw = std::fs::read_to_string(&sentinel_path).ok()?;
    let sentinel: WarmRestoreSentinel = serde_json::from_str(&raw).ok()?;
    let github_run_id = env_string_or_empty("GITHUB_RUN_ID");
    let github_job = env_string_or_empty("GITHUB_JOB");
    let github_run_attempt = env_string_or_empty("GITHUB_RUN_ATTEMPT");
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &plan.plan_inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &github_run_id,
        github_job: &github_job,
        github_run_attempt: &github_run_attempt,
        now_unix_seconds: current_unix_seconds(),
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    evaluate_warm_restore_skip(Some(&sentinel), &inputs)
}

/// Persist the warm-restore sentinel after a successful `rust-plan save`.
/// Errors are downgraded to a warning so a sentinel write failure can never
/// break the build that just succeeded; the worst case is that the next
/// invocation does the normal restore (current behavior).
fn write_warm_restore_sentinel(plan: &RustArtifactPlanContext) {
    if !warm_restore_skip_enabled() {
        return;
    }
    let sentinel = WarmRestoreSentinel {
        schema_version: 1,
        plan_inputs_hash: plan.plan_inputs_hash.clone(),
        target_dir: plan.target_dir.clone(),
        github_run_id: env_string_or_empty("GITHUB_RUN_ID"),
        github_job: env_string_or_empty("GITHUB_JOB"),
        github_run_attempt: env_string_or_empty("GITHUB_RUN_ATTEMPT"),
        session_id: plan.session_id.clone(),
        saved_at_unix_seconds: current_unix_seconds(),
    };
    let sentinel_path = warm_restore_sentinel_path(plan);
    let json = match serde_json::to_string_pretty(&sentinel) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("soldr warning: failed to serialize warm-restore sentinel: {e}");
            return;
        }
    };
    if let Some(parent) = sentinel_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!(
                "soldr warning: failed to create warm-restore sentinel dir {}: {e}",
                parent.display()
            );
            return;
        }
    }
    if let Err(e) = std::fs::write(&sentinel_path, json) {
        eprintln!(
            "soldr warning: failed to write warm-restore sentinel at {}: {e}",
            sentinel_path.display()
        );
    }
}

fn rust_artifact_cache_mode_from_env() -> Result<Option<String>, SoldrError> {
    let raw = std::env::var(TARGET_CACHE_MODE_ENV_VAR).unwrap_or_default();
    let mode = raw.trim().to_ascii_lowercase();
    match mode.as_str() {
        "" | "off" | "false" | "0" | "no" => Ok(None),
        "hot" | "thin" => Ok(Some("thin".to_string())),
        "full" => Ok(Some("full".to_string())),
        _ => Err(SoldrError::Other(format!(
            "invalid {TARGET_CACHE_MODE_ENV_VAR} value {raw:?}; expected thin, full, or off"
        ))),
    }
}

fn rust_artifact_cache_profile_from_env() -> Result<&'static str, SoldrError> {
    let raw = std::env::var(TARGET_CACHE_PROFILE_ENV_VAR).unwrap_or_default();
    let profile = raw.trim().to_ascii_lowercase();
    match profile.as_str() {
        // Default preserves the legacy slice contents until the verification
        // job in `docs/THIN_TARGET_CACHE_PRUNING.md` Section 5 is green.
        "" | "thin-v1" => Ok("thin-v1"),
        "thin-v2" => Ok("thin-v2"),
        _ => Err(SoldrError::Other(format!(
            "invalid {TARGET_CACHE_PROFILE_ENV_VAR} value {raw:?}; expected thin-v1 or thin-v2"
        ))),
    }
}

fn rust_artifact_cache_backend_from_env() -> Result<String, SoldrError> {
    let raw = std::env::var(TARGET_CACHE_BACKEND_ENV_VAR).unwrap_or_else(|_| "auto".to_string());
    let backend = raw.trim().to_ascii_lowercase();
    match backend.as_str() {
        "" | "auto" => Ok("auto".to_string()),
        "local" => Ok("local".to_string()),
        "gha" => Ok("gha".to_string()),
        _ => Err(SoldrError::Other(format!(
            "invalid {TARGET_CACHE_BACKEND_ENV_VAR} value {raw:?}; expected auto, local, or gha"
        ))),
    }
}

fn rust_artifact_cache_tar_threads_from_env() -> Result<Option<String>, SoldrError> {
    parse_rust_artifact_cache_tar_threads(
        &std::env::var(TARGET_CACHE_TAR_THREADS_ENV_VAR).unwrap_or_default(),
    )
}

fn parse_rust_artifact_cache_tar_threads(raw: &str) -> Result<Option<String>, SoldrError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.eq_ignore_ascii_case("auto") {
        return Ok(Some("auto".to_string()));
    }
    match trimmed.parse::<u32>() {
        Ok(n) if n >= 1 => Ok(Some(n.to_string())),
        _ => Err(SoldrError::Other(format!(
            "invalid {TARGET_CACHE_TAR_THREADS_ENV_VAR} value {raw:?}; expected `auto` or a positive integer (use `1` to disable parallelism)"
        ))),
    }
}

fn rust_artifact_plan_cache_dir(
    session: &ZccacheBuildSession,
) -> Result<std::path::PathBuf, SoldrError> {
    let cache_dir = non_empty_env_path(TARGET_CACHE_BUNDLE_DIR_ENV_VAR)
        .unwrap_or_else(|| session.cache_dir.join("rust-plan-cache"));
    let cache_dir = normalize_path_for_compare(&cache_dir)?;
    std::fs::create_dir_all(&cache_dir)?;
    Ok(cache_dir)
}

fn cargo_metadata(cargo: &std::path::Path, args: &[String]) -> Result<CargoMetadata, SoldrError> {
    let mut command = std::process::Command::new(cargo);
    command.args(["metadata", "--format-version", "1"]);
    command.args(cargo_metadata_passthrough_args(args));
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    command.env_remove("MAKEFLAGS");
    command.env_remove("CARGO_MAKEFLAGS");

    let output = command.output()?;
    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "cargo metadata failed while preparing Rust artifact cache plan: {}",
            command_stderr(&output)
        )));
    }

    serde_json::from_slice(&output.stdout).map_err(|e| {
        SoldrError::Other(format!(
            "failed to parse cargo metadata while preparing Rust artifact cache plan: {e}"
        ))
    })
}

fn cargo_metadata_passthrough_args(args: &[String]) -> Vec<std::ffi::OsString> {
    let mut values = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        match arg.as_str() {
            "--locked" | "--offline" | "--frozen" | "--all-features" | "--no-default-features" => {
                values.push(arg.as_str().into())
            }
            "--manifest-path" | "--config" | "--features" | "--filter-platform" => {
                if let Some(value) = iter.next() {
                    values.push(arg.as_str().into());
                    values.push(value.as_str().into());
                }
            }
            _ => {
                for flag in [
                    "--manifest-path=",
                    "--config=",
                    "--features=",
                    "--filter-platform=",
                ] {
                    if arg.starts_with(flag) {
                        values.push(arg.as_str().into());
                    }
                }
            }
        }
    }
    values
}

fn rust_toolchain_identity(
    cargo: &std::path::Path,
    rustc: &std::path::Path,
) -> Result<RustToolchainIdentity, SoldrError> {
    let rustc_output = tool_output(rustc, &["-Vv"])?;
    let cargo_output = tool_output(cargo, &["--version"])?;
    let host = rustc_output
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap_or("unknown")
        .to_string();
    let channel = std::env::var("RUSTUP_TOOLCHAIN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            rustc_output
                .lines()
                .find_map(|line| line.strip_prefix("release: "))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());

    Ok(RustToolchainIdentity {
        rustc: rustc_output.trim().to_string(),
        cargo: cargo_output.trim().to_string(),
        channel,
        host,
    })
}

fn tool_output(tool: &std::path::Path, args: &[&str]) -> Result<String, SoldrError> {
    let mut command = std::process::Command::new(tool);
    command.args(args);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command.output()?;
    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "{} {} failed: {}",
            tool.display(),
            args.join(" "),
            command_stderr(&output)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn build_rust_artifact_plan(
    metadata: &CargoMetadata,
    toolchain: &RustToolchainIdentity,
    args: &[String],
    mode: &str,
    cache_profile: Option<&'static str>,
    session: &ZccacheBuildSession,
    cargo_profile_debug_default: Option<&CargoProfileDebugDefault>,
) -> Result<RustArtifactPlan, SoldrError> {
    let workspace_root = normalize_path_for_compare(&metadata.workspace_root)?;
    let target_dir = normalize_path_for_compare(&metadata.target_directory)?;
    let workspace_members: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    let mut selected_package_ids = Vec::new();
    let mut excluded_path_package_ids = Vec::new();

    for package in &metadata.packages {
        if workspace_members.contains(package.id.as_str()) {
            continue;
        }
        match package.source.as_deref() {
            Some(source) if source.starts_with("registry+") || source.starts_with("git+") => {
                selected_package_ids.push(package.id.clone());
            }
            _ => excluded_path_package_ids.push(package.id.clone()),
        }
    }

    selected_package_ids.sort();
    excluded_path_package_ids.sort();
    let mut workspace_package_ids = metadata.workspace_members.clone();
    workspace_package_ids.sort();

    let allowed = allowed_artifact_classes(mode, cache_profile);
    let dropped = dropped_artifact_classes(mode, cache_profile);
    let cache_schema_version = match cache_profile {
        Some("thin-v2") => 2,
        _ => 1,
    };

    Ok(RustArtifactPlan {
        schema_version: 1,
        mode: mode.to_string(),
        cache_profile,
        workspace_root: path_string(&workspace_root),
        target_dir: path_string(&target_dir),
        toolchain: RustToolchainIdentity {
            rustc: toolchain.rustc.clone(),
            cargo: toolchain.cargo.clone(),
            channel: toolchain.channel.clone(),
            host: toolchain.host.clone(),
        },
        target_triple: cargo_target_triple(args, &toolchain.host),
        profile: cargo_profile(args).to_string(),
        inputs: RustPlanInputs {
            features_hash: stable_hash_json(&cargo_feature_inputs(args)),
            rustflags_hash: stable_hash_json(&rustflags_inputs()),
            env_hash: stable_hash_json(&build_env_inputs(cargo_profile_debug_default)),
            lockfile_hash: file_hash_or_missing(&workspace_root.join("Cargo.lock"))?,
            cargo_config_hash: cargo_config_hash(&workspace_root)?,
            manifest_hashes: workspace_manifest_hashes(&workspace_root)?,
        },
        packages: RustPlanPackages {
            selected_package_ids,
            workspace_package_ids,
            excluded_path_package_ids,
        },
        allowed_artifact_classes: allowed,
        dropped_artifact_classes: dropped,
        cache_schema_version,
        journal_log_path: Some(path_string(&session.journal_path)),
    })
}

/// Artifact classes the thin-slice walker is permitted to copy into the bundle.
///
/// `thin-v1` (legacy) preserves the historical contents that ship `.rlib`/
/// `.rmeta`/proc-macro library bytes alongside the freshness inputs. This is
/// kept as the safety-net default while the in-CI verification job from
/// `docs/THIN_TARGET_CACHE_PRUNING.md` Section 5 is being rolled out.
///
/// `thin-v2` is the fingerprint-aware aggressive prune. It keeps only what
/// cargo actually consults to make a fresh-vs-rebuild decision (fingerprints,
/// dep-info, build-script `out_dir/` contents, small build-script metadata).
/// The dropped library bytes are reproduced on demand by zccache's compilation
/// cache when cargo asks rustc to rebuild the missing unit.
fn allowed_artifact_classes(mode: &str, cache_profile: Option<&'static str>) -> Vec<&'static str> {
    if mode == "full" {
        return Vec::new();
    }
    match cache_profile {
        Some("thin-v2") => vec![
            // Fingerprint metadata cargo reads to decide skip-vs-rebuild.
            // Split from the legacy `cargo_fingerprint` umbrella per
            // `docs/THIN_TARGET_CACHE_PRUNING.md` Section 4.3.
            "cargo_fingerprint_meta",
            "dep_info",
            "build_script_metadata",
            "build_script_output",
        ],
        // thin-v1 (default) and any unrecognized profile that arrived via a
        // future zccache that does not yet branch on `cache_profile` get the
        // legacy class list so behavior is unchanged on rollout day 0.
        _ => vec![
            "rlib",
            "rmeta",
            "dep_info",
            "proc_macro",
            "cargo_fingerprint",
            "build_script_metadata",
            "build_script_output",
        ],
    }
}

/// Artifact classes the thin-slice walker must explicitly skip in the active
/// profile. Surfaced to zccache so it can short-circuit walks for paths it
/// would otherwise copy. Returning the drop list as data (rather than baking
/// it into zccache) keeps the policy decision in soldr where the design
/// discussion already lives.
fn dropped_artifact_classes(mode: &str, cache_profile: Option<&'static str>) -> Vec<&'static str> {
    if mode == "full" {
        return Vec::new();
    }
    match cache_profile {
        Some("thin-v2") => vec![
            // Multi-GB rustc incremental DB. Churns per-commit, low CI hit
            // rate. Cargo never reads it to decide freshness.
            "incremental",
            // Compiled build-script binaries. Cheap to regenerate from
            // cached deps; bytes live in zccache's content store when needed.
            "build_script_build",
            // Library output bytes. zccache repopulates on rustc miss.
            "rlib",
            "rmeta",
            // proc-macro shared libraries. Same story as `.rlib`.
            "proc_macro",
            // Split debug-info / pdb / macOS dSYM bundles.
            "dwo",
            "pdb",
            "dsym",
            // The fingerprint *outputs* (not the metadata). The metadata is
            // tiny and load-bearing for freshness; the outputs are large.
            "cargo_fingerprint_outputs",
        ],
        _ => Vec::new(),
    }
}

fn run_zccache_rust_plan(
    plan: &RustArtifactPlanContext,
    operation: &'static str,
    include_session: bool,
) -> Result<(), SoldrError> {
    let plan_path = path_string(&plan.path);
    let cache_dir = path_string(&plan.cache_dir);
    let journal_path = path_string(&plan.journal_path);
    let mut args = vec![
        "rust-plan".to_string(),
        operation.to_string(),
        "--plan".to_string(),
        plan_path,
        "--json".to_string(),
        "--backend".to_string(),
        plan.backend.clone(),
        "--cache-dir".to_string(),
        cache_dir,
        "--journal".to_string(),
        journal_path,
    ];
    if include_session {
        args.push("--session-id".to_string());
        args.push(plan.session_id.clone());
    }

    let output = run_zccache_command_strings_in_cache_dir(
        &plan.zccache_binary,
        &args,
        &plan.zccache_daemon_cache_dir,
    )?;
    let stdout = output.stdout.trim();
    if !stdout.is_empty() {
        eprintln!("soldr: zccache rust-plan {operation} summary");
        eprintln!("{stdout}");
        if operation == "restore" {
            warn_if_rust_plan_restore_incomplete(stdout);
        }
    }
    if operation == "save" && plan.cache_profile == Some("thin-v2") {
        if let Err(e) = write_thin_manifest(&plan.cache_dir, plan.cache_profile) {
            // Manifest emission is diagnostic; never fail the build because
            // we could not write it. Log so it shows up in CI logs.
            eprintln!(
                "soldr warning: failed to write thin-slice manifest at {}: {e}",
                plan.cache_dir.display()
            );
        }
    }
    Ok(())
}

/// Schema for `<thin-root>/manifest.v2.json`.
///
/// Written by soldr after `zccache rust-plan save` produces the bundle.
/// Downstream tooling (e.g. setup-soldr verification jobs) reads this to
/// prove what landed in the slice without unpacking it.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ThinSliceManifest {
    /// Manifest format version. `2` for the file-list manifest produced by
    /// the `thin-v2` profile.
    schema_version: u32,
    /// Active thin-slice pruning policy when this manifest was written.
    cache_profile: String,
    /// Absolute path of the bundle root the entries are relative to.
    bundle_root: String,
    /// Timestamp of manifest emission, RFC 3339 / seconds since epoch.
    generated_at_unix_seconds: u64,
    /// Every file in the bundle, sorted by relative path for stable diffs.
    files: Vec<ThinSliceManifestEntry>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ThinSliceManifestEntry {
    /// Path relative to `bundle_root`, forward-slashed for cross-platform diffability.
    path: String,
    /// File size in bytes. Optional because broken symlinks etc. may not
    /// have a usable size; serialized as `null` rather than skipped so the
    /// shape is uniform across entries.
    size_bytes: Option<u64>,
}

fn write_thin_manifest(
    bundle_root: &std::path::Path,
    cache_profile: Option<&'static str>,
) -> Result<(), SoldrError> {
    let profile = cache_profile.unwrap_or("thin-v1").to_string();
    if !bundle_root.exists() {
        // Nothing to manifest; skip rather than spamming an empty file.
        return Ok(());
    }
    let manifest = build_thin_manifest(bundle_root, &profile)?;
    let manifest_path = bundle_root.join(THIN_MANIFEST_FILENAME);
    let json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| SoldrError::Other(format!("failed to serialize thin-slice manifest: {e}")))?;
    std::fs::write(&manifest_path, json)?;
    Ok(())
}

fn build_thin_manifest(
    bundle_root: &std::path::Path,
    cache_profile: &str,
) -> Result<ThinSliceManifest, SoldrError> {
    let thread_count = resolve_bundle_walk_thread_count(
        &std::env::var(TARGET_CACHE_TAR_THREADS_ENV_VAR).unwrap_or_default(),
    )?;
    let mut files = walk_bundle_files(bundle_root, thread_count)?;
    // Drop any prior manifest so the file list does not chase its own tail
    // across repeated saves into the same bundle directory.
    files.retain(|entry| entry.path != THIN_MANIFEST_FILENAME);
    // Sort so the manifest is byte-identical regardless of walk order
    // (sequential vs parallel must produce the same output).
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let generated_at_unix_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(ThinSliceManifest {
        schema_version: 2,
        cache_profile: cache_profile.to_string(),
        bundle_root: path_string(bundle_root),
        generated_at_unix_seconds,
        files,
    })
}

/// Cap on the number of metadata-stat threads soldr will spin up for the
/// bundle walk, matching the documented zccache cap. Past ~8 threads the
/// per-file `GetFileInformation` syscall stops being the bottleneck (the
/// directory iteration becomes one), so additional workers just contend.
const BUNDLE_WALK_THREAD_CAP: usize = 8;

/// Resolve the reader-thread count for the bundle walk from a raw
/// `SOLDR_TARGET_CACHE_TAR_THREADS` value.
///
/// The env var has already been validated by
/// [`parse_rust_artifact_cache_tar_threads`] at the cargo front door, so
/// the raw input here is expected to be well-formed. We re-validate
/// defensively because [`build_thin_manifest`] can also run on the bare
/// `RUSTC_WRAPPER` passthrough path that does not flow through the front
/// door check.
///
/// Returns:
/// - `None` for `auto` / unset — use rayon's global pool, capped at the
///   smaller of the system parallelism and [`BUNDLE_WALK_THREAD_CAP`].
/// - `Some(1)` to force the sequential fallback (no rayon overhead).
/// - `Some(n)` for an explicit thread count, clamped to
///   `[1, BUNDLE_WALK_THREAD_CAP]`.
fn resolve_bundle_walk_thread_count(raw: &str) -> Result<Option<usize>, SoldrError> {
    let parsed = parse_rust_artifact_cache_tar_threads(raw)?;
    let Some(token) = parsed else {
        // Unset → auto.
        return Ok(None);
    };
    if token == "auto" {
        return Ok(None);
    }
    // parse_rust_artifact_cache_tar_threads already rejected zero / negative /
    // non-integer values, so an integer-or-bust parse here is sound.
    let n: usize = token.parse().map_err(|_| {
        SoldrError::Other(format!(
            "invalid {TARGET_CACHE_TAR_THREADS_ENV_VAR} value {raw:?}; expected `auto` or a positive integer (use `1` to disable parallelism)"
        ))
    })?;
    Ok(Some(n.clamp(1, BUNDLE_WALK_THREAD_CAP)))
}

/// Walk every file under `root` and return one [`ThinSliceManifestEntry`]
/// per regular file.
///
/// Implementation is two-phase:
/// 1. Serial directory traversal (`read_dir`) collects every file path. Per
///    `read_dir` is cheap; the per-entry cost is dominated by the metadata
///    stat in phase 2 (which on Windows pays a Defender callback per file).
/// 2. Parallel `std::fs::metadata` over the collected paths via rayon.
///    Output order is non-deterministic — the caller MUST sort.
///
/// `thread_count`:
/// - `None` → use rayon's global thread pool.
/// - `Some(1)` → fully sequential (no rayon overhead at all).
/// - `Some(n)` for `n > 1` → run inside a scoped thread pool of `n`
///   workers so the env var actually controls something soldr-side.
fn walk_bundle_files(
    root: &std::path::Path,
    thread_count: Option<usize>,
) -> Result<Vec<ThinSliceManifestEntry>, SoldrError> {
    // Phase 1: serial DFS collects (absolute_path, relative_string) pairs.
    let mut paths: Vec<(std::path::PathBuf, String)> = Vec::new();
    collect_bundle_file_paths(root, root, &mut paths)?;

    // Phase 2: stat each file. Sequential when only one worker is wanted,
    // rayon-parallel otherwise.
    let stat = |(path, rel): &(std::path::PathBuf, String)| -> ThinSliceManifestEntry {
        let size_bytes = std::fs::metadata(path).ok().map(|m| m.len());
        ThinSliceManifestEntry {
            path: rel.clone(),
            size_bytes,
        }
    };

    let files = match thread_count {
        Some(1) => paths.iter().map(stat).collect(),
        Some(n) => {
            use rayon::prelude::*;
            // Build a scoped pool so the explicit thread count actually
            // bounds this walk instead of leaking onto rayon's global pool.
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(n)
                .thread_name(|i| format!("soldr-bundle-walk-{i}"))
                .build()
                .map_err(|e| {
                    SoldrError::Other(format!("failed to build bundle-walk thread pool: {e}"))
                })?;
            pool.install(|| paths.par_iter().map(stat).collect())
        }
        None => {
            use rayon::prelude::*;
            paths.par_iter().map(stat).collect()
        }
    };

    Ok(files)
}

/// Recursively walk `dir`, pushing `(absolute_path, root-relative string)`
/// for every regular file under it. Used by [`walk_bundle_files`] as the
/// directory-iteration phase before per-file metadata stats are fanned out.
fn collect_bundle_file_paths(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(std::path::PathBuf, String)>,
) -> Result<(), SoldrError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(SoldrError::from(e)),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_bundle_file_paths(root, &path, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|_| path.clone());
            let rel_string = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((path, rel_string));
        }
    }
    Ok(())
}

fn warn_if_rust_plan_restore_incomplete(stdout: &str) {
    let Ok(summary) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return;
    };
    let absent = summary
        .get("artifact_absent_from_restored_plan")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if absent == 0 {
        return;
    }
    let restored = summary
        .get("restored_file_count")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".to_string());
    eprintln!(
        "soldr warning: rust-plan restore is partial \
         (artifact_absent_from_restored_plan={absent}, restored_file_count={restored}); \
         Cargo is likely to fail with missing .rmeta errors. This usually means two \
         `soldr cargo build` invocations are sharing the same --target-dir. Use a \
         distinct --target-dir for each build or clear the target directory before \
         re-running. See https://github.com/zackees/soldr/issues/228 for context."
    );
}

fn cargo_profile(args: &[String]) -> &str {
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

fn cargo_target_triple(args: &[String], host: &str) -> String {
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

fn cargo_feature_inputs(args: &[String]) -> Vec<String> {
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

fn selected_cargo_args(args: &[String], names: &[&str]) -> Vec<String> {
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

fn rustflags_inputs() -> Vec<(String, String)> {
    sorted_env_vars(|name| {
        name == "RUSTFLAGS"
            || name == "CARGO_ENCODED_RUSTFLAGS"
            || (name.starts_with("CARGO_TARGET_") && name.ends_with("_RUSTFLAGS"))
    })
}

fn build_env_inputs(
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

fn workspace_manifest_hashes(workspace_root: &std::path::Path) -> Result<Vec<String>, SoldrError> {
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

fn cargo_config_hash(workspace_root: &std::path::Path) -> Result<String, SoldrError> {
    let mut inputs = Vec::new();
    for relative in [".cargo/config.toml", ".cargo/config"] {
        let path = workspace_root.join(relative);
        if path.exists() {
            inputs.push(format!("{relative}:{}", file_hash_or_missing(&path)?));
        }
    }
    Ok(stable_hash_json(&inputs))
}

fn file_hash_or_missing(path: &std::path::Path) -> Result<String, SoldrError> {
    if !path.exists() {
        return Ok("missing".to_string());
    }
    Ok(sha256_bytes(&std::fs::read(path)?))
}

fn stable_hash_json<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    sha256_bytes(&bytes)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn path_string(path: &std::path::Path) -> String {
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
struct CargoProfileDebugDefault {
    profile: &'static str,
    env_var: &'static str,
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

fn available_space(path: &std::path::Path) -> std::io::Result<u64> {
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

fn existing_filesystem_probe_path(path: &std::path::Path) -> std::path::PathBuf {
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
fn first_cargo_subcommand(args: &[String]) -> Option<&str> {
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

fn resolve_toolchain_binary(tool: &str) -> Result<std::path::PathBuf, SoldrError> {
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

fn apply_implicit_toolchain_homes(command: &mut std::process::Command) {
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

struct ZccacheBuildSession {
    binary_path: std::path::PathBuf,
    cache_dir: std::path::PathBuf,
    session_id: String,
    session_log_path: std::path::PathBuf,
    journal_path: std::path::PathBuf,
    session_stats_path: std::path::PathBuf,
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

fn clear_zccache_cache() -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    let mut cleared_anything = false;

    if let Some(fetch) = cached_managed_zccache(&paths)? {
        let _ = run_zccache_command_in_cache_dir(&fetch.binary_path, &["clear"], &zccache_dir)?;
        println!("cleared zccache artifact cache");
        cleared_anything = true;
    }
    if zccache_dir.exists() {
        std::fs::remove_dir_all(&zccache_dir)?;
        println!("removed soldr zccache state dir: {}", zccache_dir.display());
        cleared_anything = true;
    }
    if !cleared_anything {
        println!(
            "managed zccache {} not fetched yet",
            soldr_fetch::MANAGED_ZCCACHE_VERSION
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// soldr gc — garbage-collect stale Cargo target/ directories.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum GcMode {
    Summary,
    Purge { all: bool },
}

struct GcInvocation {
    mode: GcMode,
    older_than: String,
    larger_than: String,
    json: bool,
}

#[derive(Serialize)]
struct GcCandidateOutput {
    path: String,
    size_bytes: u64,
    size_human: String,
    age_seconds: i64,
    age_human: String,
    eligible: bool,
    reason: Option<String>,
}

#[derive(Serialize)]
struct GcOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    dry_run: bool,
    registry_path: String,
    candidate_count: usize,
    skipped_count: usize,
    total_reclaimable_bytes: u64,
    total_reclaimable_human: String,
    candidates: Vec<GcCandidateOutput>,
    largest_candidates: Vec<GcCandidateOutput>,
    skipped: Vec<GcCandidateOutput>,
    dropped_missing: usize,
    deleted_paths: Vec<String>,
    selected_count: usize,
    succeeded_count: usize,
    failed_count: usize,
    reclaimed_bytes: u64,
    reclaimed_human: String,
    error_log_path: Option<String>,
}

fn run_gc_command(invocation: GcInvocation) -> Result<(), SoldrError> {
    use soldr_cache::gc::{
        cleanup_old_gc_logs, parse_duration, parse_size, scan, write_gc_error_log, GcOptions,
        GcPurgeSummary,
    };

    let older_than = parse_duration(&invocation.older_than).map_err(SoldrError::Other)?;
    let larger_than = parse_size(&invocation.larger_than).map_err(SoldrError::Other)?;
    let purge_all = match invocation.mode {
        GcMode::Summary => false,
        GcMode::Purge { all } => all,
    };
    let is_summary = matches!(invocation.mode, GcMode::Summary);

    let paths = SoldrPaths::new()?;
    let dev_roots = resolve_gc_dev_roots(&paths);
    let db_path = soldr_cache::data_db_path(&paths);
    let registry = soldr_cache::target_registry::TargetRegistry::open(&db_path)
        .map_err(|e| SoldrError::Other(format!("failed to open soldr registry: {e}")))?;
    let gc_log_dir = soldr_cache::gc_log_dir(&paths);
    cleanup_old_gc_logs(&gc_log_dir)
        .map_err(|e| SoldrError::Other(format!("failed to clean old gc logs: {e}")))?;

    let options = GcOptions {
        older_than_seconds: older_than,
        larger_than_bytes: larger_than,
        dev_roots,
        dry_run: is_summary,
    };

    let report =
        scan(&registry, &options).map_err(|e| SoldrError::Other(format!("gc scan failed: {e}")))?;
    let total_reclaimable_bytes = gc_total_reclaimable_bytes(&report.candidates);

    let mut deleted_paths: Vec<String> = Vec::new();
    let mut purge_summary = GcPurgeSummary::default();
    let mut error_log_path: Option<std::path::PathBuf> = None;

    if is_summary {
        if !invocation.json {
            print_gc_summary(&db_path, &report, total_reclaimable_bytes);
        }
    } else if !invocation.json {
        print_gc_purge_scan(&db_path, &report, total_reclaimable_bytes);
    }

    if !is_summary {
        purge_summary =
            run_gc_purge_candidates(&registry, &report.candidates, purge_all, invocation.json)?;
        deleted_paths = purge_summary
            .deleted_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        if !purge_summary.failures.is_empty() {
            let args = std::env::args().collect::<Vec<_>>();
            let path = write_gc_error_log(&gc_log_dir, &args, &purge_summary.failures)
                .map_err(|e| SoldrError::Other(format!("failed to write gc error log: {e}")))?;
            error_log_path = Some(path);
        }
        if !invocation.json {
            print_gc_purge_result(&purge_summary, error_log_path.as_deref());
        }
    }

    if invocation.json {
        let output = GcOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "gc",
            mode: if is_summary { "summary" } else { "purge" },
            dry_run: is_summary,
            registry_path: db_path.display().to_string(),
            candidate_count: report.candidates.len(),
            skipped_count: report.skipped.len(),
            total_reclaimable_bytes,
            total_reclaimable_human: soldr_cache::target_registry::human_size(
                total_reclaimable_bytes,
            ),
            largest_candidates: gc_largest_candidates(&report.candidates, 5)
                .into_iter()
                .map(gc_candidate_output)
                .collect(),
            candidates: report
                .candidates
                .into_iter()
                .map(gc_candidate_output)
                .collect(),
            skipped: report
                .skipped
                .into_iter()
                .map(gc_candidate_output)
                .collect(),
            dropped_missing: report.dropped_missing,
            deleted_paths,
            selected_count: purge_summary.selected_count,
            succeeded_count: purge_summary.succeeded_count,
            failed_count: purge_summary.failed_count,
            reclaimed_bytes: purge_summary.reclaimed_bytes,
            reclaimed_human: soldr_cache::target_registry::human_size(
                purge_summary.reclaimed_bytes,
            ),
            error_log_path: error_log_path.map(|p| p.display().to_string()),
        };
        print_json(&output)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct GcListEntryOutput {
    path: String,
    last_used_unix: i64,
    age_seconds: i64,
    age_human: String,
    size_bytes: u64,
    size_human: String,
    file_count: u64,
}

#[derive(Serialize)]
struct GcListOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    registry_path: String,
    entry_count: usize,
    pruned_missing: usize,
    entries: Vec<GcListEntryOutput>,
}

fn absolute_path_string(path: &std::path::Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// Compute `(size_bytes, file_count)` for a directory using rayon to
/// fan out across the top-level entries. The per-entry walk is the
/// existing sequential routine. This keeps the implementation small
/// while exploiting the typical cargo `target/` layout where the bulk
/// of bytes sit under a handful of subdirs (`debug/`, `release/`,
/// per-target triples, etc.).
fn fast_directory_size_and_files(path: &std::path::Path) -> (u64, u64) {
    use rayon::prelude::*;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return (0, 0),
    };
    if metadata.file_type().is_symlink() {
        return (0, 0);
    }
    if metadata.is_file() {
        return (metadata.len(), 1);
    }
    let entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(path) {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => return (0, 0),
    };
    entries
        .into_par_iter()
        .map(|entry| {
            let entry_path = entry.path();
            let entry_meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => return (0u64, 0u64),
            };
            if entry_meta.file_type().is_symlink() {
                (0, 0)
            } else if entry_meta.is_dir() {
                soldr_cache::target_registry::directory_size_and_files(&entry_path)
            } else if entry_meta.is_file() {
                (entry_meta.len(), 1)
            } else {
                (0, 0)
            }
        })
        .reduce(
            || (0u64, 0u64),
            |a, b| (a.0.saturating_add(b.0), a.1.saturating_add(b.1)),
        )
}

fn run_gc_list_command(json: bool) -> Result<(), SoldrError> {
    use rayon::prelude::*;

    let paths = SoldrPaths::new()?;
    let db_path = soldr_cache::data_db_path(&paths);
    let registry = soldr_cache::target_registry::TargetRegistry::open(&db_path)
        .map_err(|e| SoldrError::Other(format!("failed to open soldr registry: {e}")))?;
    let rows = registry
        .list()
        .map_err(|e| SoldrError::Other(format!("gc list failed: {e}")))?;
    let now = soldr_cache::target_registry::current_unix_seconds()
        .map_err(|e| SoldrError::Other(format!("gc list clock error: {e}")))?;

    // Partition rows into those still on disk and those that have
    // disappeared since the registry was written. Missing rows are
    // never reported — they're swept out of the registry at the end
    // via a single batched delete.
    let (live_rows, missing_paths): (Vec<_>, Vec<_>) = rows.into_par_iter().partition_map(|row| {
        if row.path.exists() {
            rayon::iter::Either::Left(row)
        } else {
            rayon::iter::Either::Right(row.path)
        }
    });

    let entries: Vec<GcListEntryOutput> = live_rows
        .into_par_iter()
        .map(|row| {
            let (size_bytes, file_count) = fast_directory_size_and_files(&row.path);
            let age_seconds = now.saturating_sub(row.last_used);
            GcListEntryOutput {
                path: absolute_path_string(&row.path),
                last_used_unix: row.last_used,
                age_seconds,
                age_human: soldr_cache::target_registry::human_age(age_seconds),
                size_bytes,
                size_human: soldr_cache::target_registry::human_size(size_bytes),
                file_count,
            }
        })
        .collect();

    let pruned_missing = registry
        .remove_many(&missing_paths)
        .map_err(|e| SoldrError::Other(format!("failed to prune missing registry rows: {e}")))?;

    if json {
        let output = GcListOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "gc",
            mode: "list",
            registry_path: db_path.display().to_string(),
            entry_count: entries.len(),
            pruned_missing,
            entries,
        };
        print_json(&output)?;
    } else {
        println!("soldr gc list: registry: {}", db_path.display());
        println!(
            "soldr gc list: {} tracked target dir{}",
            entries.len(),
            if entries.len() == 1 { "" } else { "s" }
        );
        for entry in &entries {
            println!(
                "  {}  size={}  files={}  age={}",
                entry.path, entry.size_human, entry.file_count, entry.age_human,
            );
        }
        if pruned_missing > 0 {
            println!(
                "soldr gc list: pruned {pruned_missing} missing row{} from registry",
                if pruned_missing == 1 { "" } else { "s" }
            );
        }
    }
    Ok(())
}

fn run_gc_purge_candidates(
    registry: &soldr_cache::target_registry::TargetRegistry,
    candidates: &[soldr_cache::gc::GcCandidate],
    purge_all: bool,
    json: bool,
) -> Result<soldr_cache::gc::GcPurgeSummary, SoldrError> {
    let worker_count = gc_purge_worker_count();
    let (job_tx, job_rx) = mpsc::channel::<soldr_cache::gc::GcCandidate>();
    let (result_tx, result_rx) = mpsc::channel();
    let job_rx = Arc::new(Mutex::new(job_rx));
    let mut workers = Vec::new();
    for idx in 0..worker_count {
        let job_rx = Arc::clone(&job_rx);
        let result_tx = result_tx.clone();
        let builder = std::thread::Builder::new().name(format!("soldr-gc-{idx}"));
        workers.push(
            builder
                .spawn(move || loop {
                    let next = {
                        let rx = job_rx.lock().expect("gc worker channel poisoned");
                        rx.recv()
                    };
                    match next {
                        Ok(candidate) => {
                            let _ =
                                result_tx.send(soldr_cache::gc::delete_candidate_dir(candidate));
                        }
                        Err(_) => break,
                    }
                })
                .map_err(|e| SoldrError::Other(format!("failed to start gc worker: {e}")))?,
        );
    }
    drop(result_tx);

    let mut selected_count = 0usize;
    let mut completed_count = 0usize;
    let mut outcomes = Vec::new();

    for cand in candidates {
        let should_delete = purge_all || prompt_gc_purge_candidate(cand);
        if !should_delete {
            continue;
        }

        selected_count += 1;
        job_tx
            .send(cand.clone())
            .map_err(|e| SoldrError::Other(format!("failed to queue gc delete: {e}")))?;
    }
    drop(job_tx);

    while completed_count < selected_count {
        match result_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(outcome) => {
                completed_count += 1;
                outcomes.push(outcome);
                if !json {
                    print_gc_purge_progress(completed_count, selected_count);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !json {
                    print_gc_purge_progress(completed_count, selected_count);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    for worker in workers {
        worker
            .join()
            .map_err(|_| SoldrError::Other("gc worker panicked".to_string()))?;
    }

    if !json && selected_count > 0 {
        eprintln!();
    }

    soldr_cache::gc::apply_purge_outcomes(registry, outcomes)
        .map_err(|e| SoldrError::Other(format!("failed to update gc registry: {e}")))
}

fn gc_purge_worker_count() -> usize {
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    gc_purge_worker_count_for(available)
}

fn gc_purge_worker_count_for(available_parallelism: usize) -> usize {
    available_parallelism.clamp(1, 4)
}

fn print_gc_purge_progress(completed: usize, selected: usize) {
    eprint!("\rsoldr gc purge: deleting selected targets {completed}/{selected}");
    let _ = std::io::stderr().flush();
}

fn gc_total_reclaimable_bytes(candidates: &[soldr_cache::gc::GcCandidate]) -> u64 {
    candidates.iter().map(|c| c.size_bytes).sum()
}

fn print_gc_summary(
    db_path: &std::path::Path,
    report: &soldr_cache::gc::GcReport,
    total_reclaimable_bytes: u64,
) {
    println!("soldr gc: registry: {}", db_path.display());
    println!(
        "soldr gc: eligible: {} target dir{}; reclaimable: {}",
        report.candidates.len(),
        if report.candidates.len() == 1 {
            ""
        } else {
            "s"
        },
        soldr_cache::target_registry::human_size(total_reclaimable_bytes)
    );
    println!(
        "soldr gc: skipped: {}; dropped missing rows: {}",
        report.skipped.len(),
        report.dropped_missing
    );

    if report.candidates.is_empty() {
        println!("soldr gc: nothing to reclaim.");
    } else {
        println!("soldr gc: largest eligible target directories:");
        for cand in gc_largest_candidates(&report.candidates, 5) {
            println!(
                "  {}  size={}  last_used={}",
                cand.path.display(),
                soldr_cache::target_registry::human_size(cand.size_bytes),
                soldr_cache::target_registry::human_age(cand.age_seconds),
            );
        }
        println!("Run 'soldr gc purge' to delete eligible target directories.");
    }
}

fn print_gc_purge_scan(
    db_path: &std::path::Path,
    report: &soldr_cache::gc::GcReport,
    total_reclaimable_bytes: u64,
) {
    eprintln!(
        "soldr gc purge: scanned registry at {} ({} candidate dir{}, {} skipped, {} dropped missing, {} reclaimable)",
        db_path.display(),
        report.candidates.len(),
        if report.candidates.len() == 1 { "" } else { "s" },
        report.skipped.len(),
        report.dropped_missing,
        soldr_cache::target_registry::human_size(total_reclaimable_bytes)
    );

    if report.candidates.is_empty() {
        eprintln!("soldr gc purge: nothing to delete.");
    } else {
        eprintln!("soldr gc purge: candidates");
        for cand in &report.candidates {
            eprintln!(
                "  {}  size={}  age={}",
                cand.path.display(),
                soldr_cache::target_registry::human_size(cand.size_bytes),
                soldr_cache::target_registry::human_age(cand.age_seconds),
            );
        }
    }
}

fn print_gc_purge_result(
    summary: &soldr_cache::gc::GcPurgeSummary,
    error_log_path: Option<&std::path::Path>,
) {
    eprintln!(
        "soldr gc purge: selected {}; succeeded {}; failed {}; reclaimed {}",
        summary.selected_count,
        summary.succeeded_count,
        summary.failed_count,
        soldr_cache::target_registry::human_size(summary.reclaimed_bytes)
    );
    if let Some(path) = error_log_path {
        eprintln!(
            "soldr gc purge: detailed deletion errors written to {}",
            path.display()
        );
    }
}

fn gc_largest_candidates(
    candidates: &[soldr_cache::gc::GcCandidate],
    limit: usize,
) -> Vec<soldr_cache::gc::GcCandidate> {
    let mut largest = candidates.to_vec();
    largest.sort_by(|a, b| {
        b.size_bytes
            .cmp(&a.size_bytes)
            .then_with(|| a.path.cmp(&b.path))
    });
    largest.truncate(limit);
    largest
}

fn gc_candidate_output(c: soldr_cache::gc::GcCandidate) -> GcCandidateOutput {
    GcCandidateOutput {
        path: c.path.display().to_string(),
        size_human: soldr_cache::target_registry::human_size(c.size_bytes),
        size_bytes: c.size_bytes,
        age_human: soldr_cache::target_registry::human_age(c.age_seconds),
        age_seconds: c.age_seconds,
        eligible: c.eligible,
        reason: c.reason,
    }
}

fn prompt_gc_purge_candidate(cand: &soldr_cache::gc::GcCandidate) -> bool {
    prompt_yes_no_default_yes(&format!(
        "soldr gc: delete {} ({}, age {}) ? [Y/n] ",
        cand.path.display(),
        soldr_cache::target_registry::human_size(cand.size_bytes),
        soldr_cache::target_registry::human_age(cand.age_seconds),
    ))
}

fn prompt_yes_no_default_yes(prompt: &str) -> bool {
    use std::io::{BufRead, Write};
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return false;
    }
    parse_gc_purge_answer(&line)
}

fn parse_gc_purge_answer(input: &str) -> bool {
    matches!(input.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes")
}

/// Resolve the configured `gc.allowlist_roots`, falling back to
/// `~/dev` when unset.
fn resolve_gc_dev_roots(paths: &SoldrPaths) -> Vec<std::path::PathBuf> {
    let config = paths.load_config();
    let configured = config
        .gc
        .allowlist_roots
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| !r.trim().is_empty())
        .map(|r| soldr_core::expand_user_home(&r))
        .collect::<Vec<_>>();
    if !configured.is_empty() {
        return configured;
    }
    if let Ok(home) = soldr_core::user_home_dir() {
        return vec![home.join("dev")];
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// soldr gc cargo / gc locations / gc sweep — issue #323 manual surface.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct GcCargoOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    toolchain: String,
    exit_code: i32,
    dry_run: bool,
    args: Vec<String>,
    stdout_bytes: u64,
    stderr_bytes: u64,
    skipped: bool,
    skipped_reason: Option<String>,
}

#[derive(Serialize)]
struct GcLocationOutput {
    kind: &'static str,
    path: String,
    exists: bool,
    size_bytes: u64,
    size_human: String,
    file_count: u64,
    owner: &'static str,
    purge_safety: &'static str,
}

#[derive(Serialize)]
struct GcLocationsOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    locations: Vec<GcLocationOutput>,
    total_size_bytes: u64,
    total_size_human: String,
}

#[derive(Serialize)]
struct GcSweepOutput {
    schema_version: u32,
    command: &'static str,
    mode: &'static str,
    dry_run: bool,
    cargo_gc: Option<GcCargoOutput>,
    cargo_gc_aggressive: Option<GcCargoOutput>,
    soldr_targets: Option<SoldrTargetsSummary>,
    locations: Vec<GcLocationOutput>,
    elapsed_ms: u128,
}

#[derive(Serialize, Default)]
struct SoldrTargetsSummary {
    selected_count: usize,
    succeeded_count: usize,
    failed_count: usize,
    reclaimed_bytes: u64,
    reclaimed_human: String,
}

/// `soldr gc cargo` — shell out to nightly cargo's `-Zgc clean gc`.
fn run_gc_cargo_command(args: GcCargoArgs) -> Result<(), SoldrError> {
    let outcome = invoke_cargo_native_gc(&args, false)?;
    if args.json {
        print_json(&outcome)?;
    }
    if outcome.skipped {
        // Explicit gc cargo treats a missing nightly as a hard error,
        // unlike gc sweep which downgrades the missing toolchain to a
        // skip. We surfaced the skip JSON above for callers that care,
        // but the exit code must reflect the failure.
        return Err(SoldrError::Other(
            outcome
                .skipped_reason
                .unwrap_or_else(|| "cargo nightly GC unavailable".into()),
        ));
    }
    if outcome.exit_code != 0 {
        std::process::exit(outcome.exit_code);
    }
    Ok(())
}

/// Common implementation backing `gc cargo` and the cargo-step of
/// `gc sweep`. When `skip_when_missing` is true (sweep), a missing
/// nightly toolchain returns a `skipped = true` outcome with
/// `exit_code = 0` so the orchestrator can continue.
fn invoke_cargo_native_gc(
    args: &GcCargoArgs,
    skip_when_missing: bool,
) -> Result<GcCargoOutput, SoldrError> {
    let toolchain = resolve_gc_cargo_toolchain(args.toolchain.as_deref());

    let mut forwarded: Vec<String> = Vec::new();
    push_optional_flag(&mut forwarded, "--max-src-age", args.max_src_age.as_deref());
    push_optional_flag(
        &mut forwarded,
        "--max-crate-age",
        args.max_crate_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-index-age",
        args.max_index_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-git-co-age",
        args.max_git_co_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-git-db-age",
        args.max_git_db_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-download-age",
        args.max_download_age.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-src-size",
        args.max_src_size.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-crate-size",
        args.max_crate_size.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-git-size",
        args.max_git_size.as_deref(),
    );
    push_optional_flag(
        &mut forwarded,
        "--max-download-size",
        args.max_download_size.as_deref(),
    );
    if args.dry_run {
        forwarded.push("--dry-run".to_string());
    }

    // Final `cargo` argv: -Zgc clean gc [forwarded...]
    let mut cargo_argv: Vec<String> =
        vec!["-Zgc".to_string(), "clean".to_string(), "gc".to_string()];
    cargo_argv.extend(forwarded.iter().cloned());

    // We invoke via `rustup run <toolchain> cargo ...` so the
    // workspace's rust-toolchain.toml override does not silently win.
    if !rustup_run_available_for(&toolchain) {
        if skip_when_missing {
            return Ok(GcCargoOutput {
                schema_version: JSON_SCHEMA_VERSION,
                command: "gc",
                mode: "cargo",
                toolchain: toolchain.clone(),
                exit_code: 0,
                dry_run: args.dry_run,
                args: cargo_argv,
                stdout_bytes: 0,
                stderr_bytes: 0,
                skipped: true,
                skipped_reason: Some(format!(
                    "rustup toolchain {toolchain} not installed; skipping cargo GC"
                )),
            });
        }
        return Err(SoldrError::Other(format!(
            "rustup toolchain {toolchain} not installed; install it with `rustup toolchain install {toolchain}` or pass --toolchain <name>"
        )));
    }

    let mut command = std::process::Command::new("rustup");
    command.arg("run").arg(&toolchain).arg("cargo");
    command.args(&cargo_argv);
    soldr_core::suppress_windows_console_window(&mut command);

    eprintln!(
        "soldr gc cargo: rustup run {toolchain} cargo {}",
        cargo_argv.join(" ")
    );

    let output = command.output().map_err(|e| {
        SoldrError::Other(format!(
            "failed to invoke rustup run {toolchain} cargo: {e}"
        ))
    })?;

    // Stream cargo's output through to the user's terminal.
    use std::io::Write as _;
    let _ = std::io::stdout().write_all(&output.stdout);
    let _ = std::io::stderr().write_all(&output.stderr);

    Ok(GcCargoOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "gc",
        mode: "cargo",
        toolchain,
        exit_code: output.status.code().unwrap_or(1),
        dry_run: args.dry_run,
        args: cargo_argv,
        stdout_bytes: output.stdout.len() as u64,
        stderr_bytes: output.stderr.len() as u64,
        skipped: false,
        skipped_reason: None,
    })
}

fn push_optional_flag(out: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(v) = value {
        out.push(format!("{flag}={v}"));
    }
}

fn resolve_gc_cargo_toolchain(flag: Option<&str>) -> String {
    flag.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var(SOLDR_GC_CARGO_TOOLCHAIN_ENV_VAR)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "nightly".to_string())
}

/// Best-effort probe: `rustup toolchain list` must list the supplied
/// channel. We avoid actually shelling into the toolchain here because
/// `rustup run <missing> cargo --version` will install on demand,
/// which we don't want for a probe.
fn rustup_run_available_for(toolchain: &str) -> bool {
    let mut command = std::process::Command::new("rustup");
    command.args(["toolchain", "list"]);
    soldr_core::suppress_windows_console_window(&mut command);
    let output = match command.output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .any(|line| line.trim().starts_with(toolchain))
}

/// `soldr gc locations` — read-only enumeration of every cache dir
/// soldr cares about, with sizes (no last-used derivation yet).
fn run_gc_locations_command(json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let entries = enumerate_cache_locations(&paths);
    let total_size_bytes: u64 = entries.iter().map(|e| e.size_bytes).sum();

    if json {
        let output = GcLocationsOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "gc",
            mode: "locations",
            total_size_bytes,
            total_size_human: soldr_cache::target_registry::human_size(total_size_bytes),
            locations: entries,
        };
        print_json(&output)?;
    } else {
        println!("soldr gc locations:");
        println!(
            "  total tracked size: {}",
            soldr_cache::target_registry::human_size(total_size_bytes)
        );
        for entry in &entries {
            println!(
                "  [{:>8}] {}  size={}  files={}  owner={}  purge_safety={}",
                entry.kind,
                entry.path,
                entry.size_human,
                entry.file_count,
                entry.owner,
                entry.purge_safety,
            );
        }
    }
    Ok(())
}

/// Enumerate every directory soldr cares about: cargo home subdirs,
/// rustup home subdirs, soldr's own cache root, and the state.redb
/// file. Missing paths are reported with `exists = false` and zero
/// size so the JSON shape stays predictable.
fn enumerate_cache_locations(paths: &SoldrPaths) -> Vec<GcLocationOutput> {
    let mut entries: Vec<GcLocationOutput> = Vec::new();

    if let Some(cargo_home) = soldr_core::resolve_cargo_home() {
        for (kind, suffix, owner, purge_safety) in &[
            ("cargo_registry_src", "registry/src", "cargo", "regenerable"),
            (
                "cargo_registry_cache",
                "registry/cache",
                "cargo",
                "regenerable",
            ),
            (
                "cargo_registry_index",
                "registry/index",
                "cargo",
                "regenerable",
            ),
            ("cargo_git_db", "git/db", "cargo", "regenerable"),
            (
                "cargo_git_checkouts",
                "git/checkouts",
                "cargo",
                "regenerable",
            ),
        ] {
            let path = cargo_home.join(suffix);
            entries.push(gc_location_for(kind, &path, owner, purge_safety));
        }
        // .global-cache is a single file in cargo's stable tree.
        let global_cache = cargo_home.join(".global-cache");
        entries.push(gc_location_for(
            "cargo_global_cache",
            &global_cache,
            "cargo",
            "regenerable",
        ));
    }

    if let Some(rustup_home) = soldr_core::resolve_rustup_home() {
        entries.push(gc_location_for(
            "rustup_toolchains",
            &rustup_home.join("toolchains"),
            "rustup",
            "user_action",
        ));
        entries.push(gc_location_for(
            "rustup_update_hashes",
            &rustup_home.join("update-hashes"),
            "rustup",
            "regenerable",
        ));
    }

    entries.push(gc_location_for(
        "soldr_cache",
        &paths.cache,
        "soldr",
        "regenerable",
    ));
    entries.push(gc_location_for(
        "soldr_state_db",
        &paths.root.join("state.redb"),
        "soldr",
        "user_action",
    ));

    entries
}

fn gc_location_for(
    kind: &'static str,
    path: &std::path::Path,
    owner: &'static str,
    purge_safety: &'static str,
) -> GcLocationOutput {
    let exists = path.exists();
    let (size_bytes, file_count) = if exists {
        fast_directory_size_and_files(path)
    } else {
        (0, 0)
    };
    GcLocationOutput {
        kind,
        path: path.display().to_string(),
        exists,
        size_bytes,
        size_human: soldr_cache::target_registry::human_size(size_bytes),
        file_count,
        owner,
        purge_safety,
    }
}

/// `soldr gc sweep` — orchestrate locations + cargo gc + soldr target
/// purge in one go.
fn run_gc_sweep_command(args: GcSweepArgs) -> Result<(), SoldrError> {
    let start = std::time::Instant::now();
    let paths = SoldrPaths::new()?;
    let cargo_gc_enabled = !args.no_cargo_gc;

    // 1. Locations table (always — read-only).
    let locations = enumerate_cache_locations(&paths);

    // 2. Cargo's clean gc with conservative ages (unless disabled).
    let cargo_gc_outcome = if cargo_gc_enabled {
        // Use conservative defaults — let cargo's own ~1mo / ~3mo
        // policy decide. We don't pass any --max-*-age flags so the
        // user can configure cargo independently.
        let cargo_args = GcCargoArgs {
            dry_run: args.dry_run,
            toolchain: None,
            max_src_age: None,
            max_crate_age: None,
            max_index_age: None,
            max_git_co_age: None,
            max_git_db_age: None,
            max_download_age: None,
            max_src_size: None,
            max_crate_size: None,
            max_git_size: None,
            max_download_size: None,
            json: args.json,
        };
        Some(invoke_cargo_native_gc(&cargo_args, true)?)
    } else {
        None
    };

    // 3. soldr's target purge over registered workspaces.
    let soldr_targets = if args.dry_run {
        if !args.json {
            eprintln!("soldr gc sweep: dry-run; skipping soldr target purge");
        }
        None
    } else {
        Some(run_soldr_target_purge_for_sweep(
            &paths, args.all, args.json,
        )?)
    };

    // 4. Aggressive second cargo pass.
    let cargo_gc_aggressive = if args.aggressive && cargo_gc_enabled {
        let cfg = paths.load_config();
        let floor = cfg.auto_gc.min_age_secs;
        let aggressive_args = aggressive_cargo_args(args.json, args.dry_run, floor);
        Some(invoke_cargo_native_gc(&aggressive_args, true)?)
    } else {
        None
    };

    if !args.json {
        eprintln!("soldr gc sweep: done in {} ms", start.elapsed().as_millis());
    }

    if args.json {
        let output = GcSweepOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "gc",
            mode: "sweep",
            dry_run: args.dry_run,
            cargo_gc: cargo_gc_outcome,
            cargo_gc_aggressive,
            soldr_targets,
            locations,
            elapsed_ms: start.elapsed().as_millis(),
        };
        print_json(&output)?;
    }
    Ok(())
}

fn aggressive_cargo_args(json: bool, dry_run: bool, min_age_secs: u64) -> GcCargoArgs {
    // Helper: clamp `aggressive_days * 86_400` to the configured min
    // age. Express the result back in seconds (cargo accepts `s` /
    // `secs` / `seconds`).
    let clamp = |days: u64| -> String {
        let secs =
            soldr_cache::auto_gc::clamp_age_to_floor(days.saturating_mul(86_400), min_age_secs);
        format!("{secs}secs")
    };
    GcCargoArgs {
        dry_run,
        toolchain: None,
        max_src_age: Some(clamp(7)),
        max_crate_age: Some(clamp(14)),
        max_index_age: None,
        max_git_co_age: Some(clamp(7)),
        max_git_db_age: None,
        max_download_age: None,
        max_src_size: None,
        max_crate_size: None,
        max_git_size: None,
        max_download_size: None,
        json,
    }
}

fn run_soldr_target_purge_for_sweep(
    _paths: &SoldrPaths,
    purge_all: bool,
    json: bool,
) -> Result<SoldrTargetsSummary, SoldrError> {
    use soldr_cache::gc::{parse_duration, parse_size, scan, GcOptions};
    let paths = SoldrPaths::new()?;
    let dev_roots = resolve_gc_dev_roots(&paths);
    let db_path = soldr_cache::data_db_path(&paths);
    let registry = soldr_cache::target_registry::TargetRegistry::open(&db_path)
        .map_err(|e| SoldrError::Other(format!("failed to open soldr registry: {e}")))?;

    let cfg = paths.load_config();
    let older_than_seconds = soldr_cache::auto_gc::clamp_age_to_floor(
        parse_duration("10d").map_err(SoldrError::Other)?,
        cfg.auto_gc.min_age_secs,
    );
    let larger_than_bytes = parse_size("256M").map_err(SoldrError::Other)?;

    let options = GcOptions {
        older_than_seconds,
        larger_than_bytes,
        dev_roots,
        dry_run: false,
    };
    let report =
        scan(&registry, &options).map_err(|e| SoldrError::Other(format!("gc scan failed: {e}")))?;
    if report.candidates.is_empty() {
        return Ok(SoldrTargetsSummary::default());
    }
    let purge_summary = run_gc_purge_candidates(&registry, &report.candidates, purge_all, json)?;
    Ok(SoldrTargetsSummary {
        selected_count: purge_summary.selected_count,
        succeeded_count: purge_summary.succeeded_count,
        failed_count: purge_summary.failed_count,
        reclaimed_bytes: purge_summary.reclaimed_bytes,
        reclaimed_human: soldr_cache::target_registry::human_size(purge_summary.reclaimed_bytes),
    })
}

// ---------------------------------------------------------------------------
// Auto-GC under disk pressure (issue #323).
//
// Hook lives at the soldr cargo front door. On every cargo invocation
// the wrapper consults a throttle marker and, if the throttle has
// expired and the user hasn't opted out, spawns a detached background
// thread that:
//
//   1. enumerates soldr-relevant paths and groups them by volume;
//   2. probes free space per volume;
//   3. runs the tiered GC plan only against volumes below the trigger;
//   4. appends a structured line to ~/.soldr/logs/auto-gc.log.
//
// We deliberately spawn instead of running inline so the wrapper never
// blocks the build. cargo's `.package-cache` mutex handles concurrent
// invocations of `cargo clean gc` cleanly for us.
// ---------------------------------------------------------------------------

const AUTO_GC_THROTTLE_SECONDS: u64 = 5 * 60;
const AUTO_GC_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
const AUTO_GC_DISABLE_ENV_VAR: &str = "SOLDR_AUTO_GC_DISABLED";

fn maybe_kick_auto_gc(paths: &SoldrPaths) {
    if auto_gc_env_disabled() {
        return;
    }
    let config = paths.load_config().auto_gc;
    if !config.enabled {
        return;
    }
    let marker = soldr_cache::auto_gc_throttle_marker_path(paths);
    if !auto_gc_throttle_expired(&marker, AUTO_GC_THROTTLE_SECONDS) {
        return;
    }
    // Touch the marker before spawning so a crashing background thread
    // doesn't cause us to immediately rerun on the next invocation.
    let _ = touch_auto_gc_marker(&marker);

    let log_path = soldr_cache::auto_gc_log_path(paths);
    let paths_root = paths.root.clone();
    let _ = std::thread::Builder::new()
        .name("soldr-auto-gc".to_string())
        .spawn(move || {
            run_auto_gc_background(paths_root, log_path);
        });
}

fn auto_gc_env_disabled() -> bool {
    match std::env::var(AUTO_GC_DISABLE_ENV_VAR) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
        Err(_) => false,
    }
}

fn auto_gc_throttle_expired(marker: &std::path::Path, throttle_seconds: u64) -> bool {
    let Ok(meta) = std::fs::metadata(marker) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    let elapsed = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or(std::time::Duration::ZERO);
    elapsed.as_secs() >= throttle_seconds
}

fn touch_auto_gc_marker(marker: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(marker, "")
}

fn run_auto_gc_background(paths_root: std::path::PathBuf, log_path: std::path::PathBuf) {
    use soldr_cache::auto_gc::DiskFreeProbe as _;
    let start = std::time::Instant::now();
    let paths = SoldrPaths::with_root(paths_root);
    let config = paths.load_config().auto_gc;
    let (validated, warnings) = soldr_cache::auto_gc::validate_config(&config);
    for warning in &warnings {
        let _ = append_auto_gc_log_line(&log_path, &format!("warning: {warning}"));
    }

    let auto_paths = enumerate_auto_gc_paths(&paths);
    let probe = SystemVolumeProbe;
    let plans = soldr_cache::auto_gc::plan_auto_gc(&validated, &auto_paths, &probe, &probe);
    if plans.is_empty() {
        return; // Either disabled or no volume is below trigger.
    }

    for plan in &plans {
        let line = format!(
            "auto-gc volume={} free_gib={:.2} trigger_gib={} target_gib={} paths={} status=detected",
            plan.volume_key,
            (plan.free_bytes as f64) / (soldr_cache::auto_gc::GIB as f64),
            validated.trigger_free_gb,
            validated.target_free_gb,
            plan.paths.len()
        );
        let _ = append_auto_gc_log_line(&log_path, &line);

        // Tier 1: conservative cargo GC (no explicit --max-*-age flags
        // so cargo uses its own conservative defaults). Only attempt
        // when the volume holds the cargo home.
        let mut last_tier = 0u8;
        let cargo_volume_paths = plan
            .paths
            .iter()
            .filter(|p| matches!(p.kind, soldr_cache::auto_gc::AutoGcPathKind::CargoHome))
            .count();
        if cargo_volume_paths > 0 {
            let outcome = run_conservative_cargo_gc_background(&log_path);
            last_tier = 1;
            let _ = append_auto_gc_log_line(
                &log_path,
                &format!(
                    "tier=1 volume={} exit_code={} skipped={} reason={}",
                    plan.volume_key,
                    outcome.exit_code,
                    outcome.skipped,
                    outcome.reason.as_deref().unwrap_or("ran")
                ),
            );
        }

        // Re-probe and decide whether to escalate.
        let mut free_bytes = probe.free_bytes(&plan.paths[0].path).unwrap_or(0);
        let target_bytes = validated
            .target_free_gb
            .saturating_mul(soldr_cache::auto_gc::GIB);

        // Tier 2: soldr target purge (only if volume holds workspace
        // targets and we're still under target).
        if soldr_cache::auto_gc::next_tier(free_bytes, target_bytes, last_tier).is_some() {
            let workspace_targets: Vec<_> = plan
                .paths
                .iter()
                .filter(|p| {
                    matches!(
                        p.kind,
                        soldr_cache::auto_gc::AutoGcPathKind::WorkspaceTarget
                    )
                })
                .map(|p| p.path.clone())
                .collect();
            if !workspace_targets.is_empty() {
                let reclaimed = run_soldr_target_purge_background(
                    &paths,
                    &workspace_targets,
                    validated.min_age_secs,
                );
                last_tier = 2;
                free_bytes = probe.free_bytes(&plan.paths[0].path).unwrap_or(free_bytes);
                let _ = append_auto_gc_log_line(
                    &log_path,
                    &format!(
                        "tier=2 volume={} reclaimed_bytes={} free_gib={:.2}",
                        plan.volume_key,
                        reclaimed,
                        (free_bytes as f64) / (soldr_cache::auto_gc::GIB as f64),
                    ),
                );
            }
        }

        // Tier 3: aggressive cargo GC (clamped to min_age_secs).
        if soldr_cache::auto_gc::next_tier(free_bytes, target_bytes, last_tier).is_some()
            && cargo_volume_paths > 0
        {
            let ages = soldr_cache::auto_gc::TIER3_AGES.clamped_seconds(validated.min_age_secs);
            let outcome = run_aggressive_cargo_gc_background(&log_path, &ages);
            last_tier = 3;
            free_bytes = probe.free_bytes(&plan.paths[0].path).unwrap_or(free_bytes);
            let _ = append_auto_gc_log_line(
                &log_path,
                &format!(
                    "tier=3 volume={} exit_code={} skipped={} reason={} free_gib={:.2}",
                    plan.volume_key,
                    outcome.exit_code,
                    outcome.skipped,
                    outcome.reason.as_deref().unwrap_or("ran"),
                    (free_bytes as f64) / (soldr_cache::auto_gc::GIB as f64),
                ),
            );
        }

        if soldr_cache::auto_gc::next_tier(free_bytes, target_bytes, last_tier).is_none()
            && free_bytes < target_bytes
        {
            let _ = append_auto_gc_log_line(
                &log_path,
                &format!(
                    "auto-gc warning volume={} free_gib={:.2} target_gib={} \
                    tiers exhausted; run `soldr gc sweep --aggressive`",
                    plan.volume_key,
                    (free_bytes as f64) / (soldr_cache::auto_gc::GIB as f64),
                    validated.target_free_gb,
                ),
            );
        }
    }

    let _ = append_auto_gc_log_line(
        &log_path,
        &format!(
            "auto-gc done elapsed_ms={} volumes={}",
            start.elapsed().as_millis(),
            plans.len(),
        ),
    );
    let _ = rotate_auto_gc_log_if_needed(&log_path, AUTO_GC_LOG_MAX_BYTES);
}

struct AutoGcCargoOutcome {
    exit_code: i32,
    skipped: bool,
    reason: Option<String>,
}

fn run_conservative_cargo_gc_background(log_path: &std::path::Path) -> AutoGcCargoOutcome {
    let args = GcCargoArgs {
        dry_run: false,
        toolchain: None,
        max_src_age: None,
        max_crate_age: None,
        max_index_age: None,
        max_git_co_age: None,
        max_git_db_age: None,
        max_download_age: None,
        max_src_size: None,
        max_crate_size: None,
        max_git_size: None,
        max_download_size: None,
        json: true,
    };
    match invoke_cargo_native_gc(&args, true) {
        Ok(outcome) => AutoGcCargoOutcome {
            exit_code: outcome.exit_code,
            skipped: outcome.skipped,
            reason: outcome.skipped_reason,
        },
        Err(e) => {
            let _ = append_auto_gc_log_line(log_path, &format!("tier=1 invoke_error={e}"));
            AutoGcCargoOutcome {
                exit_code: 1,
                skipped: true,
                reason: Some(format!("invoke_error: {e}")),
            }
        }
    }
}

fn run_aggressive_cargo_gc_background(
    log_path: &std::path::Path,
    ages: &soldr_cache::auto_gc::CargoGcAgeSeconds,
) -> AutoGcCargoOutcome {
    let args = GcCargoArgs {
        dry_run: false,
        toolchain: None,
        max_src_age: Some(format!("{}secs", ages.max_src)),
        max_crate_age: Some(format!("{}secs", ages.max_crate)),
        max_index_age: None,
        max_git_co_age: Some(format!("{}secs", ages.max_git_co)),
        max_git_db_age: None,
        max_download_age: None,
        max_src_size: None,
        max_crate_size: None,
        max_git_size: None,
        max_download_size: None,
        json: true,
    };
    match invoke_cargo_native_gc(&args, true) {
        Ok(outcome) => AutoGcCargoOutcome {
            exit_code: outcome.exit_code,
            skipped: outcome.skipped,
            reason: outcome.skipped_reason,
        },
        Err(e) => {
            let _ = append_auto_gc_log_line(log_path, &format!("tier=3 invoke_error={e}"));
            AutoGcCargoOutcome {
                exit_code: 1,
                skipped: true,
                reason: Some(format!("invoke_error: {e}")),
            }
        }
    }
}

fn run_soldr_target_purge_background(
    paths: &SoldrPaths,
    workspace_targets: &[std::path::PathBuf],
    min_age_secs: u64,
) -> u64 {
    use soldr_cache::gc::{parse_size, scan, GcOptions};
    let db_path = soldr_cache::data_db_path(paths);
    let Ok(registry) = soldr_cache::target_registry::TargetRegistry::open(&db_path) else {
        return 0;
    };
    let larger_than_bytes = parse_size("256M").unwrap_or(256 * 1024 * 1024);
    // Auto-GC always honors at least the configured min-age floor.
    // We never go below 1h.
    let older_than_seconds = soldr_cache::auto_gc::clamp_age_to_floor(min_age_secs, 3600);
    let options = GcOptions {
        older_than_seconds,
        larger_than_bytes,
        dev_roots: resolve_gc_dev_roots(paths),
        dry_run: false,
    };
    let report = match scan(&registry, &options) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    // Filter to candidates that actually live on the affected volumes.
    let mut reclaimed = 0u64;
    let on_volume: std::collections::HashSet<&std::path::Path> =
        workspace_targets.iter().map(|p| p.as_path()).collect();
    for cand in report.candidates {
        if !on_volume.contains(cand.path.as_path()) {
            continue;
        }
        let bytes = cand.size_bytes;
        let outcome = soldr_cache::gc::delete_candidate_dir(cand);
        if outcome.removed {
            reclaimed = reclaimed.saturating_add(bytes);
            let _ = registry.remove(&outcome.candidate.path);
        }
    }
    reclaimed
}

/// Enumerate every soldr-owned path for the auto-GC orchestrator.
fn enumerate_auto_gc_paths(paths: &SoldrPaths) -> Vec<soldr_cache::auto_gc::AutoGcPath> {
    let mut out: Vec<soldr_cache::auto_gc::AutoGcPath> = Vec::new();
    if let Some(cargo_home) = soldr_core::resolve_cargo_home() {
        out.push(soldr_cache::auto_gc::AutoGcPath {
            kind: soldr_cache::auto_gc::AutoGcPathKind::CargoHome,
            path: cargo_home,
        });
    }
    if let Some(rustup_home) = soldr_core::resolve_rustup_home() {
        out.push(soldr_cache::auto_gc::AutoGcPath {
            kind: soldr_cache::auto_gc::AutoGcPathKind::RustupHome,
            path: rustup_home,
        });
    }
    out.push(soldr_cache::auto_gc::AutoGcPath {
        kind: soldr_cache::auto_gc::AutoGcPathKind::SoldrCache,
        path: paths.cache.clone(),
    });
    let db_path = soldr_cache::data_db_path(paths);
    if db_path.exists() {
        if let Ok(registry) = soldr_cache::target_registry::TargetRegistry::open(&db_path) {
            if let Ok(rows) = registry.list() {
                for row in rows {
                    if row.path.exists() {
                        out.push(soldr_cache::auto_gc::AutoGcPath {
                            kind: soldr_cache::auto_gc::AutoGcPathKind::WorkspaceTarget,
                            path: row.path,
                        });
                    }
                }
            }
        }
    }
    out
}

/// System volume probe — Windows uses the drive letter (`C`, `D`),
/// Unix uses the device id from `stat()`. Falls back to the canonical
/// path's root component when neither is available.
struct SystemVolumeProbe;

impl soldr_cache::auto_gc::DiskFreeProbe for SystemVolumeProbe {
    fn free_bytes(&self, path: &std::path::Path) -> Option<u64> {
        let probe = existing_filesystem_probe_path(path);
        available_space(&probe).ok()
    }
}

impl soldr_cache::auto_gc::VolumeProbe for SystemVolumeProbe {
    fn volume_key(&self, path: &std::path::Path) -> Option<String> {
        let probe = existing_filesystem_probe_path(path);
        volume_key_for_path(&probe)
    }
}

#[cfg(windows)]
fn volume_key_for_path(path: &std::path::Path) -> Option<String> {
    // On Windows: prefer the canonical path's drive letter (e.g. "C").
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = canonical.to_string_lossy().to_string();
    // Strip UNC prefix \\?\ if present.
    let trimmed = s.trim_start_matches(r"\\?\");
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0].is_ascii_alphabetic()) {
        return Some((bytes[0] as char).to_ascii_uppercase().to_string());
    }
    None
}

#[cfg(unix)]
fn volume_key_for_path(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let meta = std::fs::metadata(&canonical).ok()?;
    Some(meta.dev().to_string())
}

fn append_auto_gc_log_line(log_path: &std::path::Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    use std::io::Write as _;
    writeln!(file, "{ts} {line}")?;
    Ok(())
}

fn rotate_auto_gc_log_if_needed(log_path: &std::path::Path, max_bytes: u64) -> std::io::Result<()> {
    let meta = match std::fs::metadata(log_path) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    if meta.len() < max_bytes {
        return Ok(());
    }
    let archive = log_path.with_extension("log.old");
    let _ = std::fs::remove_file(&archive);
    std::fs::rename(log_path, &archive)?;
    Ok(())
}

fn emit_startup_target_warning_if_due() {
    let Ok(paths) = SoldrPaths::new() else { return };
    let db_path = soldr_cache::data_db_path(&paths);
    if !db_path.exists() {
        return;
    }
    let Ok(registry) = soldr_cache::target_registry::TargetRegistry::open(&db_path) else {
        return;
    };
    let options = soldr_cache::gc::GcOptions {
        older_than_seconds: soldr_cache::target_registry::DEFAULT_STALE_AGE_SECONDS,
        larger_than_bytes: soldr_cache::target_registry::DEFAULT_STALE_SIZE_BYTES,
        dev_roots: resolve_gc_dev_roots(&paths),
        dry_run: true,
    };
    let marker = soldr_cache::gc_warning_marker_path(&paths);
    match soldr_cache::gc::maybe_build_startup_warning(&registry, &options, &marker) {
        Ok(Some(message)) => eprintln!("{message}"),
        Ok(None) => {}
        Err(_) => {}
    }
}

fn purge_soldr_cache() -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let mut purged_anything = false;

    purged_anything |= remove_soldr_artifact_dir("cache", &paths.cache)?;
    purged_anything |= remove_soldr_artifact_dir("bin", &paths.bin)?;

    if !purged_anything {
        println!("soldr cache is already empty: {}", paths.root.display());
    }

    Ok(())
}

fn remove_soldr_artifact_dir(label: &str, path: &std::path::Path) -> Result<bool, SoldrError> {
    if !path.exists() {
        return Ok(false);
    }

    if std::fs::symlink_metadata(path)?.file_type().is_dir() {
        std::fs::remove_dir_all(path)?;
        println!("removed soldr {label} dir: {}", path.display());
    } else {
        std::fs::remove_file(path)?;
        println!("removed soldr {label} entry: {}", path.display());
    }
    Ok(true)
}

#[derive(Serialize)]
struct VersionOutput {
    schema_version: u32,
    command: &'static str,
    soldr_version: String,
}

#[derive(Serialize)]
struct StatusOutput {
    schema_version: u32,
    command: &'static str,
    soldr_version: String,
    target: String,
    root_dir: String,
    cache_dir: String,
    cache_default_enabled: bool,
    cache_enabled_for_invocation: bool,
    managed_zccache_version: &'static str,
    zccache: ZccacheStatusSnapshot,
}

#[derive(Serialize)]
struct CacheOutput {
    schema_version: u32,
    command: &'static str,
    soldr_version: String,
    managed_zccache_version: &'static str,
    zccache: ZccacheStatusSnapshot,
}

#[derive(Serialize)]
struct ZccacheStatusSnapshot {
    cache_dir: String,
    state_dir: String,
    session_log_path: String,
    session_log_present: bool,
    journal_path: String,
    journal_present: bool,
    session_stats_path: String,
    session_stats_present: bool,
    binary_path: Option<String>,
    binary_fetched: bool,
    status_lines: Vec<String>,
    status_empty: bool,
}

fn version_output() -> VersionOutput {
    VersionOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "version",
        soldr_version: soldr_core::version().to_string(),
    }
}

fn collect_status_output(cache_enabled: bool) -> Result<StatusOutput, SoldrError> {
    let target = soldr_core::TargetTriple::detect()?;
    let paths = SoldrPaths::new()?;
    Ok(StatusOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "status",
        soldr_version: soldr_core::version().to_string(),
        target: target.to_string(),
        root_dir: paths.root.display().to_string(),
        cache_dir: paths.cache.display().to_string(),
        cache_default_enabled: true,
        cache_enabled_for_invocation: cache_enabled,
        managed_zccache_version: soldr_fetch::MANAGED_ZCCACHE_VERSION,
        zccache: collect_zccache_status(&paths)?,
    })
}

fn collect_cache_output() -> Result<CacheOutput, SoldrError> {
    let paths = SoldrPaths::new()?;
    Ok(CacheOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache",
        soldr_version: soldr_core::version().to_string(),
        managed_zccache_version: soldr_fetch::MANAGED_ZCCACHE_VERSION,
        zccache: collect_zccache_status(&paths)?,
    })
}

#[derive(Serialize)]
struct CacheReportOutput {
    schema_version: u32,
    command: &'static str,
    soldr_version: String,
    managed_zccache_version: &'static str,
    /// Path to the per-session stats JSON file.
    session_stats_path: String,
    /// Whether the session-stats file exists on disk.
    session_stats_present: bool,
    /// Path to the per-session JSONL journal.
    journal_path: String,
    /// Whether the journal file exists on disk.
    journal_present: bool,
    /// Verbatim contents of `last-session-stats.json`, parsed into a JSON
    /// value. `null` if the file is missing or unparseable.
    last_session: Option<serde_json::Value>,
    /// Output of `zccache analyze --json` over the per-session journal,
    /// when the managed zccache supports it. `null` otherwise.
    rollups: Option<serde_json::Value>,
    /// Empty for now — populated by future rule passes that turn the
    /// session + rollups into AI-readable diagnoses.
    diagnoses: Vec<serde_json::Value>,
    /// Why a particular field came back null, when relevant. Each entry
    /// is a short string the user can search the soldr docs for.
    notes: Vec<String>,
}

fn collect_cache_report_output() -> Result<CacheReportOutput, SoldrError> {
    let paths = SoldrPaths::new()?;
    let zccache_dir = managed_zccache_cache_dir(&paths)?;
    let session_stats_path = soldr_cache::session_stats_path(&zccache_dir);
    let journal_path = soldr_cache::session_journal_path(&zccache_dir);
    let session_stats_present = session_stats_path.exists();
    let journal_present = journal_path.exists();

    let mut notes: Vec<String> = Vec::new();

    let last_session = if session_stats_present {
        match std::fs::read_to_string(&session_stats_path) {
            Ok(s) => match serde_json::from_str::<serde_json::Value>(s.trim()) {
                Ok(v) => Some(v),
                Err(e) => {
                    notes.push(format!("last_session: unparseable JSON ({e})"));
                    None
                }
            },
            Err(e) => {
                notes.push(format!("last_session: read failed ({e})"));
                None
            }
        }
    } else {
        notes.push(
            "last_session: file missing — run a build with managed zccache first".to_string(),
        );
        None
    };

    let rollups = if journal_present {
        match cached_managed_zccache(&paths)? {
            Some(fetch) => {
                let journal_arg = journal_path.display().to_string();
                let result = run_zccache_command_raw_in_cache_dir(
                    &fetch.binary_path,
                    &["analyze", &journal_arg, "--json"],
                    &zccache_dir,
                )?;
                if result.status.success() {
                    let stdout = String::from_utf8_lossy(&result.stdout);
                    match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            notes
                                .push(format!("rollups: zccache analyze stdout unparseable ({e})"));
                            None
                        }
                    }
                } else if zccache_subcommand_unsupported(&result, "analyze") {
                    notes.push(format!(
                        "rollups: managed zccache {} does not yet support `analyze` — upgrade to 1.5.0+",
                        soldr_fetch::MANAGED_ZCCACHE_VERSION
                    ));
                    None
                } else {
                    notes.push(format!(
                        "rollups: zccache analyze exited with status {:?}",
                        result.status.code()
                    ));
                    None
                }
            }
            None => {
                notes.push(
                    "rollups: managed zccache binary not yet fetched (no builds run yet)"
                        .to_string(),
                );
                None
            }
        }
    } else {
        notes
            .push("rollups: journal missing — soldr writes it on cache-enabled builds".to_string());
        None
    };

    Ok(CacheReportOutput {
        schema_version: JSON_SCHEMA_VERSION,
        command: "cache report",
        soldr_version: soldr_core::version().to_string(),
        managed_zccache_version: soldr_fetch::MANAGED_ZCCACHE_VERSION,
        session_stats_path: session_stats_path.display().to_string(),
        session_stats_present,
        journal_path: journal_path.display().to_string(),
        journal_present,
        last_session,
        rollups,
        diagnoses: Vec::new(),
        notes,
    })
}

/// Heuristic: detect whether a `zccache <subcommand>` invocation failed
/// because the subcommand does not exist in the running binary (clap
/// emits "error: unrecognized subcommand"). Used to differentiate
/// version-skew misses from real failures.
fn zccache_subcommand_unsupported(output: &std::process::Output, subcommand: &str) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let needles = [
        "unrecognized subcommand",
        "unrecognized command",
        "error: subcommand",
        "invalid value for",
    ];
    let combined = format!("{stderr}\n{stdout}");
    needles.iter().any(|n| combined.contains(n)) && combined.contains(subcommand)
}

fn run_cache_report_command(json: bool) -> Result<(), SoldrError> {
    let output = collect_cache_report_output()?;
    if json {
        print_json(&output)?;
    } else {
        print_cache_report_output(&output);
    }
    Ok(())
}

fn print_cache_report_output(output: &CacheReportOutput) {
    println!("soldr cache report");
    println!(
        "  session-stats: {} ({})",
        output.session_stats_path,
        if output.session_stats_present {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "  journal:       {} ({})",
        output.journal_path,
        if output.journal_present {
            "present"
        } else {
            "missing"
        }
    );
    if let Some(stats) = &output.last_session {
        if let Some(rate) = stats.get("hit_rate").and_then(|v| v.as_f64()) {
            println!("  hit_rate:      {:.1}%", rate * 100.0);
        }
        if let Some(hits) = stats.get("hits").and_then(|v| v.as_u64()) {
            let misses = stats.get("misses").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("  hits/misses:   {hits}/{misses}");
        }
        if let Some(saved_ms) = stats.get("time_saved_ms").and_then(|v| v.as_u64()) {
            println!("  time_saved:    {saved_ms} ms");
        }
    }
    if let Some(rollups) = &output.rollups {
        if let Some(by_ext) = rollups.get("by_extension").and_then(|v| v.as_object()) {
            if !by_ext.is_empty() {
                println!("  by extension:");
                for (ext, bucket) in by_ext {
                    let h = bucket.get("hits").and_then(|v| v.as_u64()).unwrap_or(0);
                    let m = bucket.get("misses").and_then(|v| v.as_u64()).unwrap_or(0);
                    println!("    {ext:<14}  hits={h}  misses={m}");
                }
            }
        }
    }
    if !output.notes.is_empty() {
        println!("  notes:");
        for note in &output.notes {
            println!("    - {note}");
        }
    }
}

fn collect_zccache_status(paths: &SoldrPaths) -> Result<ZccacheStatusSnapshot, SoldrError> {
    let zccache_dir = managed_zccache_cache_dir(paths)?;
    let session_log_path = soldr_cache::session_log_path(&zccache_dir);
    let session_log_present = session_log_path.exists();
    let journal_path = soldr_cache::session_journal_path(&zccache_dir);
    let journal_present = journal_path.exists();
    let session_stats_path = soldr_cache::session_stats_path(&zccache_dir);
    let session_stats_present = session_stats_path.exists();

    match cached_managed_zccache(paths)? {
        Some(fetch) => {
            let output =
                run_zccache_command_in_cache_dir(&fetch.binary_path, &["status"], &zccache_dir)?;
            let stdout = output.stdout.trim();
            let status_lines = stdout.lines().map(str::to_owned).collect();
            Ok(ZccacheStatusSnapshot {
                cache_dir: zccache_dir.display().to_string(),
                state_dir: zccache_dir.display().to_string(),
                session_log_path: session_log_path.display().to_string(),
                session_log_present,
                journal_path: journal_path.display().to_string(),
                journal_present,
                session_stats_path: session_stats_path.display().to_string(),
                session_stats_present,
                binary_path: Some(fetch.binary_path.display().to_string()),
                binary_fetched: true,
                status_lines,
                status_empty: stdout.is_empty(),
            })
        }
        None => Ok(ZccacheStatusSnapshot {
            cache_dir: zccache_dir.display().to_string(),
            state_dir: zccache_dir.display().to_string(),
            session_log_path: session_log_path.display().to_string(),
            session_log_present,
            journal_path: journal_path.display().to_string(),
            journal_present,
            session_stats_path: session_stats_path.display().to_string(),
            session_stats_present,
            binary_path: None,
            binary_fetched: false,
            status_lines: Vec::new(),
            status_empty: false,
        }),
    }
}

fn print_status_output(output: &StatusOutput) {
    println!("soldr {}", output.soldr_version);
    println!("target: {}", output.target);
    println!("root dir: {}", output.root_dir);
    println!("cache dir: {}", output.cache_dir);
    println!("cache default: enabled");
    println!(
        "cache mode: {}",
        if output.cache_enabled_for_invocation {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("zccache version: {}", output.managed_zccache_version);
    print_zccache_status_snapshot(&output.zccache);
}

fn print_cache_output(output: &CacheOutput) {
    print_zccache_status_snapshot(&output.zccache);
}

fn print_zccache_status_snapshot(snapshot: &ZccacheStatusSnapshot) {
    println!("soldr zccache cache dir: {}", snapshot.cache_dir);
    println!("soldr zccache state dir: {}", snapshot.state_dir);
    println!(
        "last session log: {} ({})",
        snapshot.session_log_path,
        if snapshot.session_log_present {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "last session journal: {} ({})",
        snapshot.journal_path,
        if snapshot.journal_present {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "last session stats: {} ({})",
        snapshot.session_stats_path,
        if snapshot.session_stats_present {
            "present"
        } else {
            "missing"
        }
    );

    if let Some(binary_path) = &snapshot.binary_path {
        println!("zccache binary: {binary_path}");
        if snapshot.status_empty {
            println!("zccache status: no output");
        } else {
            for line in &snapshot.status_lines {
                println!("zccache: {line}");
            }
        }
    } else {
        println!(
            "zccache binary: not fetched yet (will fetch managed zccache {} on the first cache-enabled build)",
            soldr_fetch::MANAGED_ZCCACHE_VERSION
        );
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<(), SoldrError> {
    serde_json::to_writer_pretty(std::io::stdout(), value)
        .map_err(|e| SoldrError::Other(format!("failed to serialize JSON output: {e}")))?;
    println!();
    Ok(())
}

struct CommandOutput {
    stdout: String,
}

fn managed_zccache_cache_dir(paths: &SoldrPaths) -> Result<std::path::PathBuf, SoldrError> {
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

fn normalize_path_for_compare(path: &std::path::Path) -> Result<std::path::PathBuf, SoldrError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn run_zccache_command_in_cache_dir(
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

fn run_zccache_command_strings_in_cache_dir(
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

fn run_zccache_command_raw_in_cache_dir(
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

fn command_stderr(output: &std::process::Output) -> String {
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

fn rustup_binary() -> std::path::PathBuf {
    non_empty_env_path(TEST_RUSTUP_BIN_ENV_VAR).unwrap_or_else(|| "rustup".into())
}

fn zccache_binary_override() -> Option<std::path::PathBuf> {
    non_empty_env_path(TEST_ZCCACHE_BIN_ENV_VAR)
        .or_else(|| non_empty_env_path(soldr_cache::ZCCACHE_BINARY_ENV_VAR))
}

fn non_empty_env_path(env_var: &str) -> Option<std::path::PathBuf> {
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

fn cached_managed_zccache(
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
mod tests {
    use super::{
        allowed_artifact_classes, build_rust_artifact_plan, build_thin_manifest,
        cargo_args_specify_target, cargo_args_use_reserved_no_cache,
        cargo_metadata_passthrough_args, cargo_profile, cargo_target_triple,
        compute_plan_inputs_hash, dropped_artifact_classes, evaluate_warm_restore_skip,
        extract_as_pin, first_cargo_subcommand, gc_purge_worker_count_for, is_sccache_wrapper,
        low_disk_warning_for_free_bytes, low_disk_warning_for_path, normalize_version,
        parse_gc_purge_answer, parse_rust_artifact_cache_tar_threads, parse_tool_spec,
        resolve_bundle_walk_thread_count, rustc_wrapper_mode_from_env_var,
        rustup_resolution_failure, selected_cargo_args, should_self_relocate_for_invocation,
        should_skip_warm_restore, should_trampoline, stderr_indicates_unknown_session,
        walk_bundle_files, warm_restore_sentinel_path, warm_restore_skip_enabled,
        write_thin_manifest, write_warm_restore_sentinel, CargoMetadata, CargoMetadataPackage, Cli,
        Commands, GcSubcommand, RustArtifactPlan, RustArtifactPlanContext, RustPlanInputs,
        RustPlanPackages, RustToolchainIdentity, RustcWrapperMode, ThinSliceManifest,
        WarmRestoreSentinel, WarmRestoreSkipInputs, ZccacheBuildSession, BUNDLE_WALK_THREAD_CAP,
        LOW_DISK_WARNING_THRESHOLD_BYTES, RUSTC_WRAPPER_OVERRIDE_ENV_VAR,
        SKIP_WARM_RESTORE_ENV_VAR, THIN_MANIFEST_FILENAME, WARM_RESTORE_MAX_AGE_SECONDS,
    };
    use clap::Parser;
    use soldr_fetch::VersionSpec;
    use std::ffi::{OsStr, OsString};
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Serialises tests that mutate process-wide environment variables so
    /// they do not race with each other under parallel `cargo test`. The
    /// guard objects below restore the previous value on drop, but two
    /// tests touching the same key concurrently would still observe each
    /// other's mid-test state without this lock.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that sets or removes an environment variable for the
    /// duration of a test and restores the previous value on drop. Modelled
    /// after the same helper in `soldr-core`'s test module.
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn gc_cli_parses_summary_and_purge_modes() {
        let summary = Cli::try_parse_from(["soldr", "gc", "--json"]).unwrap();
        match summary.command {
            Commands::Gc {
                command: None,
                json,
                ..
            } => assert!(json, "gc --json should parse as summary JSON"),
            _ => panic!("expected gc summary command"),
        }

        let purge = Cli::try_parse_from([
            "soldr",
            "gc",
            "purge",
            "--all",
            "--older-than",
            "30d",
            "--larger-than",
            "1GB",
        ])
        .unwrap();
        match purge.command {
            Commands::Gc {
                command:
                    Some(GcSubcommand::Purge {
                        all,
                        older_than,
                        larger_than,
                        ..
                    }),
                ..
            } => {
                assert!(all);
                assert_eq!(older_than, "30d");
                assert_eq!(larger_than, "1GB");
            }
            _ => panic!("expected gc purge command"),
        }
    }

    #[test]
    fn gc_purge_prompt_defaults_enter_to_yes() {
        for input in ["", "\n", "y", "Y", "yes", " YES "] {
            assert!(parse_gc_purge_answer(input), "expected {input:?} to accept");
        }
        for input in ["n", "no", "anything else"] {
            assert!(!parse_gc_purge_answer(input), "expected {input:?} to skip");
        }
    }

    #[test]
    fn gc_purge_worker_count_is_bounded() {
        assert_eq!(gc_purge_worker_count_for(0), 1);
        assert_eq!(gc_purge_worker_count_for(1), 1);
        assert_eq!(gc_purge_worker_count_for(2), 2);
        assert_eq!(gc_purge_worker_count_for(16), 4);
    }

    #[test]
    fn low_disk_warning_formats_yellow_below_threshold() {
        let message = low_disk_warning_for_free_bytes(1536 * 1024 * 1024, true)
            .expect("expected low-disk warning below threshold");
        assert!(message.contains("\x1b[33mwarning\x1b[0m"));
        assert!(message.contains("1.5 GB free"));
        assert!(message.contains("Run `soldr gc`"));
    }

    #[test]
    fn low_disk_warning_omits_at_threshold() {
        assert!(low_disk_warning_for_free_bytes(LOW_DISK_WARNING_THRESHOLD_BYTES, true).is_none());
    }

    #[test]
    fn low_disk_probe_failure_is_nonfatal() {
        let warning = low_disk_warning_for_path(std::path::Path::new("."), true, |_| {
            Err(std::io::Error::other("probe failed"))
        });
        assert!(warning.is_none());
    }

    #[test]
    fn cargo_args_detect_explicit_target_flag() {
        assert!(cargo_args_specify_target(&[
            "build".into(),
            "--target".into(),
            "x86_64-pc-windows-msvc".into(),
        ]));
        assert!(cargo_args_specify_target(&[
            "build".into(),
            "--target=x86_64-pc-windows-msvc".into(),
        ]));
    }

    #[test]
    fn cargo_args_ignore_target_after_passthrough_separator() {
        assert!(!cargo_args_specify_target(&[
            "test".into(),
            "--".into(),
            "--target".into(),
            "ignored".into(),
        ]));
    }

    #[test]
    fn cargo_args_reject_reserved_no_cache_before_passthrough_separator() {
        assert!(cargo_args_use_reserved_no_cache(&[
            "build".into(),
            "--no-cache".into(),
        ]));
        assert!(!cargo_args_use_reserved_no_cache(&[
            "test".into(),
            "--".into(),
            "--no-cache".into(),
        ]));
    }

    #[test]
    fn rustc_wrapper_override_defaults_to_managed_zccache() {
        assert_eq!(
            rustc_wrapper_mode_from_env_var(None),
            RustcWrapperMode::ManagedZccache
        );
    }

    #[test]
    fn rustc_wrapper_override_disables_wrapper_for_empty_or_none() {
        for value in ["", " ", "none", "NONE"] {
            assert_eq!(
                rustc_wrapper_mode_from_env_var(Some(OsStr::new(value))),
                RustcWrapperMode::Disabled,
                "expected {value:?} to disable wrapper injection"
            );
        }
    }

    #[test]
    fn rustc_wrapper_override_uses_custom_wrapper_name() {
        assert_eq!(
            rustc_wrapper_mode_from_env_var(Some(OsStr::new("sccache"))),
            RustcWrapperMode::Custom("sccache".into())
        );
    }

    #[test]
    fn self_relocate_gate_targets_managed_cacheable_cargo_builds() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _wrapper = EnvVarGuard::remove(RUSTC_WRAPPER_OVERRIDE_ENV_VAR);

        assert!(should_self_relocate_for_invocation(&[
            "soldr".into(),
            "cargo".into(),
            "build".into(),
        ]));
        assert!(should_self_relocate_for_invocation(&[
            "soldr".into(),
            "--as".into(),
            env!("CARGO_PKG_VERSION").into(),
            "cargo".into(),
            "test".into(),
        ]));
        assert!(!should_self_relocate_for_invocation(&[
            "soldr".into(),
            "cargo".into(),
            "--version".into(),
        ]));
        assert!(!should_self_relocate_for_invocation(&[
            "soldr".into(),
            "--no-cache".into(),
            "cargo".into(),
            "build".into(),
        ]));
        assert!(!should_self_relocate_for_invocation(&[
            "soldr".into(),
            "version".into(),
        ]));

        let _custom = EnvVarGuard::set(RUSTC_WRAPPER_OVERRIDE_ENV_VAR, "sccache");
        assert!(!should_self_relocate_for_invocation(&[
            "soldr".into(),
            "cargo".into(),
            "build".into(),
        ]));
    }

    #[test]
    fn sccache_wrapper_detection_accepts_binary_names_and_paths() {
        assert!(is_sccache_wrapper(OsStr::new("sccache")));
        assert!(is_sccache_wrapper(OsStr::new("sccache.exe")));
        assert!(is_sccache_wrapper(OsStr::new("/tmp/tools/sccache")));
        assert!(!is_sccache_wrapper(OsStr::new("zccache")));
        assert!(!is_sccache_wrapper(OsStr::new("sccache-proxy")));
    }

    #[test]
    fn parse_tool_spec_defaults_to_latest_version() {
        let (tool, version) = parse_tool_spec("maturin");
        assert_eq!(tool, "maturin");
        assert!(matches!(version, VersionSpec::Latest));
    }

    #[test]
    fn first_cargo_subcommand_skips_leading_flags() {
        assert_eq!(
            first_cargo_subcommand(&["--verbose".into(), "nextest".into(), "run".into()]),
            Some("nextest")
        );
        assert_eq!(
            first_cargo_subcommand(&["nextest".into(), "run".into()]),
            Some("nextest")
        );
        assert_eq!(first_cargo_subcommand(&["--help".into()]), None);
        assert_eq!(first_cargo_subcommand(&[]), None);
    }

    #[test]
    fn first_cargo_subcommand_stops_at_passthrough_separator() {
        assert_eq!(
            first_cargo_subcommand(&["--".into(), "nextest".into()]),
            None
        );
    }

    #[test]
    fn rust_artifact_plan_selects_external_packages_and_path_exclusions() {
        let root =
            std::env::temp_dir().join(format!("soldr-rust-plan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("app/src")).unwrap();
        std::fs::create_dir_all(root.join("local_dep/src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("Cargo.lock"), "# lock\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.join("app/Cargo.toml"), "[package]\nname='app'\n").unwrap();
        std::fs::write(
            root.join("local_dep/Cargo.toml"),
            "[package]\nname='local_dep'\n",
        )
        .unwrap();

        let metadata = CargoMetadata {
            workspace_root: root.clone(),
            target_directory: root.join("target"),
            workspace_members: vec!["path+file:///repo/app#app@0.1.0".to_string()],
            packages: vec![
                CargoMetadataPackage {
                    id: "path+file:///repo/app#app@0.1.0".to_string(),
                    source: None,
                },
                CargoMetadataPackage {
                    id: "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0"
                        .to_string(),
                    source: Some("registry+https://github.com/rust-lang/crates.io-index".into()),
                },
                CargoMetadataPackage {
                    id: "path+file:///repo/local_dep#local_dep@0.1.0".to_string(),
                    source: None,
                },
            ],
        };
        let toolchain = RustToolchainIdentity {
            rustc: "rustc 1.0.0-test".to_string(),
            cargo: "cargo 1.0.0-test".to_string(),
            channel: "test".to_string(),
            host: "x86_64-unknown-test".to_string(),
        };
        let session = ZccacheBuildSession {
            binary_path: "zccache".into(),
            cache_dir: root.join("cache"),
            session_id: "session-1".to_string(),
            session_log_path: root.join("cache/logs/last-session.log"),
            journal_path: root.join("cache/logs/last-session.jsonl"),
            session_stats_path: root.join("cache/logs/last-session-stats.json"),
        };
        let args = vec![
            "build".to_string(),
            "--release".to_string(),
            "--features".to_string(),
            "serde/derive".to_string(),
            "--target".to_string(),
            "x86_64-unknown-linux-gnu".to_string(),
        ];

        let plan = build_rust_artifact_plan(
            &metadata,
            &toolchain,
            &args,
            "thin",
            Some("thin-v1"),
            &session,
            None,
        )
        .expect("build rust artifact plan");

        assert_eq!(plan.schema_version, 1);
        assert_eq!(plan.mode, "thin");
        assert_eq!(plan.cache_profile, Some("thin-v1"));
        assert_eq!(plan.profile, "release");
        assert_eq!(plan.target_triple, "x86_64-unknown-linux-gnu");
        assert_eq!(plan.packages.workspace_package_ids.len(), 1);
        assert_eq!(plan.packages.selected_package_ids.len(), 1);
        assert!(plan.packages.selected_package_ids[0].contains("serde"));
        assert_eq!(plan.packages.excluded_path_package_ids.len(), 1);
        assert!(plan.allowed_artifact_classes.contains(&"cargo_fingerprint"));
        assert!(plan.dropped_artifact_classes.is_empty());
        assert_eq!(plan.cache_schema_version, 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rust_artifact_plan_helpers_parse_mode_profile_target_and_metadata_args() {
        let args = vec![
            "+stable".to_string(),
            "build".to_string(),
            "--locked".to_string(),
            "--features=fast".to_string(),
            "--target".to_string(),
            "wasm32-unknown-unknown".to_string(),
            "--profile".to_string(),
            "release-lto".to_string(),
            "--".to_string(),
            "--ignored".to_string(),
        ];

        assert_eq!(cargo_profile(&args), "release-lto");
        assert_eq!(
            cargo_target_triple(&args, "x86_64-unknown-linux-gnu"),
            "wasm32-unknown-unknown"
        );
        assert_eq!(
            selected_cargo_args(&args, &["--features"]),
            vec!["--features=fast".to_string()]
        );
        assert_eq!(allowed_artifact_classes("full", None), Vec::<&str>::new());
        assert_eq!(
            cargo_metadata_passthrough_args(&args)
                .iter()
                .map(|value| value.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec!["--locked".to_string(), "--features=fast".to_string()]
        );
    }

    /// `thin-v1` is the legacy slice. It must continue to ship the
    /// historically-included library-output classes so rollout day 0 is a
    /// no-op for callers that did not opt in to `thin-v2`.
    #[test]
    fn allowed_artifact_classes_thin_v1_keeps_legacy_set() {
        let allowed = allowed_artifact_classes("thin", Some("thin-v1"));
        for expected in [
            "rlib",
            "rmeta",
            "dep_info",
            "proc_macro",
            "cargo_fingerprint",
            "build_script_metadata",
            "build_script_output",
        ] {
            assert!(
                allowed.contains(&expected),
                "thin-v1 must keep {expected} in the allowlist; got {allowed:?}"
            );
        }
        assert!(dropped_artifact_classes("thin", Some("thin-v1")).is_empty());
    }

    /// `thin-v2` aggressively prunes the slice. The categories listed in
    /// `docs/THIN_TARGET_CACHE_PRUNING.md` Section 3.2 must NOT appear in the
    /// allowlist, and the new fingerprint split (`cargo_fingerprint_meta`,
    /// dropping `cargo_fingerprint_outputs`) must be honored.
    #[test]
    fn allowed_artifact_classes_thin_v2_drops_heavy_categories() {
        let allowed = allowed_artifact_classes("thin", Some("thin-v2"));

        // Drop list per design Section 3.2.
        for forbidden in [
            "rlib",
            "rmeta",
            "proc_macro",
            "incremental",
            "cargo_fingerprint",
            "cargo_fingerprint_outputs",
            "build_script_build",
            "dwo",
            "pdb",
            "dsym",
        ] {
            assert!(
                !allowed.contains(&forbidden),
                "thin-v2 must drop {forbidden} from the allowlist; got {allowed:?}"
            );
        }

        // Keep list per design Section 3.1.
        for required in [
            "cargo_fingerprint_meta",
            "dep_info",
            "build_script_metadata",
            "build_script_output",
        ] {
            assert!(
                allowed.contains(&required),
                "thin-v2 must keep {required} in the allowlist; got {allowed:?}"
            );
        }

        // The drop list is surfaced as data so zccache can short-circuit.
        let dropped = dropped_artifact_classes("thin", Some("thin-v2"));
        for forbidden in [
            "incremental",
            "rlib",
            "rmeta",
            "proc_macro",
            "build_script_build",
            "dwo",
            "pdb",
            "dsym",
            "cargo_fingerprint_outputs",
        ] {
            assert!(
                dropped.contains(&forbidden),
                "thin-v2 must publish {forbidden} in dropped_artifact_classes; got {dropped:?}"
            );
        }
    }

    /// Bumping `cache_schema_version` from 1 to 2 is the contract zccache
    /// uses to decide whether the new fingerprint split is in effect.
    #[test]
    fn rust_artifact_plan_bumps_cache_schema_version_for_thin_v2() {
        let root = std::env::temp_dir().join(format!(
            "soldr-rust-plan-thinv2-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos(),
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("app/src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("Cargo.lock"), "# lock\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.join("app/Cargo.toml"), "[package]\nname='app'\n").unwrap();

        let metadata = CargoMetadata {
            workspace_root: root.clone(),
            target_directory: root.join("target"),
            workspace_members: vec!["path+file:///repo/app#app@0.1.0".to_string()],
            packages: vec![CargoMetadataPackage {
                id: "path+file:///repo/app#app@0.1.0".to_string(),
                source: None,
            }],
        };
        let toolchain = RustToolchainIdentity {
            rustc: "rustc 1.0.0-test".to_string(),
            cargo: "cargo 1.0.0-test".to_string(),
            channel: "test".to_string(),
            host: "x86_64-unknown-test".to_string(),
        };
        let session = ZccacheBuildSession {
            binary_path: "zccache".into(),
            cache_dir: root.join("cache"),
            session_id: "session-thinv2".to_string(),
            session_log_path: root.join("cache/logs/last-session.log"),
            journal_path: root.join("cache/logs/last-session.jsonl"),
            session_stats_path: root.join("cache/logs/last-session-stats.json"),
        };

        let plan = build_rust_artifact_plan(
            &metadata,
            &toolchain,
            &["build".to_string()],
            "thin",
            Some("thin-v2"),
            &session,
            None,
        )
        .expect("build rust artifact plan");

        assert_eq!(plan.schema_version, 1, "outer schema is unchanged");
        assert_eq!(
            plan.cache_schema_version, 2,
            "thin-v2 bumps the cache-side schema so zccache can branch on it"
        );
        assert_eq!(plan.cache_profile, Some("thin-v2"));
        assert!(plan.allowed_artifact_classes.contains(&"dep_info"));
        assert!(!plan.allowed_artifact_classes.contains(&"rlib"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The manifest must enumerate every regular file in the bundle, with
    /// relative POSIX-style paths and either a size or `null`. It must NOT
    /// list directories or its own filename.
    #[test]
    fn thin_manifest_enumerates_only_files_actually_present() {
        let bundle = tempfile::tempdir().expect("tempdir for bundle");
        let bundle_path = bundle.path();

        // Build a representative bundle layout: nested dir + a file at root +
        // an empty subdir (must not appear in the manifest).
        std::fs::create_dir_all(bundle_path.join("debug/.fingerprint/foo-abc")).unwrap();
        std::fs::create_dir_all(bundle_path.join("debug/deps")).unwrap();
        std::fs::create_dir_all(bundle_path.join("debug/empty_subdir")).unwrap();
        std::fs::write(
            bundle_path.join("debug/.fingerprint/foo-abc/invoked.timestamp"),
            "",
        )
        .unwrap();
        std::fs::write(
            bundle_path.join("debug/.fingerprint/foo-abc/dep-lib-foo"),
            b"abc123",
        )
        .unwrap();
        std::fs::write(bundle_path.join("debug/deps/foo-abc.d"), b"foo.rs:\n").unwrap();
        std::fs::write(bundle_path.join("CACHEDIR.TAG"), b"Signature: 8a4773\n").unwrap();

        let manifest = build_thin_manifest(bundle_path, "thin-v2").expect("build manifest");

        assert_eq!(manifest.schema_version, 2);
        assert_eq!(manifest.cache_profile, "thin-v2");

        let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        // Sorted, POSIX-style, no manifest self-reference, no empty dir.
        assert_eq!(
            paths,
            vec![
                "CACHEDIR.TAG",
                "debug/.fingerprint/foo-abc/dep-lib-foo",
                "debug/.fingerprint/foo-abc/invoked.timestamp",
                "debug/deps/foo-abc.d",
            ],
        );
        // Sizes are populated for files that exist on disk.
        let by_path: std::collections::HashMap<_, _> = manifest
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.size_bytes))
            .collect();
        assert_eq!(
            by_path.get("debug/.fingerprint/foo-abc/dep-lib-foo"),
            Some(&Some(6))
        );
        assert_eq!(
            by_path.get("debug/.fingerprint/foo-abc/invoked.timestamp"),
            Some(&Some(0))
        );
    }

    /// The on-disk manifest emitted by `write_thin_manifest` must round-trip
    /// through serde so downstream verifiers can deserialize it without
    /// surprises (no field renames, no missing fields).
    #[test]
    fn thin_manifest_round_trips_through_serde() {
        let bundle = tempfile::tempdir().expect("tempdir for manifest round-trip");
        let bundle_path = bundle.path();
        std::fs::create_dir_all(bundle_path.join("debug/deps")).unwrap();
        std::fs::write(
            bundle_path.join("debug/deps/example.d"),
            b"example: src/lib.rs\n",
        )
        .unwrap();

        write_thin_manifest(bundle_path, Some("thin-v2")).expect("write manifest");

        let manifest_path = bundle_path.join(THIN_MANIFEST_FILENAME);
        assert!(
            manifest_path.is_file(),
            "manifest must land at the well-known path"
        );

        let raw = std::fs::read_to_string(&manifest_path).expect("read manifest");
        let parsed: ThinSliceManifest = serde_json::from_str(&raw).expect("deserialize manifest");

        assert_eq!(parsed.schema_version, 2);
        assert_eq!(parsed.cache_profile, "thin-v2");
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].path, "debug/deps/example.d");

        // Serializing the parsed value back must produce a JSON document that
        // deserializes to an equal value (canonical round-trip).
        let serialized = serde_json::to_string(&parsed).expect("serialize manifest");
        let reparsed: ThinSliceManifest =
            serde_json::from_str(&serialized).expect("re-deserialize manifest");
        assert_eq!(parsed, reparsed);
    }

    /// A second `write_thin_manifest` call into the same bundle directory
    /// must not list the previously-written manifest among its own entries.
    #[test]
    fn thin_manifest_does_not_self_reference_on_repeat_save() {
        let bundle = tempfile::tempdir().expect("tempdir for repeat save");
        let bundle_path = bundle.path();
        std::fs::write(bundle_path.join("only.txt"), b"hello").unwrap();

        write_thin_manifest(bundle_path, Some("thin-v2")).expect("first manifest write");
        write_thin_manifest(bundle_path, Some("thin-v2")).expect("second manifest write");

        let raw = std::fs::read_to_string(bundle_path.join(THIN_MANIFEST_FILENAME))
            .expect("read manifest");
        let parsed: ThinSliceManifest = serde_json::from_str(&raw).expect("parse manifest");

        let paths: Vec<&str> = parsed.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["only.txt"]);
    }

    #[test]
    fn known_subcommand_registry_recognizes_phase_two_tools() {
        for sub in ["nextest", "deny", "audit", "llvm-cov"] {
            let spec = soldr_fetch::lookup_by_cargo_subcommand(sub)
                .unwrap_or_else(|| panic!("missing registry entry for cargo {sub}"));
            assert_eq!(spec.cargo_subcommand, Some(sub));
            assert!(spec.crate_name.starts_with("cargo-"));
        }
    }

    #[test]
    fn known_subcommand_registry_recognizes_phase_three_tools() {
        for sub in ["udeps", "semver-checks", "expand", "watch"] {
            let spec = soldr_fetch::lookup_by_cargo_subcommand(sub)
                .unwrap_or_else(|| panic!("missing registry entry for cargo {sub}"));
            assert_eq!(spec.cargo_subcommand, Some(sub));
            assert!(spec.crate_name.starts_with("cargo-"));
        }
    }

    #[test]
    fn top_level_tools_are_not_cargo_subcommands() {
        for crate_name in [
            "cross",
            "mdbook",
            "cbindgen",
            "wasm-pack",
            "trunk",
            "sccache",
        ] {
            let spec = soldr_fetch::lookup_by_crate(crate_name)
                .unwrap_or_else(|| panic!("missing registry entry for {crate_name}"));
            assert_eq!(spec.cargo_subcommand, None);
        }
    }

    #[test]
    fn soldr_itself_is_registered_for_self_trampoline() {
        let spec = soldr_fetch::lookup_by_crate("soldr")
            .expect("soldr should be registered in known_tools for --as trampoline");
        assert_eq!(spec.binary_name, "soldr");
        assert_eq!(spec.repo, Some(("zackees", "soldr")));
        assert_eq!(spec.cargo_subcommand, None);
    }

    #[test]
    fn extract_as_pin_extracts_space_separated_flag_before_subcommand() {
        let (version, rest) = extract_as_pin(&[
            "--as".into(),
            "0.5.2".into(),
            "cargo".into(),
            "build".into(),
        ])
        .unwrap();
        assert_eq!(version, Some("0.5.2".into()));
        assert_eq!(rest, vec!["cargo".to_string(), "build".into()]);
    }

    #[test]
    fn extract_as_pin_extracts_equals_form() {
        let (version, rest) =
            extract_as_pin(&["--as=0.5.2".into(), "cargo".into(), "build".into()]).unwrap();
        assert_eq!(version, Some("0.5.2".into()));
        assert_eq!(rest, vec!["cargo".to_string(), "build".into()]);
    }

    #[test]
    fn extract_as_pin_preserves_other_leading_flags() {
        let (version, rest) = extract_as_pin(&[
            "--no-cache".into(),
            "--as".into(),
            "0.5.2".into(),
            "cargo".into(),
        ])
        .unwrap();
        assert_eq!(version, Some("0.5.2".into()));
        assert_eq!(rest, vec!["--no-cache".to_string(), "cargo".into()]);
    }

    #[test]
    fn extract_as_pin_ignores_flag_after_subcommand() {
        let args = vec!["cargo".into(), "--as".into(), "0.5.2".into()];
        let (version, rest) = extract_as_pin(&args).unwrap();
        assert_eq!(version, None);
        assert_eq!(rest, args);
    }

    #[test]
    fn extract_as_pin_ignores_flag_after_passthrough_separator() {
        let args = vec!["cargo".into(), "--".into(), "--as".into(), "0.5.2".into()];
        let (version, rest) = extract_as_pin(&args).unwrap();
        assert_eq!(version, None);
        assert_eq!(rest, args);
    }

    #[test]
    fn extract_as_pin_rejects_missing_value() {
        let err = extract_as_pin(&["--as".into()]).unwrap_err();
        assert!(err.to_string().contains("requires a version"));
    }

    #[test]
    fn extract_as_pin_rejects_empty_value() {
        let err = extract_as_pin(&["--as".into(), "".into()]).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
        let err2 = extract_as_pin(&["--as=".into()]).unwrap_err();
        assert!(err2.to_string().contains("requires a version"));
    }

    #[test]
    fn extract_as_pin_rejects_duplicate_flag() {
        let err =
            extract_as_pin(&["--as".into(), "0.5.2".into(), "--as=0.4.0".into()]).unwrap_err();
        assert!(err.to_string().contains("more than once"));
    }

    #[test]
    fn normalize_version_strips_leading_v() {
        assert_eq!(normalize_version("0.5.2"), "0.5.2");
        assert_eq!(normalize_version("v0.5.2"), "0.5.2");
        assert_eq!(normalize_version("  v0.5.2 "), "0.5.2");
    }

    #[test]
    fn should_trampoline_matches_current_version_as_no_op() {
        assert!(!should_trampoline(env!("CARGO_PKG_VERSION")));
        assert!(!should_trampoline(&format!(
            "v{}",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(should_trampoline("0.0.0-not-this-version"));
    }

    #[test]
    fn rustup_resolution_failure_appends_ci_guidance() {
        let error = rustup_resolution_failure(
            "rustc",
            b"error: toolchain '1.94.1-x86_64-pc-windows-msvc' is not installed",
        );

        let rendered = error.to_string();
        assert!(rendered.contains("failed to resolve rustc via rustup: error: toolchain '1.94.1-x86_64-pc-windows-msvc' is not installed"));
        assert!(rendered.contains("pins Rust in rust-toolchain.toml"));
        assert!(rendered.contains("generic stable toolchain"));
        assert!(rendered.contains("RUSTUP_TOOLCHAIN"));
        assert!(rendered.contains("setup-soldr action path"));
    }

    /// Regression test for the zccache v1.4.0 wire-compat bug. zccache
    /// v1.4.0 deserializes the plan with `#[serde(deny_unknown_fields)]`
    /// and does NOT know about `cache_profile` / `dropped_artifact_classes`.
    /// Therefore the default `thin-v1` (and `full`) JSON must look exactly
    /// like the pre-PR plan: neither field may appear in the JSON. The
    /// thin-v2 opt-in is allowed (and required) to surface them.
    #[test]
    fn rust_artifact_plan_thin_v1_json_omits_new_fields_for_zccache_compat() {
        let plan = RustArtifactPlan {
            schema_version: 1,
            mode: "thin".to_string(),
            cache_profile: Some("thin-v1"),
            workspace_root: "/tmp/ws".to_string(),
            target_dir: "/tmp/ws/target".to_string(),
            toolchain: RustToolchainIdentity {
                rustc: "rustc 1.0.0".to_string(),
                cargo: "cargo 1.0.0".to_string(),
                channel: "stable".to_string(),
                host: "x86_64-unknown-linux-gnu".to_string(),
            },
            target_triple: "x86_64-unknown-linux-gnu".to_string(),
            profile: "release".to_string(),
            inputs: RustPlanInputs {
                features_hash: "f".to_string(),
                rustflags_hash: "r".to_string(),
                env_hash: "e".to_string(),
                lockfile_hash: "l".to_string(),
                cargo_config_hash: "c".to_string(),
                manifest_hashes: vec![],
            },
            packages: RustPlanPackages {
                selected_package_ids: vec![],
                workspace_package_ids: vec![],
                excluded_path_package_ids: vec![],
            },
            allowed_artifact_classes: vec!["cargo_fingerprint"],
            dropped_artifact_classes: vec![],
            cache_schema_version: 1,
            journal_log_path: None,
        };

        let json = serde_json::to_string(&plan).expect("serialize thin-v1 plan");
        assert!(
            !json.contains("\"cache_profile\""),
            "thin-v1 plan must NOT serialize cache_profile (zccache v1.4.0 \
             rejects unknown fields); got: {json}"
        );
        assert!(
            !json.contains("\"dropped_artifact_classes\""),
            "thin-v1 plan must NOT serialize dropped_artifact_classes; got: {json}"
        );
    }

    /// `full` mode also predates the new fields and zccache's strict
    /// deserializer rejects them, so `cache_profile == None` plus an empty
    /// drop list must serialize without either field.
    #[test]
    fn rust_artifact_plan_full_mode_json_omits_new_fields() {
        let plan = RustArtifactPlan {
            schema_version: 1,
            mode: "full".to_string(),
            cache_profile: None,
            workspace_root: "/tmp/ws".to_string(),
            target_dir: "/tmp/ws/target".to_string(),
            toolchain: RustToolchainIdentity {
                rustc: "rustc 1.0.0".to_string(),
                cargo: "cargo 1.0.0".to_string(),
                channel: "stable".to_string(),
                host: "x86_64-unknown-linux-gnu".to_string(),
            },
            target_triple: "x86_64-unknown-linux-gnu".to_string(),
            profile: "release".to_string(),
            inputs: RustPlanInputs {
                features_hash: "f".to_string(),
                rustflags_hash: "r".to_string(),
                env_hash: "e".to_string(),
                lockfile_hash: "l".to_string(),
                cargo_config_hash: "c".to_string(),
                manifest_hashes: vec![],
            },
            packages: RustPlanPackages {
                selected_package_ids: vec![],
                workspace_package_ids: vec![],
                excluded_path_package_ids: vec![],
            },
            allowed_artifact_classes: vec![],
            dropped_artifact_classes: vec![],
            cache_schema_version: 1,
            journal_log_path: None,
        };

        let json = serde_json::to_string(&plan).expect("serialize full plan");
        assert!(!json.contains("\"cache_profile\""), "got: {json}");
        assert!(
            !json.contains("\"dropped_artifact_classes\""),
            "got: {json}"
        );
    }

    /// thin-v2 is the opt-in that ships the new wire fields. zccache
    /// builds that consume thin-v2 must see both `cache_profile` and the
    /// non-empty `dropped_artifact_classes` list.
    #[test]
    fn rust_artifact_plan_thin_v2_json_includes_new_fields() {
        let plan = RustArtifactPlan {
            schema_version: 1,
            mode: "thin".to_string(),
            cache_profile: Some("thin-v2"),
            workspace_root: "/tmp/ws".to_string(),
            target_dir: "/tmp/ws/target".to_string(),
            toolchain: RustToolchainIdentity {
                rustc: "rustc 1.0.0".to_string(),
                cargo: "cargo 1.0.0".to_string(),
                channel: "stable".to_string(),
                host: "x86_64-unknown-linux-gnu".to_string(),
            },
            target_triple: "x86_64-unknown-linux-gnu".to_string(),
            profile: "release".to_string(),
            inputs: RustPlanInputs {
                features_hash: "f".to_string(),
                rustflags_hash: "r".to_string(),
                env_hash: "e".to_string(),
                lockfile_hash: "l".to_string(),
                cargo_config_hash: "c".to_string(),
                manifest_hashes: vec![],
            },
            packages: RustPlanPackages {
                selected_package_ids: vec![],
                workspace_package_ids: vec![],
                excluded_path_package_ids: vec![],
            },
            allowed_artifact_classes: vec!["dep_info"],
            dropped_artifact_classes: vec!["rlib", "rmeta"],
            cache_schema_version: 2,
            journal_log_path: None,
        };

        let json = serde_json::to_string(&plan).expect("serialize thin-v2 plan");
        assert!(
            json.contains("\"cache_profile\":\"thin-v2\""),
            "thin-v2 must serialize cache_profile; got: {json}"
        );
        assert!(
            json.contains("\"dropped_artifact_classes\""),
            "thin-v2 must serialize dropped_artifact_classes; got: {json}"
        );
    }

    fn warm_restore_test_plan() -> RustArtifactPlan {
        RustArtifactPlan {
            schema_version: 1,
            mode: "thin".to_string(),
            cache_profile: Some("thin-v1"),
            workspace_root: "/tmp/ws".to_string(),
            target_dir: "/tmp/ws/target".to_string(),
            toolchain: RustToolchainIdentity {
                rustc: "rustc 1.0.0-test".to_string(),
                cargo: "cargo 1.0.0-test".to_string(),
                channel: "stable".to_string(),
                host: "x86_64-unknown-linux-gnu".to_string(),
            },
            target_triple: "x86_64-unknown-linux-gnu".to_string(),
            profile: "test".to_string(),
            inputs: RustPlanInputs {
                features_hash: "F".to_string(),
                rustflags_hash: "R".to_string(),
                env_hash: "E".to_string(),
                lockfile_hash: "L".to_string(),
                cargo_config_hash: "C".to_string(),
                manifest_hashes: vec!["M1".to_string(), "M2".to_string()],
            },
            packages: RustPlanPackages {
                selected_package_ids: vec!["serde@1.0.0".to_string()],
                workspace_package_ids: vec!["app@0.1.0".to_string()],
                excluded_path_package_ids: vec![],
            },
            allowed_artifact_classes: vec!["rlib", "rmeta"],
            dropped_artifact_classes: vec![],
            cache_schema_version: 1,
            journal_log_path: Some("/tmp/journal".to_string()),
        }
    }

    fn warm_restore_test_sentinel(plan: &RustArtifactPlan) -> WarmRestoreSentinel {
        WarmRestoreSentinel {
            schema_version: 1,
            plan_inputs_hash: compute_plan_inputs_hash(plan),
            target_dir: plan.target_dir.clone(),
            github_run_id: "111".to_string(),
            github_job: "test".to_string(),
            github_run_attempt: "1".to_string(),
            session_id: "session-1".to_string(),
            saved_at_unix_seconds: 1_000_000,
        }
    }

    /// The sentinel hash must change whenever any plan input cargo would
    /// consult to decide freshness changes. Otherwise the warm-restore
    /// short-circuit could fire across step pairs that are not actually
    /// equivalent.
    #[test]
    fn plan_inputs_hash_changes_when_inputs_change() {
        let plan_a = warm_restore_test_plan();
        let mut plan_b = warm_restore_test_plan();
        plan_b.inputs.lockfile_hash = "different".to_string();
        assert_ne!(
            compute_plan_inputs_hash(&plan_a),
            compute_plan_inputs_hash(&plan_b),
        );

        let mut plan_c = warm_restore_test_plan();
        plan_c.toolchain.rustc = "rustc 9.9.9".to_string();
        assert_ne!(
            compute_plan_inputs_hash(&plan_a),
            compute_plan_inputs_hash(&plan_c),
        );

        let mut plan_d = warm_restore_test_plan();
        plan_d.target_triple = "aarch64-apple-darwin".to_string();
        assert_ne!(
            compute_plan_inputs_hash(&plan_a),
            compute_plan_inputs_hash(&plan_d),
        );
    }

    /// Cosmetic plan fields (the journal path, the schema version we
    /// already pin to 1) must not leak into the sentinel hash, so an
    /// unrelated path swap does not invalidate the warm-restore optim.
    #[test]
    fn plan_inputs_hash_ignores_cosmetic_fields() {
        let plan_a = warm_restore_test_plan();
        let mut plan_b = warm_restore_test_plan();
        plan_b.journal_log_path = Some("/tmp/other-journal".to_string());
        plan_b.workspace_root = "/different/ws".to_string();
        assert_eq!(
            compute_plan_inputs_hash(&plan_a),
            compute_plan_inputs_hash(&plan_b),
        );
    }

    /// Happy path: sentinel proves the same plan was just saved into the
    /// same target dir from the same CI job/attempt — restore is skipped.
    #[test]
    fn warm_restore_skip_fires_on_exact_match() {
        let plan = warm_restore_test_plan();
        let sentinel = warm_restore_test_sentinel(&plan);
        let now = sentinel.saved_at_unix_seconds + 60;
        let inputs_hash = compute_plan_inputs_hash(&plan);
        let inputs = WarmRestoreSkipInputs {
            plan_inputs_hash: &inputs_hash,
            plan_target_dir: &plan.target_dir,
            github_run_id: &sentinel.github_run_id,
            github_job: &sentinel.github_job,
            github_run_attempt: &sentinel.github_run_attempt,
            now_unix_seconds: now,
            max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
        };
        let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
        assert!(result.is_some(), "expected skip; got {result:?}");
    }

    /// Plain "no sentinel on disk" must fall through to the normal restore.
    #[test]
    fn warm_restore_skip_falls_through_when_sentinel_missing() {
        let plan = warm_restore_test_plan();
        let inputs_hash = compute_plan_inputs_hash(&plan);
        let inputs = WarmRestoreSkipInputs {
            plan_inputs_hash: &inputs_hash,
            plan_target_dir: &plan.target_dir,
            github_run_id: "111",
            github_job: "test",
            github_run_attempt: "1",
            now_unix_seconds: 1_000_000,
            max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
        };
        assert!(evaluate_warm_restore_skip(None, &inputs).is_none());
    }

    /// Sentinel from a prior re-run attempt must NOT short-circuit into a
    /// fresh attempt — the action restored the cache from scratch and the
    /// `target/` mtimes are no longer guaranteed to be the live ones.
    #[test]
    fn warm_restore_skip_rejects_mismatched_run_attempt() {
        let plan = warm_restore_test_plan();
        let sentinel = warm_restore_test_sentinel(&plan);
        let now = sentinel.saved_at_unix_seconds + 60;
        let inputs_hash = compute_plan_inputs_hash(&plan);
        let inputs = WarmRestoreSkipInputs {
            plan_inputs_hash: &inputs_hash,
            plan_target_dir: &plan.target_dir,
            github_run_id: &sentinel.github_run_id,
            github_job: &sentinel.github_job,
            github_run_attempt: "2", // different attempt
            now_unix_seconds: now,
            max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
        };
        let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
        assert!(result.is_none());
    }

    /// Sentinel from a different job in the same workflow must not bleed
    /// across job boundaries even when the run id matches.
    #[test]
    fn warm_restore_skip_rejects_mismatched_job() {
        let plan = warm_restore_test_plan();
        let sentinel = warm_restore_test_sentinel(&plan);
        let now = sentinel.saved_at_unix_seconds + 60;
        let inputs_hash = compute_plan_inputs_hash(&plan);
        let inputs = WarmRestoreSkipInputs {
            plan_inputs_hash: &inputs_hash,
            plan_target_dir: &plan.target_dir,
            github_run_id: &sentinel.github_run_id,
            github_job: "other-job",
            github_run_attempt: &sentinel.github_run_attempt,
            now_unix_seconds: now,
            max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
        };
        let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
        assert!(result.is_none());
    }

    /// Sentinel for an unrelated target dir (e.g. a sibling workspace
    /// also writing into the shared bundle dir) must not short-circuit.
    #[test]
    fn warm_restore_skip_rejects_mismatched_target_dir() {
        let plan = warm_restore_test_plan();
        let sentinel = warm_restore_test_sentinel(&plan);
        let now = sentinel.saved_at_unix_seconds + 60;
        let inputs_hash = compute_plan_inputs_hash(&plan);
        let inputs = WarmRestoreSkipInputs {
            plan_inputs_hash: &inputs_hash,
            plan_target_dir: "/tmp/different-target",
            github_run_id: &sentinel.github_run_id,
            github_job: &sentinel.github_job,
            github_run_attempt: &sentinel.github_run_attempt,
            now_unix_seconds: now,
            max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
        };
        let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
        assert!(result.is_none());
    }

    /// Once a plan input changes (lockfile bump, new manifest, etc.) the
    /// sentinel hash diverges and restore must run normally.
    #[test]
    fn warm_restore_skip_rejects_mismatched_inputs_hash() {
        let plan = warm_restore_test_plan();
        let mut sentinel = warm_restore_test_sentinel(&plan);
        sentinel.plan_inputs_hash = "stale-hash".to_string();
        let now = sentinel.saved_at_unix_seconds + 60;
        let inputs_hash = compute_plan_inputs_hash(&plan);
        let inputs = WarmRestoreSkipInputs {
            plan_inputs_hash: &inputs_hash,
            plan_target_dir: &plan.target_dir,
            github_run_id: &sentinel.github_run_id,
            github_job: &sentinel.github_job,
            github_run_attempt: &sentinel.github_run_attempt,
            now_unix_seconds: now,
            max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
        };
        let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
        assert!(result.is_none());
    }

    /// Stale sentinels (older than the configured window) must not
    /// short-circuit. Otherwise a leftover sentinel from a previous
    /// workflow run could cause skipping in a fresh job that happened to
    /// inherit the same env identifiers.
    #[test]
    fn warm_restore_skip_rejects_stale_sentinel() {
        let plan = warm_restore_test_plan();
        let sentinel = warm_restore_test_sentinel(&plan);
        let now = sentinel.saved_at_unix_seconds + WARM_RESTORE_MAX_AGE_SECONDS + 1;
        let inputs_hash = compute_plan_inputs_hash(&plan);
        let inputs = WarmRestoreSkipInputs {
            plan_inputs_hash: &inputs_hash,
            plan_target_dir: &plan.target_dir,
            github_run_id: &sentinel.github_run_id,
            github_job: &sentinel.github_job,
            github_run_attempt: &sentinel.github_run_attempt,
            now_unix_seconds: now,
            max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
        };
        let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
        assert!(result.is_none());
    }

    /// A future-version sentinel (say after a soldr upgrade that bumps
    /// the schema) must be ignored, never crash, and force a normal
    /// restore on the next invocation.
    #[test]
    fn warm_restore_skip_rejects_unknown_schema_version() {
        let plan = warm_restore_test_plan();
        let mut sentinel = warm_restore_test_sentinel(&plan);
        sentinel.schema_version = 99;
        let now = sentinel.saved_at_unix_seconds + 60;
        let inputs_hash = compute_plan_inputs_hash(&plan);
        let inputs = WarmRestoreSkipInputs {
            plan_inputs_hash: &inputs_hash,
            plan_target_dir: &plan.target_dir,
            github_run_id: &sentinel.github_run_id,
            github_job: &sentinel.github_job,
            github_run_attempt: &sentinel.github_run_attempt,
            now_unix_seconds: now,
            max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
        };
        let result = evaluate_warm_restore_skip(Some(&sentinel), &inputs);
        assert!(result.is_none());
    }

    /// Sentinel must round-trip as JSON without dropping fields, so
    /// disk-roundtrip behavior is observable here too (the
    /// filesystem-bound caller relies on serde to be exact).
    #[test]
    fn warm_restore_sentinel_round_trips_json() {
        let plan = warm_restore_test_plan();
        let sentinel = warm_restore_test_sentinel(&plan);
        let json = serde_json::to_string(&sentinel).expect("serialize sentinel");
        let parsed: WarmRestoreSentinel = serde_json::from_str(&json).expect("parse sentinel back");
        assert_eq!(parsed, sentinel);
    }

    /// Build a `RustArtifactPlanContext` whose plan-derived fields match
    /// `plan` and whose filesystem-touching paths live under `tempdir`. The
    /// other fields are filled with deterministic placeholders so tests can
    /// inspect them without caring about the daemon plumbing they would
    /// drive in production.
    fn warm_restore_test_context(
        plan: &RustArtifactPlan,
        tempdir: &TempDir,
    ) -> RustArtifactPlanContext {
        let root = tempdir.path();
        RustArtifactPlanContext {
            path: root.join("plan.json"),
            zccache_binary: root.join("zccache"),
            cache_dir: root.join("cache"),
            zccache_daemon_cache_dir: root.join("daemon"),
            session_id: "session-test".to_string(),
            journal_path: root.join("journal"),
            backend: "fs".to_string(),
            cache_profile: Some("thin-v1"),
            plan_inputs_hash: compute_plan_inputs_hash(plan),
            target_dir: plan.target_dir.clone(),
        }
    }

    /// With the gating env var enabled, `write_warm_restore_sentinel` must
    /// materialise a JSON sentinel under the plan's cache dir whose fields
    /// reflect the plan inputs and the current GitHub Actions env. This is
    /// the producer half of the warm-restore short-circuit.
    #[test]
    fn write_warm_restore_sentinel_emits_matching_json_when_enabled() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
        let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-42");
        let _job = EnvVarGuard::set("GITHUB_JOB", "test-job");
        let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "3");

        let tempdir = TempDir::new().expect("create tempdir");
        let plan = warm_restore_test_plan();
        let ctx = warm_restore_test_context(&plan, &tempdir);

        write_warm_restore_sentinel(&ctx);

        let sentinel_path = warm_restore_sentinel_path(&ctx);
        let raw = std::fs::read_to_string(&sentinel_path)
            .expect("sentinel file should exist after write");
        let sentinel: WarmRestoreSentinel =
            serde_json::from_str(&raw).expect("sentinel JSON should parse");

        assert_eq!(sentinel.schema_version, 1);
        assert_eq!(sentinel.plan_inputs_hash, ctx.plan_inputs_hash);
        assert_eq!(sentinel.target_dir, ctx.target_dir);
        assert_eq!(sentinel.github_run_id, "run-42");
        assert_eq!(sentinel.github_job, "test-job");
        assert_eq!(sentinel.github_run_attempt, "3");
        assert_eq!(sentinel.session_id, ctx.session_id);
    }

    /// When the gating env var is explicitly opted out (falsy value), the
    /// producer must be a strict no-op so the short-circuit cannot
    /// accidentally fire on the next invocation. No sentinel file should
    /// appear on disk.
    #[test]
    fn write_warm_restore_sentinel_is_noop_when_disabled() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "0");

        let tempdir = TempDir::new().expect("create tempdir");
        let plan = warm_restore_test_plan();
        let ctx = warm_restore_test_context(&plan, &tempdir);

        write_warm_restore_sentinel(&ctx);

        let sentinel_path = warm_restore_sentinel_path(&ctx);
        assert!(
            !sentinel_path.exists(),
            "no sentinel should be written when {SKIP_WARM_RESTORE_ENV_VAR} is set to a falsy value"
        );
    }

    /// Full filesystem round-trip: write a sentinel that exactly matches
    /// the current plan and CI env, then ask `should_skip_warm_restore`
    /// whether it should fire. The short-circuit must return `Some` with
    /// a non-empty operator-visible reason string.
    #[test]
    fn should_skip_warm_restore_returns_some_on_full_match() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
        let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-7");
        let _job = EnvVarGuard::set("GITHUB_JOB", "build");
        let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

        let tempdir = TempDir::new().expect("create tempdir");
        let plan = warm_restore_test_plan();
        let ctx = warm_restore_test_context(&plan, &tempdir);
        let sentinel_path = warm_restore_sentinel_path(&ctx);
        std::fs::create_dir_all(sentinel_path.parent().expect("sentinel has parent dir"))
            .expect("create sentinel parent");
        let sentinel = WarmRestoreSentinel {
            schema_version: 1,
            plan_inputs_hash: ctx.plan_inputs_hash.clone(),
            target_dir: ctx.target_dir.clone(),
            github_run_id: "run-7".to_string(),
            github_job: "build".to_string(),
            github_run_attempt: "1".to_string(),
            session_id: "session-prev".to_string(),
            saved_at_unix_seconds: super::current_unix_seconds(),
        };
        std::fs::write(
            &sentinel_path,
            serde_json::to_string(&sentinel).expect("serialize sentinel"),
        )
        .expect("write sentinel");

        let result = should_skip_warm_restore(&ctx);
        let reason = result.expect("expected Some(reason) on full match");
        assert!(
            !reason.is_empty(),
            "skip reason should be non-empty for operator visibility"
        );
    }

    /// A sentinel left behind by a previous invocation with a different
    /// `plan_inputs_hash` (e.g. after a lockfile bump) must not fire the
    /// short-circuit even when the file is otherwise present and fresh.
    #[test]
    fn should_skip_warm_restore_returns_none_on_hash_mismatch() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
        let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-7");
        let _job = EnvVarGuard::set("GITHUB_JOB", "build");
        let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

        let tempdir = TempDir::new().expect("create tempdir");
        let plan = warm_restore_test_plan();
        let ctx = warm_restore_test_context(&plan, &tempdir);
        let sentinel_path = warm_restore_sentinel_path(&ctx);
        std::fs::create_dir_all(sentinel_path.parent().expect("sentinel has parent dir"))
            .expect("create sentinel parent");
        let sentinel = WarmRestoreSentinel {
            schema_version: 1,
            plan_inputs_hash: "stale-hash-from-previous-step".to_string(),
            target_dir: ctx.target_dir.clone(),
            github_run_id: "run-7".to_string(),
            github_job: "build".to_string(),
            github_run_attempt: "1".to_string(),
            session_id: "session-prev".to_string(),
            saved_at_unix_seconds: super::current_unix_seconds(),
        };
        std::fs::write(
            &sentinel_path,
            serde_json::to_string(&sentinel).expect("serialize sentinel"),
        )
        .expect("write sentinel");

        assert!(should_skip_warm_restore(&ctx).is_none());
    }

    /// When no sentinel file exists at all, the short-circuit must fall
    /// through without panicking on the missing-file IO error.
    #[test]
    fn should_skip_warm_restore_returns_none_when_sentinel_missing() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "1");
        let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-7");
        let _job = EnvVarGuard::set("GITHUB_JOB", "build");
        let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

        let tempdir = TempDir::new().expect("create tempdir");
        let plan = warm_restore_test_plan();
        let ctx = warm_restore_test_context(&plan, &tempdir);
        assert!(!warm_restore_sentinel_path(&ctx).exists());

        assert!(should_skip_warm_restore(&ctx).is_none());
    }

    /// With the gating env var explicitly opted out (`"0"`), the
    /// short-circuit must stay off even when a perfectly-matching sentinel
    /// exists. This is the safety property that lets operators disable the
    /// feature on demand without having to clear stale sentinel files.
    #[test]
    fn should_skip_warm_restore_returns_none_when_disabled_even_with_match() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, "0");
        let _run = EnvVarGuard::set("GITHUB_RUN_ID", "run-7");
        let _job = EnvVarGuard::set("GITHUB_JOB", "build");
        let _attempt = EnvVarGuard::set("GITHUB_RUN_ATTEMPT", "1");

        let tempdir = TempDir::new().expect("create tempdir");
        let plan = warm_restore_test_plan();
        let ctx = warm_restore_test_context(&plan, &tempdir);
        let sentinel_path = warm_restore_sentinel_path(&ctx);
        std::fs::create_dir_all(sentinel_path.parent().expect("sentinel has parent dir"))
            .expect("create sentinel parent");
        let sentinel = WarmRestoreSentinel {
            schema_version: 1,
            plan_inputs_hash: ctx.plan_inputs_hash.clone(),
            target_dir: ctx.target_dir.clone(),
            github_run_id: "run-7".to_string(),
            github_job: "build".to_string(),
            github_run_attempt: "1".to_string(),
            session_id: "session-prev".to_string(),
            saved_at_unix_seconds: super::current_unix_seconds(),
        };
        std::fs::write(
            &sentinel_path,
            serde_json::to_string(&sentinel).expect("serialize sentinel"),
        )
        .expect("write sentinel");

        assert!(should_skip_warm_restore(&ctx).is_none());
    }

    /// After the #229 validation flip, an unset env var must enable the
    /// short-circuit by default. This locks in the default-on contract so
    /// future refactors cannot regress it without updating the test.
    #[test]
    fn warm_restore_skip_enabled_defaults_on() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _skip = EnvVarGuard::remove(SKIP_WARM_RESTORE_ENV_VAR);

        assert!(
            warm_restore_skip_enabled(),
            "warm-restore skip must default to enabled when {SKIP_WARM_RESTORE_ENV_VAR} is unset"
        );
    }

    /// The default-on flip preserves an explicit opt-out path: each of the
    /// recognised falsy spellings (`0`, `false`, `no`, `off`, empty string,
    /// case-insensitive) must disable the short-circuit.
    #[test]
    fn warm_restore_skip_enabled_respects_explicit_falsy() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        for value in ["0", "false", "FALSE", "No", "off", "OFF", "", "  0  "] {
            let _skip = EnvVarGuard::set(SKIP_WARM_RESTORE_ENV_VAR, value);
            assert!(
                !warm_restore_skip_enabled(),
                "warm-restore skip must be disabled when {SKIP_WARM_RESTORE_ENV_VAR} is set to {value:?}"
            );
        }
    }

    // -------- stderr_indicates_unknown_session (issue #265) --------

    #[test]
    fn unknown_session_detector_rejects_empty_stderr() {
        assert!(!stderr_indicates_unknown_session(b""));
    }

    #[test]
    fn unknown_session_detector_matches_exact_zccache_line() {
        let stderr = b"zccache error: unknown session: abc-123\n";
        assert!(stderr_indicates_unknown_session(stderr));
    }

    #[test]
    fn unknown_session_detector_matches_substring_mid_line() {
        // The marker can appear anywhere in the stream, not necessarily at
        // the start of a line.
        let stderr = b"prelude blah blah unknown session: 0000 trailing\n";
        assert!(stderr_indicates_unknown_session(stderr));
    }

    #[test]
    fn unknown_session_detector_ignores_unrelated_session_mentions() {
        // The word "session" alone is not enough; we only treat the literal
        // "unknown session:" marker as a resync trigger.
        let stderr = b"zccache info: session started\nzccache info: session ok\n";
        assert!(!stderr_indicates_unknown_session(stderr));
    }

    #[test]
    fn unknown_session_detector_tolerates_non_utf8_bytes() {
        // Surround the marker with raw non-UTF-8 byte sequences; the
        // detector must not panic and must still find the literal needle.
        let mut stderr: Vec<u8> = vec![0xFF, 0xFE, 0x80, 0x81];
        stderr.extend_from_slice(b"zccache error: unknown session: deadbeef\n");
        stderr.extend_from_slice(&[0xC3, 0x28, 0xA0]);
        assert!(stderr_indicates_unknown_session(&stderr));
    }

    #[test]
    fn unknown_session_detector_rejects_partial_marker() {
        // "unknown sessio" (missing the trailing "n:") must NOT match — we
        // only resync on the exact daemon-emitted marker.
        let stderr = b"unknown sessio\n";
        assert!(!stderr_indicates_unknown_session(stderr));
    }

    #[test]
    fn tar_threads_unset_or_blank_yields_none() {
        assert!(parse_rust_artifact_cache_tar_threads("").unwrap().is_none());
        assert!(parse_rust_artifact_cache_tar_threads("   ")
            .unwrap()
            .is_none());
    }

    #[test]
    fn tar_threads_auto_is_normalized_lowercase() {
        assert_eq!(
            parse_rust_artifact_cache_tar_threads("auto").unwrap(),
            Some("auto".to_string())
        );
        assert_eq!(
            parse_rust_artifact_cache_tar_threads("  AUTO ").unwrap(),
            Some("auto".to_string())
        );
    }

    #[test]
    fn tar_threads_positive_integer_passes_through() {
        for raw in ["1", "4", "8", "16"] {
            assert_eq!(
                parse_rust_artifact_cache_tar_threads(raw).unwrap(),
                Some(raw.to_string())
            );
        }
    }

    #[test]
    fn tar_threads_rejects_zero_negative_and_garbage() {
        for raw in ["0", "-1", "1.5", "twelve", "auto4", "4 threads"] {
            let err = parse_rust_artifact_cache_tar_threads(raw)
                .expect_err(&format!("expected error for {raw:?}"));
            let msg = err.to_string();
            assert!(
                msg.contains("SOLDR_TARGET_CACHE_TAR_THREADS"),
                "error for {raw:?} must mention the env var, got {msg}"
            );
        }
    }

    /// Unset / `auto` / case-variants of `auto` must all yield `None`, which
    /// signals "use rayon's global thread pool" to `walk_bundle_files`.
    #[test]
    fn bundle_walk_thread_count_auto_yields_none() {
        for raw in ["", "  ", "auto", "AUTO", " Auto "] {
            assert_eq!(
                resolve_bundle_walk_thread_count(raw).unwrap(),
                None,
                "raw {raw:?} should resolve to None (auto)"
            );
        }
    }

    /// An explicit `1` must turn into `Some(1)` so the walk takes the
    /// sequential fallback path (no rayon overhead).
    #[test]
    fn bundle_walk_thread_count_one_forces_sequential() {
        assert_eq!(resolve_bundle_walk_thread_count("1").unwrap(), Some(1));
    }

    /// In-range explicit counts pass through unmodified; values above the
    /// internal cap are clamped down to `BUNDLE_WALK_THREAD_CAP`.
    #[test]
    fn bundle_walk_thread_count_clamps_to_cap() {
        assert_eq!(resolve_bundle_walk_thread_count("2").unwrap(), Some(2));
        assert_eq!(
            resolve_bundle_walk_thread_count("8").unwrap(),
            Some(BUNDLE_WALK_THREAD_CAP)
        );
        // 64 → capped at BUNDLE_WALK_THREAD_CAP.
        assert_eq!(
            resolve_bundle_walk_thread_count("64").unwrap(),
            Some(BUNDLE_WALK_THREAD_CAP)
        );
        assert_eq!(
            resolve_bundle_walk_thread_count("9999").unwrap(),
            Some(BUNDLE_WALK_THREAD_CAP)
        );
    }

    /// Garbage values inherited from the parser must still propagate as
    /// errors here so callers on the bare `RUSTC_WRAPPER` passthrough path
    /// (which bypasses the cargo front-door validation) get a clear message
    /// instead of a silent default.
    #[test]
    fn bundle_walk_thread_count_rejects_garbage() {
        for raw in ["0", "twelve", "1.5"] {
            let err = resolve_bundle_walk_thread_count(raw)
                .expect_err(&format!("expected error for {raw:?}"));
            assert!(
                err.to_string().contains("SOLDR_TARGET_CACHE_TAR_THREADS"),
                "error must reference the env var name"
            );
        }
    }

    /// Build a bundle layout with a handful of files at varying depths and
    /// verify that the walker returns one entry per regular file with the
    /// correct relative path string (forward-slashed, root-relative).
    fn populate_walk_bundle_fixture(root: &std::path::Path) {
        std::fs::create_dir_all(root.join("debug/deps")).unwrap();
        std::fs::create_dir_all(root.join("debug/build")).unwrap();
        std::fs::write(root.join("debug/deps/a.rlib"), b"alpha").unwrap();
        std::fs::write(root.join("debug/deps/b.rmeta"), b"beta!!").unwrap();
        std::fs::write(root.join("debug/build/c.txt"), b"gamma").unwrap();
        std::fs::write(root.join("top.txt"), b"delta-delta").unwrap();
    }

    /// The sequential path (`Some(1)`) must enumerate every file with the
    /// expected relative paths and sizes. This is the baseline against which
    /// the parallel walks are compared for determinism.
    #[test]
    fn walk_bundle_files_sequential_lists_every_file_with_size() {
        let bundle = tempfile::tempdir().expect("tempdir");
        populate_walk_bundle_fixture(bundle.path());

        let mut entries =
            walk_bundle_files(bundle.path(), Some(1)).expect("sequential walk must succeed");
        entries.sort_by(|a, b| a.path.cmp(&b.path));

        let observed: Vec<_> = entries
            .iter()
            .map(|e| (e.path.as_str(), e.size_bytes))
            .collect();
        assert_eq!(
            observed,
            vec![
                ("debug/build/c.txt", Some(5)),
                ("debug/deps/a.rlib", Some(5)),
                ("debug/deps/b.rmeta", Some(6)),
                ("top.txt", Some(11)),
            ]
        );
    }

    /// Output of the walk must be byte-identical (after the caller's
    /// canonical sort) regardless of whether the metadata phase ran
    /// sequentially, on rayon's global pool, or on a scoped explicit pool.
    /// This is the determinism acceptance criterion from issue #272.
    #[test]
    fn walk_bundle_files_parallel_matches_sequential_after_sort() {
        let bundle = tempfile::tempdir().expect("tempdir");
        populate_walk_bundle_fixture(bundle.path());

        let mut sequential =
            walk_bundle_files(bundle.path(), Some(1)).expect("sequential walk must succeed");
        sequential.sort_by(|a, b| a.path.cmp(&b.path));

        for thread_count in [None, Some(2), Some(BUNDLE_WALK_THREAD_CAP)] {
            let mut parallel = walk_bundle_files(bundle.path(), thread_count)
                .unwrap_or_else(|e| panic!("walk failed with thread_count {thread_count:?}: {e}"));
            parallel.sort_by(|a, b| a.path.cmp(&b.path));
            assert_eq!(
                parallel, sequential,
                "thread_count {thread_count:?} produced a different file list after canonical sort"
            );
        }
    }

    /// A missing root is not an error — the bundle may legitimately not
    /// exist yet (e.g. zccache restore produced nothing). The walk must
    /// return an empty vec rather than propagating a `NotFound` IO error.
    #[test]
    fn walk_bundle_files_missing_root_returns_empty() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let missing = bundle.path().join("never-created");
        for thread_count in [Some(1), None, Some(4)] {
            let entries = walk_bundle_files(&missing, thread_count)
                .unwrap_or_else(|e| panic!("missing root must not error ({thread_count:?}): {e}"));
            assert!(
                entries.is_empty(),
                "missing root walk with {thread_count:?} should be empty, got {entries:?}"
            );
        }
    }
}
