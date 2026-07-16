//! Optional project policy for preferring a newer globally-installed soldr.
//!
//! A checkout can opt in with `prefer_newer_global = true` under
//! `[workspace.metadata.soldr]` (or `[package.metadata.soldr]`). When a
//! locally-built soldr starts in that project, this module finds the first
//! different `soldr` executable on PATH and delegates only if its SemVer is
//! strictly newer. The guard environment variable applies to both the probe
//! and hand-off so PATH arrangements can never recurse indefinitely.

use std::path::{Path, PathBuf};
use std::process::Command;

use semver::Version;

const GLOBAL_DELEGATION_ENV_VAR: &str = "SOLDR_GLOBAL_DELEGATING";

/// Hand this invocation to a newer globally-installed soldr when the current
/// project opted in. Returns `Some(exit_code)` only when delegation occurred
/// (or the exec failed); callers should continue their normal dispatch on
/// `None`.
pub fn maybe_delegate(raw_args: &[String]) -> Option<i32> {
    if std::env::var_os(GLOBAL_DELEGATION_ENV_VAR).is_some()
        || !crate::cargo_metadata_soldr::prefer_newer_global_from_cwd()
    {
        return None;
    }

    let current = std::env::current_exe().ok()?;
    let global = find_global_soldr(&current)?;
    let global_version = probe_version(&global)?;
    let current_version = Version::parse(crate::core::version()).ok()?;

    if global_version <= current_version {
        return None;
    }

    eprintln!(
        "soldr: delegating to newer global soldr v{global_version} at {} (current v{current_version})",
        global.display()
    );
    Some(delegate(&global, &raw_args[1..]))
}

fn find_global_soldr(current: &Path) -> Option<PathBuf> {
    let current = std::fs::canonicalize(current).ok()?;
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let Some(candidate) = executable_in_dir(&directory, "soldr") else {
            continue;
        };
        if std::fs::canonicalize(&candidate).ok().as_ref() != Some(&current) {
            return Some(candidate);
        }
    }
    None
}

fn executable_in_dir(directory: &Path, name: &str) -> Option<PathBuf> {
    let candidate = directory.join(name);
    if candidate.is_file() {
        return Some(candidate);
    }

    #[cfg(windows)]
    {
        for extension in windows_path_extensions() {
            let candidate = directory.join(format!("{name}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn windows_path_extensions() -> Vec<String> {
    std::env::var_os("PATHEXT")
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(str::trim)
        .filter(|extension| !extension.is_empty())
        .map(|extension| extension.to_ascii_lowercase())
        .collect()
}

fn probe_version(binary: &Path) -> Option<Version> {
    let mut command = Command::new(binary);
    command.arg("--version").env(GLOBAL_DELEGATION_ENV_VAR, "1");
    crate::core::suppress_windows_console_window(&mut command);
    let output =
        crate::core::command_output_with_timeout(&mut command, "global soldr --version").ok()?;
    output.status.success().then_some(())?;
    parse_soldr_version(&String::from_utf8_lossy(&output.stdout))
}

fn parse_soldr_version(output: &str) -> Option<Version> {
    output.lines().find_map(|line| {
        let version = line.trim().strip_prefix("soldr ")?.trim_start_matches('v');
        Version::parse(version).ok()
    })
}

fn delegate(binary: &Path, args: &[String]) -> i32 {
    let mut command = Command::new(binary);
    command.args(args).env(GLOBAL_DELEGATION_ENV_VAR, "1");
    crate::core::suppress_windows_console_window(&mut command);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let error = command.exec();
        eprintln!(
            "soldr: failed to exec newer global soldr at {}: {error}",
            binary.display()
        );
        1
    }

    #[cfg(not(unix))]
    {
        match command.status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(error) => {
                eprintln!(
                    "soldr: failed to run newer global soldr at {}: {error}",
                    binary.display()
                );
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(parses_plain_and_v_prefixed_versions, {
        assert_eq!(
            parse_soldr_version("soldr 0.8.19\n"),
            Some(Version::parse("0.8.19").expect("valid semver"))
        );
        assert_eq!(
            parse_soldr_version("soldr v1.0.0-beta.1\n"),
            Some(Version::parse("1.0.0-beta.1").expect("valid semver"))
        );
    });

    crate::timed_test!(rejects_non_soldr_or_invalid_output, {
        assert_eq!(parse_soldr_version("cargo 1.2.3\n"), None);
        assert_eq!(parse_soldr_version("soldr latest\n"), None);
    });
}
