//! Toolchain / rustup / zccache binary resolution helpers. Extracted from
//! `main.rs` as part of issue #339.

use crate::{
    REAL_TOOLCHAIN_BINARY_ENV_PREFIX, TEST_CARGO_BIN_ENV_VAR, TEST_RUSTC_BIN_ENV_VAR,
    TEST_RUSTUP_BIN_ENV_VAR, TEST_ZCCACHE_BIN_ENV_VAR,
};
use soldr_core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use soldr_fetch::VersionSpec;

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

pub(crate) fn rustup_resolution_failure(tool: &str, stderr: &[u8]) -> SoldrError {
    let raw_failure = String::from_utf8_lossy(stderr).trim().to_string();
    SoldrError::Other(format!(
        "failed to resolve {tool} via rustup: {raw_failure}\n\
CI hint: if this repository pins Rust in rust-toolchain.toml, preinstall that exact channel instead of a generic stable toolchain.\n\
CI hint: export RUSTUP_TOOLCHAIN to that exact channel for later cargo, rustc, and soldr cargo steps, or use the documented setup-soldr action path (uses: zackees/soldr@<ref> or uses: ./).\n\
Bootstrap hint: if rustup itself is missing on this host, run `soldr bootstrap` to install it into soldr's managed bin dir (or unset SOLDR_NO_BOOTSTRAP and re-run if you previously opted out)."
    ))
}

pub(crate) fn parse_tool_spec(spec: &str) -> (String, VersionSpec) {
    if let Some((name, version)) = spec.split_once('@') {
        (name.to_string(), VersionSpec::parse(version))
    } else {
        (spec.to_string(), VersionSpec::Latest)
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

/// Resolve the rustup binary to spawn for any subprocess call. Always
/// honours `SOLDR_TEST_RUSTUP_BIN` for tests. Then prefers `rustup` already on
/// `PATH` or the soldr-managed copy under `<SoldrPaths::bin>/rustup`. If
/// neither exists and the user has not opted out via `SOLDR_NO_BOOTSTRAP=1`,
/// triggers a one-shot bootstrap (`rustup-init`) that installs rustup into
/// the soldr-managed bin dir before returning that path.
pub(crate) fn rustup_binary() -> std::path::PathBuf {
    if let Some(path) = non_empty_env_path(TEST_RUSTUP_BIN_ENV_VAR) {
        return path;
    }
    if let Ok(paths) = SoldrPaths::new() {
        if let Some(existing) = soldr_fetch::discover_rustup(&paths) {
            return existing;
        }
        match soldr_fetch::auto_bootstrap_if_missing_blocking(&paths) {
            Ok(soldr_fetch::AutoBootstrapOutcome::AlreadyInstalled(p)) => return p,
            Ok(soldr_fetch::AutoBootstrapOutcome::Installed(report)) => return report.rustup_path,
            Ok(soldr_fetch::AutoBootstrapOutcome::OptedOut) => {
                // Fall through to the bare "rustup" path; the subsequent
                // resolution failure surfaces the standard diagnostic plus
                // the `SOLDR_NO_BOOTSTRAP` hint.
            }
            Err(err) => {
                eprintln!(
                    "soldr: auto-bootstrap failed ({err}). Falling back to `rustup` on PATH. \
                     Run `soldr bootstrap` manually or unset {} to retry.",
                    soldr_fetch::NO_BOOTSTRAP_ENV_VAR
                );
            }
        }
    }
    "rustup".into()
}

pub(crate) fn zccache_binary_override() -> Option<std::path::PathBuf> {
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

pub(crate) fn current_soldr_binary() -> Result<std::path::PathBuf, SoldrError> {
    std::env::current_exe().map_err(SoldrError::from)
}

pub(crate) async fn fetch_managed_zccache(
    paths: &SoldrPaths,
) -> Result<soldr_fetch::FetchResult, SoldrError> {
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
mod tests {
    use super::*;

    #[test]
    fn parse_tool_spec_defaults_to_latest_version() {
        let (tool, version) = parse_tool_spec("maturin");
        assert_eq!(tool, "maturin");
        assert!(matches!(version, VersionSpec::Latest));
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
        assert!(rendered.contains("soldr bootstrap"));
        assert!(rendered.contains("SOLDR_NO_BOOTSTRAP"));
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
}
