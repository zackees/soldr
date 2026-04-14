use clap::{Parser, Subcommand};
use soldr_core::SoldrError;
use soldr_fetch::VersionSpec;

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
            std::process::exit(run_cargo_front_door(&args, cache_enabled)?);
        }
        Commands::Status => {
            println!("soldr {}", soldr_core::version());
            let target = soldr_core::TargetTriple::detect()?;
            let paths = soldr_core::SoldrPaths::new()?;
            println!("target: {target}");
            println!("cache dir: {}", paths.cache.display());
            println!("cache default: enabled");
            println!(
                "cache mode: {}",
                if cache_enabled { "enabled" } else { "disabled" }
            );
            println!("build cache: control plane wired; artifact cache not yet implemented");
        }
        Commands::Clean => {
            println!("(clean not yet implemented)");
        }
        Commands::Config => {
            println!("(config not yet implemented)");
        }
        Commands::Cache => {
            println!("(cache not yet implemented)");
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
    arg == "rustc"
        || arg.ends_with("/rustc")
        || arg.ends_with("\\rustc")
        || arg.ends_with("rustc.exe")
}

fn run_rustc_wrapper(raw_args: &[String]) -> Result<i32, SoldrError> {
    if soldr_cache::cache_enabled_in_current_process() {
        // The actual artifact cache will slot in here in a follow-up slice.
        // For now, cache-enabled and cache-disabled wrapper mode both
        // delegate to the real rustc without modifying outputs.
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

fn run_cargo_front_door(args: &[String], cache_enabled: bool) -> Result<i32, SoldrError> {
    let cargo = resolve_toolchain_binary("cargo")?;
    let rustc = resolve_toolchain_binary("rustc")?;
    let current_exe = std::env::current_exe()?;
    let cargo_bin_dir = cargo
        .parent()
        .ok_or_else(|| SoldrError::Other("failed to resolve cargo bin directory".into()))?
        .to_path_buf();
    let existing_path = std::env::var_os("PATH");

    let mut command = std::process::Command::new(cargo);
    command.args(args);
    command.env("RUSTC_WRAPPER", current_exe);
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

    let status = command.status()?;
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

#[cfg(test)]
mod tests {
    use super::{cargo_args_specify_target, parse_tool_spec};
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
    fn parse_tool_spec_defaults_to_latest_version() {
        let (tool, version) = parse_tool_spec("maturin");
        assert_eq!(tool, "maturin");
        assert!(matches!(version, VersionSpec::Latest));
    }
}
