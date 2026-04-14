use clap::{Parser, Subcommand};
use soldr_core::{SoldrError, SoldrPaths};
use soldr_fetch::VersionSpec;

const TEST_CARGO_BIN_ENV_VAR: &str = "SOLDR_TEST_CARGO_BIN";
const TEST_RUSTC_BIN_ENV_VAR: &str = "SOLDR_TEST_RUSTC_BIN";
const TEST_ZCCACHE_BIN_ENV_VAR: &str = "SOLDR_TEST_ZCCACHE_BIN";

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
    /// Show cache status and tool info
    Status,
    /// Clear caches
    Clean,
    /// Show or set configuration
    Config,
    /// Inspect the compilation cache
    Cache,
    /// Show version
    Version,
    /// Anything else is a tool to fetch and run
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[tokio::main]
async fn main() {
    // RUSTC_WRAPPER mode: cargo passes `soldr /path/to/rustc <args...>`
    // Must be checked before clap parsing.
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.len() > 1 && is_rustc_path(&raw_args[1]) {
        std::process::exit(run_rustc_wrapper(&raw_args).unwrap_or_else(report_and_exit));
    }

    if let Err(e) = run().await {
        eprintln!("soldr: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), SoldrError> {
    let cli = Cli::parse();
    let cache_enabled = !cli.no_cache;

    match cli.command {
        Commands::Cargo { args } => {
            std::process::exit(run_cargo_front_door(&args, cache_enabled).await?);
        }
        Commands::Status => {
            println!("soldr {}", soldr_core::version());
            let target = soldr_core::TargetTriple::detect()?;
            let paths = soldr_core::SoldrPaths::new()?;
            println!("target: {target}");
            println!("root dir: {}", paths.root.display());
            println!("cache dir: {}", paths.cache.display());
            println!("cache default: enabled");
            println!(
                "cache mode: {}",
                if cache_enabled { "enabled" } else { "disabled" }
            );
            println!("zccache version: {}", soldr_fetch::MANAGED_ZCCACHE_VERSION);
            print_zccache_status(&paths)?;
        }
        Commands::Clean => {
            clear_zccache_cache()?;
        }
        Commands::Config => {
            println!("(config not yet implemented)");
        }
        Commands::Cache => {
            print_zccache_status(&SoldrPaths::new()?)?;
        }
        Commands::Version => {
            println!("soldr {}", soldr_core::version());
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

fn is_rustc_path(arg: &str) -> bool {
    if arg == "rustc" {
        return true;
    }

    std::path::Path::new(arg)
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        == Some("rustc")
}

fn run_rustc_wrapper(raw_args: &[String]) -> Result<i32, SoldrError> {
    if soldr_cache::cache_enabled_in_current_process() {
        if let Some(zccache) = zccache_binary_override() {
            return run_wrapper_through_zccache(raw_args, &zccache);
        }
    }

    let rustc = raw_args
        .get(1)
        .ok_or_else(|| SoldrError::Other("missing rustc path in wrapper mode".into()))?;
    let rustc = if rustc == "rustc" {
        resolve_toolchain_binary("rustc")?
    } else {
        rustc.into()
    };

    let status = std::process::Command::new(rustc)
        .args(&raw_args[2..])
        .status()?;

    Ok(status.code().unwrap_or(1))
}

fn run_wrapper_through_zccache(
    raw_args: &[String],
    zccache: &std::path::Path,
) -> Result<i32, SoldrError> {
    let status = std::process::Command::new(zccache)
        .args(&raw_args[1..])
        .status()?;

    Ok(status.code().unwrap_or(1))
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

    let mut command = std::process::Command::new(cargo);
    command.args(args);
    command.env("RUSTC", rustc);
    command.env(
        soldr_cache::CACHE_ENABLED_ENV_VAR,
        soldr_cache::cache_enabled_env_value(cache_enabled),
    );
    command.env(
        "PATH",
        prepend_path(&cargo_bin_dir, existing_path.as_deref())?,
    );
    if let Some(target) = default_cargo_build_target(args)? {
        command.env("CARGO_BUILD_TARGET", target);
    }

    let session = if cache_enabled {
        Some(prepare_zccache_build(&mut command, &paths).await?)
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

fn prepend_path(
    dir: &std::path::Path,
    existing_path: Option<&std::ffi::OsStr>,
) -> Result<std::ffi::OsString, SoldrError> {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing_path) = existing_path {
        paths.extend(std::env::split_paths(existing_path));
    }
    std::env::join_paths(paths).map_err(|e| SoldrError::Other(format!("invalid PATH: {e}")))
}

fn resolve_toolchain_binary(tool: &str) -> Result<std::path::PathBuf, SoldrError> {
    if let Some(path) = toolchain_binary_override(tool) {
        return Ok(path);
    }

    let output = std::process::Command::new("rustup")
        .args(["which", tool])
        .output()?;

    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "failed to resolve {tool} via rustup: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(SoldrError::Other(format!(
            "rustup did not return a path for {tool}"
        )));
    }

    Ok(path.into())
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
    session_id: String,
}

async fn prepare_zccache_build(
    cargo: &mut std::process::Command,
    paths: &SoldrPaths,
) -> Result<ZccacheBuildSession, SoldrError> {
    let fetch = fetch_managed_zccache(paths).await?;
    let zccache_dir = soldr_cache::zccache_dir(paths);
    std::fs::create_dir_all(&zccache_dir)?;
    std::fs::create_dir_all(zccache_dir.join("logs"))?;
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

    run_zccache_command(&fetch.binary_path, &["start"])?;

    let journal_path = soldr_cache::session_journal_path(&zccache_dir);
    let journal_path = journal_path.display().to_string();
    let session_json = run_zccache_command(
        &fetch.binary_path,
        &["session-start", "--stats", "--journal", &journal_path],
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
    cargo.env(soldr_cache::ZCCACHE_SESSION_ID_ENV_VAR, &session_id);

    Ok(ZccacheBuildSession {
        binary_path: fetch.binary_path,
        session_id,
    })
}

fn finish_zccache_build(session: &ZccacheBuildSession) -> Result<(), SoldrError> {
    let output = run_zccache_command(&session.binary_path, &["session-end", &session.session_id])?;
    let stdout = output.stdout.trim();
    if !stdout.is_empty() {
        eprintln!("soldr: zccache session summary");
        eprintln!("{stdout}");
    }
    Ok(())
}

fn clear_zccache_cache() -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let zccache_dir = soldr_cache::zccache_dir(&paths);
    let mut cleared_anything = false;

    if let Some(fetch) = cached_managed_zccache(&paths)? {
        let _ = run_zccache_command(&fetch.binary_path, &["clear"])?;
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

fn print_zccache_status(paths: &SoldrPaths) -> Result<(), SoldrError> {
    let zccache_dir = soldr_cache::zccache_dir(paths);
    let journal_path = soldr_cache::session_journal_path(&zccache_dir);
    println!("soldr zccache state dir: {}", zccache_dir.display());
    println!(
        "last session journal: {} ({})",
        journal_path.display(),
        if journal_path.exists() {
            "present"
        } else {
            "missing"
        }
    );

    match cached_managed_zccache(paths)? {
        Some(fetch) => {
            println!("zccache binary: {}", fetch.binary_path.display());
            let output = run_zccache_command(&fetch.binary_path, &["status"])?;
            let stdout = output.stdout.trim();
            if stdout.is_empty() {
                println!("zccache status: no output");
            } else {
                for line in stdout.lines() {
                    println!("zccache: {line}");
                }
            }
        }
        None => {
            println!(
                "zccache binary: not fetched yet (will fetch managed zccache {} on the first cache-enabled build)",
                soldr_fetch::MANAGED_ZCCACHE_VERSION
            );
        }
    }
    Ok(())
}

struct CommandOutput {
    stdout: String,
}

fn run_zccache_command(
    binary: &std::path::Path,
    args: &[&str],
) -> Result<CommandOutput, SoldrError> {
    let output = std::process::Command::new(binary).args(args).output()?;
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
    use super::{cargo_args_specify_target, cargo_args_use_reserved_no_cache, parse_tool_spec};
    use soldr_fetch::VersionSpec;

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
    fn parse_tool_spec_defaults_to_latest_version() {
        let (tool, version) = parse_tool_spec("maturin");
        assert_eq!(tool, "maturin");
        assert!(matches!(version, VersionSpec::Latest));
    }
}
