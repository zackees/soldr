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
        let err = command.exec();
        Err(SoldrError::Other(format!(
            "failed to exec zccache at {}: {err}",
            zccache.display()
        )))
    }

    #[cfg(not(unix))]
    {
        let status = command.status()?;
        Ok(status.code().unwrap_or(1))
    }
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
        run_zccache_rust_plan(plan, "restore", false)?;
    }

    let status = command.status()?;
    if status.success() {
        if let Some(plan) = rust_plan.as_ref() {
            run_zccache_rust_plan(plan, "save", true)?;
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

#[derive(Debug, Serialize)]
struct RustArtifactPlan {
    schema_version: u32,
    mode: String,
    workspace_root: String,
    target_dir: String,
    toolchain: RustToolchainIdentity,
    target_triple: String,
    profile: String,
    inputs: RustPlanInputs,
    packages: RustPlanPackages,
    allowed_artifact_classes: Vec<&'static str>,
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
    session_id: String,
    journal_path: std::path::PathBuf,
    backend: String,
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

    let metadata = cargo_metadata(cargo, args)?;
    let toolchain = rust_toolchain_identity(cargo, rustc)?;
    let plan = build_rust_artifact_plan(&metadata, &toolchain, args, &mode, session)?;
    let plan_dir = session.cache_dir.join("plans");
    std::fs::create_dir_all(&plan_dir)?;
    let plan_path = plan_dir.join("last-rust-artifact-plan.json");
    let plan_json = serde_json::to_string_pretty(&plan)
        .map_err(|e| SoldrError::Other(format!("failed to serialize Rust artifact plan: {e}")))?;
    std::fs::write(&plan_path, plan_json)?;

    Ok(Some(RustArtifactPlanContext {
        path: plan_path,
        zccache_binary: session.binary_path.clone(),
        cache_dir: rust_artifact_plan_cache_dir(session)?,
        session_id: session.session_id.clone(),
        journal_path: session.journal_path.clone(),
        backend: rust_artifact_cache_backend_from_env()?,
    }))
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

    Ok(RustArtifactPlan {
        schema_version: 1,
        mode: mode.to_string(),
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
        allowed_artifact_classes: allowed_artifact_classes(mode),
        cache_schema_version: 1,
        journal_log_path: Some(path_string(&session.journal_path)),
    })
}

fn allowed_artifact_classes(mode: &str) -> Vec<&'static str> {
    if mode == "full" {
        return Vec::new();
    }
    vec![
        "rlib",
        "rmeta",
        "dep_info",
        "proc_macro",
        "cargo_fingerprint",
        "build_script_metadata",
        "build_script_output",
    ]
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

    let output =
        run_zccache_command_strings_in_cache_dir(&plan.zccache_binary, &args, &plan.cache_dir)?;
    let stdout = output.stdout.trim();
    if !stdout.is_empty() {
        eprintln!("soldr: zccache rust-plan {operation} summary");
        eprintln!("{stdout}");
    }
    Ok(())
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

    let journal_path = soldr_cache::session_journal_path(&zccache_dir);
    let journal_path_arg = journal_path.display().to_string();
    let session_json = run_zccache_command_in_cache_dir(
        &fetch.binary_path,
        &["session-start", "--stats", "--journal", &journal_path_arg],
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
        journal_path,
    })
}

fn finish_zccache_build(session: &ZccacheBuildSession) -> Result<(), SoldrError> {
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
        allowed_artifact_classes, build_rust_artifact_plan, cargo_args_specify_target,
        cargo_args_use_reserved_no_cache, cargo_metadata_passthrough_args, cargo_profile,
        cargo_target_triple, extract_as_pin, first_cargo_subcommand, is_sccache_wrapper,
        normalize_version, parse_tool_spec, rustc_wrapper_mode_from_env_var,
        rustup_resolution_failure, selected_cargo_args, should_trampoline, CargoMetadata,
        CargoMetadataPackage, RustToolchainIdentity, RustcWrapperMode, ZccacheBuildSession,
    };
    use soldr_fetch::VersionSpec;
    use std::ffi::OsStr;

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

        let plan = build_rust_artifact_plan(&metadata, &toolchain, &args, "thin", &session)
            .expect("build rust artifact plan");

        assert_eq!(plan.schema_version, 1);
        assert_eq!(plan.mode, "thin");
        assert_eq!(plan.profile, "release");
        assert_eq!(plan.target_triple, "x86_64-unknown-linux-gnu");
        assert_eq!(plan.packages.workspace_package_ids.len(), 1);
        assert_eq!(plan.packages.selected_package_ids.len(), 1);
        assert!(plan.packages.selected_package_ids[0].contains("serde"));
        assert_eq!(plan.packages.excluded_path_package_ids.len(), 1);
        assert!(plan.allowed_artifact_classes.contains(&"cargo_fingerprint"));
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
        assert_eq!(allowed_artifact_classes("full"), Vec::<&str>::new());
        assert_eq!(
            cargo_metadata_passthrough_args(&args)
                .iter()
                .map(|value| value.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec!["--locked".to_string(), "--features=fast".to_string()]
        );
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
}
