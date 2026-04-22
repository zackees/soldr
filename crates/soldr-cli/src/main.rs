use clap::{Parser, Subcommand};
use serde::Serialize;
use soldr_core::{SoldrError, SoldrPaths};
use soldr_fetch::VersionSpec;

const TEST_CARGO_BIN_ENV_VAR: &str = "SOLDR_TEST_CARGO_BIN";
const TEST_RUSTC_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTC_BIN";
const TEST_RUSTUP_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTUP_BIN";
const TEST_ZCCACHE_BIN_ENV_VAR: &str = "SOLDR_TEST_ZCCACHE_BIN";
const JSON_SCHEMA_VERSION: u32 = 1;
const RUSTC_WRAPPER_OVERRIDE_ENV_VAR: &str = "SOLDR_RUSTC_WRAPPER";

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
    /// Clear caches
    Clean,
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
    let pinned_version = pinned_version.or_else(|| {
        std::env::var(SOLDR_AS_ENV_VAR)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    });

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

    let status = std::process::Command::new(&result.binary_path)
        .args(args)
        .env(SOLDR_TRAMPOLINING_ENV_VAR, env!("CARGO_PKG_VERSION"))
        .status()
        .map_err(|e| {
            SoldrError::Other(format!(
                "failed to exec soldr v{} at {}: {e}",
                result.version,
                result.binary_path.display()
            ))
        })?;

    Ok(status.code().unwrap_or(1))
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

    let mut command = std::process::Command::new(cargo);
    command.args(args);
    apply_implicit_toolchain_homes(&mut command);
    command.env("RUSTC", rustc);
    command.env(
        soldr_cache::CACHE_ENABLED_ENV_VAR,
        soldr_cache::cache_enabled_env_value(cache_enabled),
    );
    let mut path_dirs: Vec<std::path::PathBuf> = Vec::with_capacity(1 + extra_bin_dirs.len());
    path_dirs.push(cargo_bin_dir);
    path_dirs.extend(extra_bin_dirs);
    command.env("PATH", prepend_paths(&path_dirs, existing_path.as_deref())?);
    if let Some(target) = default_cargo_build_target(args)? {
        command.env("CARGO_BUILD_TARGET", target);
    }

    let session = if cache_enabled {
        prepare_rustc_wrapper(&mut command, &paths).await?
    } else {
        None
    };

    let status = command.status()?;
    if let Some(session) = session {
        finish_zccache_build(&session)?;
    }
    Ok(status.code().unwrap_or(1))
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
    for arg in args {
        if arg == "--" {
            break;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.as_str());
    }
    None
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
    if let Some(path) = soldr_core::probe_toolchain_binary(tool, start_dir.as_deref()) {
        return Ok(path);
    }

    let mut command = std::process::Command::new(rustup_binary());
    command.args(["which", tool]);
    apply_implicit_toolchain_homes(&mut command);
    let output = command.output()?;

    if !output.status.success() {
        return Err(rustup_resolution_failure(tool, &output.stderr));
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(SoldrError::Other(format!(
            "rustup did not return a path for {tool}"
        )));
    }

    Ok(path.into())
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
                cargo.env("SCCACHE_DIR", soldr_cache::sccache_dir(paths));
            }
            cargo.env("RUSTC_WRAPPER", wrapper);
            cargo.env_remove(soldr_cache::ZCCACHE_BINARY_ENV_VAR);
            cargo.env_remove(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR);
            Ok(None)
        }
        RustcWrapperMode::Disabled => {
            cargo.env_remove("RUSTC_WRAPPER");
            cargo.env_remove(soldr_cache::ZCCACHE_BINARY_ENV_VAR);
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

    run_zccache_command_in_cache_dir(&fetch.binary_path, &["start"], &zccache_dir)?;

    let journal_path = soldr_cache::session_journal_path(&zccache_dir);
    let journal_path = journal_path.display().to_string();
    let session_json = run_zccache_command_in_cache_dir(
        &fetch.binary_path,
        &["session-start", "--stats", "--journal", &journal_path],
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
    cargo.env(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR, &session_id);

    Ok(ZccacheBuildSession {
        binary_path: fetch.binary_path,
        cache_dir: zccache_dir,
        session_id,
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
    if let Some(explicit) = non_empty_env_path(soldr_cache::ZCCACHE_CACHE_DIR_ENV_VAR) {
        let explicit = normalize_path_for_compare(&explicit)?;
        if explicit != zccache_dir {
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

fn run_zccache_command_with_env(
    binary: &std::path::Path,
    args: &[&str],
    envs: &[(&str, &std::ffi::OsStr)],
) -> Result<CommandOutput, SoldrError> {
    let mut command = std::process::Command::new(binary);
    command.args(args);
    for &(name, value) in envs {
        command.env(name, value);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "zccache {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
    })
}

fn toolchain_binary_override(tool: &str) -> Option<std::path::PathBuf> {
    let env_var = match tool {
        "cargo" => TEST_CARGO_BIN_ENV_VAR,
        "rustc" => TEST_RUSTC_BIN_ENV_VAR,
        _ => return None,
    };
    non_empty_env_path(env_var)
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
        cargo_args_specify_target, cargo_args_use_reserved_no_cache, extract_as_pin,
        first_cargo_subcommand, is_sccache_wrapper, normalize_version, parse_tool_spec,
        rustc_wrapper_mode_from_env_var, rustup_resolution_failure, should_trampoline,
        RustcWrapperMode,
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
