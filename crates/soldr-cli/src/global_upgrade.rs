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

/// Set by soldr on the probe/delegate child so a delegated invocation does
/// not delegate again.
const GLOBAL_DELEGATION_ENV_VAR: &str = "SOLDR_GLOBAL_DELEGATING";

/// Opt out of the delegation probe without claiming to be a delegated child.
///
/// soldr#2785: the integration harness needs the probe off — it costs a
/// process spawn per invocation and stages a broker into the fixture's home.
/// Setting [`GLOBAL_DELEGATION_ENV_VAR`] would do that, but it means a second
/// thing: `reentrancy_guard` lists it in `SANCTIONED_EDGE_ENV_VARS`, so every
/// fixture would also be exempted from re-entrancy enforcement. That guard is
/// the whole point of soldr#2566, and blanket-exempting the suite from it to
/// save a process spawn is not a trade worth making.
///
/// This carries the first meaning only.
pub const GLOBAL_DELEGATION_DISABLE_ENV_VAR: &str = "SOLDR_NO_GLOBAL_DELEGATION";

/// Hand this invocation to a newer globally-installed soldr when the current
/// project opted in. Returns `Some(exit_code)` only when delegation occurred
/// (or the exec failed); callers should continue their normal dispatch on
/// `None`.
pub fn maybe_delegate(raw_args: &[String]) -> Option<i32> {
    if is_delegation_exempt(raw_args)
        || std::env::var_os(GLOBAL_DELEGATION_ENV_VAR).is_some()
        || std::env::var_os(GLOBAL_DELEGATION_DISABLE_ENV_VAR).is_some()
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

/// Invocation shapes the delegation policy must leave alone. The probe is not
/// free of side effects: `probe_version` runs `<global soldr> --version` with
/// this process's environment, and a released soldr's front door stages a
/// broker image under the inherited HOME and spawns `broker serve` before it
/// prints its version. That side effect is what made the target-run
/// broker-absent tests find "a broker" running in their isolated homes
/// (#2521 D) — the probe child had just created it.
///
/// * `broker` family — lifecycle commands operate on the invoked image's own
///   endpoint identity; delegating (or even probing) here either swaps which
///   identity is inspected/retired or manufactures the very broker a status
///   probe is asking about. `broker serve` additionally must never re-enter
///   soldr before the bind (the front door already selected this exact image).
/// * flag-shaped first argument (`--version`, `--help`, `-V`) — prints and
///   exits; there is no build for a newer global soldr to own.
fn is_delegation_exempt(raw_args: &[String]) -> bool {
    match raw_args.get(1).map(String::as_str) {
        None => true,
        Some("broker") => true,
        Some(first) => first.starts_with('-'),
    }
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

    // Windows: also try the PATHEXT suffixes (no-op list on other hosts).
    for extension in crate::platform::executable::search::candidate_extensions() {
        let candidate = directory.join(format!("{name}{extension}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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

    // Unix execs (replacing this image); Windows spawns and waits. Only
    // the failure path returns here on Unix.
    match crate::platform::process::spawn::exec_or_status(&mut command) {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!(
                "soldr: failed to exec newer global soldr at {}: {error}",
                binary.display()
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_v_prefixed_versions() {
        assert_eq!(
            parse_soldr_version("soldr 0.8.19\n"),
            Some(Version::parse("0.8.19").expect("valid semver"))
        );
        assert_eq!(
            parse_soldr_version("soldr v1.0.0-beta.1\n"),
            Some(Version::parse("1.0.0-beta.1").expect("valid semver"))
        );
    }

    #[test]
    fn rejects_non_soldr_or_invalid_output() {
        assert_eq!(parse_soldr_version("cargo 1.2.3\n"), None);
        assert_eq!(parse_soldr_version("soldr latest\n"), None);
    }

    #[test]
    fn broker_family_and_flag_invocations_are_delegation_exempt() {
        for verb in ["serve", "status", "stop", "routes", "remove"] {
            assert!(
                is_delegation_exempt(&["soldr".into(), "broker".into(), verb.into()]),
                "broker {verb} must not probe or delegate"
            );
        }
        for flag in ["--version", "-V", "--help"] {
            assert!(
                is_delegation_exempt(&["soldr".into(), flag.into()]),
                "{flag} must not probe or delegate"
            );
        }
        assert!(is_delegation_exempt(&["soldr".into()]));
        // Ordinary commands still delegate in an opted-in checkout.
        for verb in ["version", "status", "cargo"] {
            assert!(
                !is_delegation_exempt(&["soldr".into(), verb.into()]),
                "{verb} must remain delegable"
            );
        }
    }
}
