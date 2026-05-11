use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use soldr_core::{SoldrError, SoldrPaths};
use soldr_fetch::VersionSpec;
use std::collections::BTreeSet;

const TEST_CARGO_BIN_ENV_VAR: &str = "SOLDR_TEST_CARGO_BIN";
const TEST_RUSTC_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTC_BIN";
const TEST_RUSTUP_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTUP_BIN";
const TEST_ZCCACHE_BIN_ENV_VAR: &str = "SOLDR_TEST_ZCCACHE_BIN";
const JSON_SCHEMA_VERSION: u32 = 1;
const RUSTC_WRAPPER_OVERRIDE_ENV_VAR: &str = "SOLDR_RUSTC_WRAPPER";
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
    },
    /// Show version
    Version {
        /// Emit the stable machine-facing JSON form for this command
        #[arg(long)]
        json: bool,
    },
    /// Garbage-collect stale Cargo `target/` directories tracked by
    /// the soldr registry (`~/.soldr/data.db`).
    ///
    /// Aliases: `purge-targets` (matches issue #234's `soldr --purge`
    /// wording).
    #[command(alias = "purge-targets")]
    Gc {
        /// Show candidates and totals without deleting anything.
        #[arg(long)]
        dry_run: bool,
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
    /// Anything else is a tool to fetch and run
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[tokio::main]
async fn main() {
    // RUSTC_WRAPPER mode: cargo passes `soldr /path/to/rustc <args...>`
    // Must be checked before clap parsing.
    let raw_args: Vec<String> = std::env::args().collect();
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

    // Dispatch path only: one-shot stale-target/ warning, throttled
    // to once per day. Never runs on the rustc-wrapper hot path.
    emit_startup_target_warning_if_due();

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
        Commands::Cache { json } => {
            let output = collect_cache_output()?;
            if json {
                print_json(&output)?;
            } else {
                print_cache_output(&output);
            }
        }
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
        } => {
            run_gc_command(GcInvocation {
                dry_run,
                all,
                older_than,
                larger_than,
                json,
            })?;
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

            let status = std::process::Command::new(&result.binary_path)
                .args(tool_args)
                .status()?;

            std::process::exit(status.code().unwrap_or(1));
        }
    }

    Ok(())
}

fn report_and_exit(error: SoldrError) -> i32 {
    eprintln!("soldr: {error}");
    1
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
    // can't resolve a workspace target dir cheaply, or the sqlite
    // upsert fails for any reason, skip silently — never fail a build.
    if tool_stem == "rustc" {
        record_target_dir_in_registry(&raw_args[2..]);
    }

    // Only route through zccache for actual rustc invocations, not
    // clippy-driver or other workspace wrappers.
    if tool_stem == "rustc" && soldr_cache::cache_enabled_in_current_process() {
        if let Some(zccache) = zccache_binary_override() {
            return run_wrapper_through_zccache(raw_args, &zccache);
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
    command.args(&raw_args[2..]);
    apply_implicit_toolchain_homes(&mut command);
    let status = command.status()?;

    Ok(status.code().unwrap_or(1))
}

/// Run a rustup-managed toolchain binary with pass-through args.
fn run_toolchain_passthrough(tool: &str, args: &[String]) -> Result<i32, SoldrError> {
    let binary = resolve_toolchain_binary(tool)?;
    let mut command = std::process::Command::new(binary);
    command.args(args);
    apply_implicit_toolchain_homes(&mut command);
    let status = command.status()?;
    Ok(status.code().unwrap_or(1))
}

fn run_wrapper_through_zccache(
    raw_args: &[String],
    zccache: &std::path::Path,
) -> Result<i32, SoldrError> {
    let mut command = std::process::Command::new(zccache);
    command.args(&raw_args[1..]);

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
    command.env("RUSTC", &rustc);
    let cache_enabled_for_cargo = cache_enabled && cargo_args_are_cacheable(args);

    command.env(
        soldr_cache::CACHE_ENABLED_ENV_VAR,
        soldr_cache::cache_enabled_env_value(cache_enabled_for_cargo),
    );
    let mut path_dirs: Vec<std::path::PathBuf> = Vec::with_capacity(1 + extra_bin_dirs.len());
    path_dirs.push(cargo_bin_dir);
    path_dirs.extend(extra_bin_dirs);
    command.env("PATH", prepend_paths(&path_dirs, existing_path.as_deref())?);
    if let Some(target) = default_cargo_build_target(args)? {
        command.env("CARGO_BUILD_TARGET", target);
    }

    let session = if cache_enabled_for_cargo {
        prepare_rustc_wrapper(&mut command, &paths).await?
    } else {
        None
    };

    let rust_plan = if let Some(session) = session.as_ref() {
        maybe_prepare_rust_artifact_plan(&cargo, &rustc, args, session)?
    } else {
        None
    };
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

    let metadata = cargo_metadata(cargo, args)?;
    let toolchain = rust_toolchain_identity(cargo, rustc)?;
    let plan = build_rust_artifact_plan(&metadata, &toolchain, args, &mode, profile, session)?;
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
            env_hash: stable_hash_json(&build_env_inputs()),
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
    let mut files = Vec::new();
    walk_bundle_files(bundle_root, bundle_root, &mut files)?;
    // Drop any prior manifest so the file list does not chase its own tail
    // across repeated saves into the same bundle directory.
    files.retain(|entry| entry.path != THIN_MANIFEST_FILENAME);
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

fn walk_bundle_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<ThinSliceManifestEntry>,
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
            walk_bundle_files(root, &path, out)?;
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
            let size_bytes = std::fs::metadata(&path).ok().map(|m| m.len());
            out.push(ThinSliceManifestEntry {
                path: rel_string,
                size_bytes,
            });
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

fn build_env_inputs() -> Vec<(String, String)> {
    sorted_env_vars(|name| {
        name == "CARGO_BUILD_TARGET"
            || name == "CARGO_TARGET_DIR"
            || name.starts_with("CARGO_PROFILE_")
            || name.starts_with("CARGO_CFG_")
    })
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
    })
}

fn finish_zccache_build(session: &ZccacheBuildSession) -> Result<(), SoldrError> {
    let output = run_zccache_command_in_cache_dir(
        &session.binary_path,
        &["session-end", &session.session_id],
        &session.cache_dir,
    )?;
    if session.session_log_path.exists() {
        eprintln!(
            "soldr: zccache session log: {}",
            session.session_log_path.display()
        );
    }
    let stdout = output.stdout.trim();
    if !stdout.is_empty() {
        eprintln!("soldr: zccache session summary");
        eprintln!("{stdout}");
    }
    Ok(())
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

struct GcInvocation {
    dry_run: bool,
    all: bool,
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
    dry_run: bool,
    candidates: Vec<GcCandidateOutput>,
    skipped: Vec<GcCandidateOutput>,
    dropped_missing: usize,
    deleted_paths: Vec<String>,
}

fn run_gc_command(invocation: GcInvocation) -> Result<(), SoldrError> {
    use soldr_cache::gc::{parse_duration, parse_size, purge_one, scan, GcOptions};

    let older_than = parse_duration(&invocation.older_than).map_err(SoldrError::Other)?;
    let larger_than = parse_size(&invocation.larger_than).map_err(SoldrError::Other)?;

    let paths = SoldrPaths::new()?;
    let dev_roots = resolve_gc_dev_roots(&paths);
    let db_path = soldr_cache::data_db_path(&paths);
    let registry = soldr_cache::target_registry::TargetRegistry::open(&db_path)
        .map_err(|e| SoldrError::Other(format!("failed to open soldr registry: {e}")))?;

    let options = GcOptions {
        older_than_seconds: older_than,
        larger_than_bytes: larger_than,
        dev_roots,
        dry_run: invocation.dry_run,
    };

    let report =
        scan(&registry, &options).map_err(|e| SoldrError::Other(format!("gc scan failed: {e}")))?;

    let mut deleted_paths: Vec<String> = Vec::new();

    if !invocation.json {
        eprintln!(
            "soldr gc: scanned registry at {} ({} candidate dir{}, {} skipped, {} dropped missing)",
            db_path.display(),
            report.candidates.len(),
            if report.candidates.len() == 1 {
                ""
            } else {
                "s"
            },
            report.skipped.len(),
            report.dropped_missing
        );

        if report.candidates.is_empty() {
            eprintln!("soldr gc: nothing to reclaim.");
        } else {
            eprintln!("soldr gc: candidates");
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

    for cand in &report.candidates {
        let should_delete = if invocation.dry_run {
            false
        } else if invocation.all {
            true
        } else {
            prompt_yes_no(&format!(
                "soldr gc: delete {} ({}, age {}) ? [y/N] ",
                cand.path.display(),
                soldr_cache::target_registry::human_size(cand.size_bytes),
                soldr_cache::target_registry::human_age(cand.age_seconds),
            ))
        };

        if should_delete {
            match purge_one(&registry, &cand.path, false) {
                Ok(true) => {
                    if !invocation.json {
                        eprintln!("soldr gc: deleted {}", cand.path.display());
                    }
                    deleted_paths.push(cand.path.display().to_string());
                }
                Ok(false) => {
                    if !invocation.json {
                        eprintln!(
                            "soldr gc: nothing to delete at {} (already gone)",
                            cand.path.display()
                        );
                    }
                }
                Err(e) => {
                    eprintln!("soldr gc: failed to delete {}: {e}", cand.path.display());
                }
            }
        }
    }

    if invocation.json {
        let output = GcOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "gc",
            dry_run: invocation.dry_run,
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
        };
        print_json(&output)?;
    }
    Ok(())
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

fn prompt_yes_no(prompt: &str) -> bool {
    use std::io::{BufRead, Write};
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
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

fn collect_zccache_status(paths: &SoldrPaths) -> Result<ZccacheStatusSnapshot, SoldrError> {
    let zccache_dir = managed_zccache_cache_dir(paths)?;
    let session_log_path = soldr_cache::session_log_path(&zccache_dir);
    let session_log_present = session_log_path.exists();
    let journal_path = soldr_cache::session_journal_path(&zccache_dir);
    let journal_present = journal_path.exists();

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
        extract_as_pin, first_cargo_subcommand, is_sccache_wrapper, normalize_version,
        parse_tool_spec, rustc_wrapper_mode_from_env_var, rustup_resolution_failure,
        selected_cargo_args, should_skip_warm_restore, should_trampoline,
        stderr_indicates_unknown_session, warm_restore_sentinel_path, warm_restore_skip_enabled,
        write_thin_manifest, write_warm_restore_sentinel, CargoMetadata, CargoMetadataPackage,
        RustArtifactPlan, RustArtifactPlanContext, RustPlanInputs, RustPlanPackages,
        RustToolchainIdentity, RustcWrapperMode, ThinSliceManifest, WarmRestoreSentinel,
        WarmRestoreSkipInputs, ZccacheBuildSession, SKIP_WARM_RESTORE_ENV_VAR,
        THIN_MANIFEST_FILENAME, WARM_RESTORE_MAX_AGE_SECONDS,
    };
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
        };

        let plan = build_rust_artifact_plan(
            &metadata,
            &toolchain,
            &["build".to_string()],
            "thin",
            Some("thin-v2"),
            &session,
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
}
