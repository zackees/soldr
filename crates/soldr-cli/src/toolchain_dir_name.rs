//! Installed-directory spelling of `RUSTUP_TOOLCHAIN` (soldr#3394).
//!
//! rustup's own proxies expand a short channel such as `nightly-2026-05-28`
//! to the installed `nightly-2026-05-28-x86_64-unknown-linux-gnu`, but tools
//! that read the variable literally (the Dylint driver builds its sysroot as
//! `$RUSTUP_HOME/toolchains/$RUSTUP_TOOLCHAIN`) do not. Whenever a child
//! command carries an explicit `RUSTUP_HOME`, its `RUSTUP_TOOLCHAIN` must name
//! a directory that exists under `<home>/toolchains/`.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

/// The directory name under `<rustup_home>/toolchains/` that `toolchain`
/// denotes: itself when that directory exists, otherwise the single installed
/// directory that extends it with a `-<host-triple>` suffix. `None` when the
/// name is already exact, is a path, or is missing or ambiguous.
pub(crate) fn installed_toolchain_dir_name(rustup_home: &Path, toolchain: &str) -> Option<String> {
    if toolchain.is_empty() || toolchain.contains(['/', '\\']) {
        return None;
    }
    let toolchains = rustup_home.join("toolchains");
    if toolchains.join(toolchain).is_dir() {
        return None;
    }
    let prefix = format!("{toolchain}-");
    let mut matches = std::fs::read_dir(&toolchains)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(&prefix));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// Rewrite the command's effective `RUSTUP_TOOLCHAIN` to the installed
/// directory name when the command's effective `RUSTUP_HOME` only has the
/// suffixed spelling. Explicit values on the command win over the process
/// environment; with no `RUSTUP_HOME` (rustup's default home) nothing changes.
pub(crate) fn apply_installed_toolchain_dir_name(command: &mut Command) {
    let (Some(home), Some(toolchain)) = (
        effective_env(command, "RUSTUP_HOME"),
        effective_env(command, "RUSTUP_TOOLCHAIN"),
    ) else {
        return;
    };
    let Some(toolchain) = toolchain.to_str() else {
        return;
    };
    if let Some(installed) = installed_toolchain_dir_name(Path::new(&home), toolchain) {
        command.env("RUSTUP_TOOLCHAIN", installed);
    }
}

fn effective_env(command: &Command, key: &str) -> Option<OsString> {
    let explicit = command
        .get_envs()
        .find(|(name, _)| *name == OsStr::new(key))
        .map(|(_, value)| value.map(OsStr::to_os_string));
    let value = match explicit {
        Some(value) => value,
        None => std::env::var_os(key),
    };
    value.filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(dirs: &[&str]) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        for dir in dirs {
            std::fs::create_dir_all(home.path().join("toolchains").join(dir)).unwrap();
        }
        home
    }

    #[test]
    fn short_channel_expands_to_the_single_installed_directory() {
        let home = home_with(&["nightly-2026-05-28-x86_64-unknown-linux-gnu"]);
        assert_eq!(
            installed_toolchain_dir_name(home.path(), "nightly-2026-05-28").as_deref(),
            Some("nightly-2026-05-28-x86_64-unknown-linux-gnu")
        );
    }

    #[test]
    fn exact_ambiguous_missing_and_path_names_are_left_alone() {
        let home = home_with(&[
            "1.98.1-x86_64-unknown-linux-gnu",
            "1.98.1-aarch64-apple-darwin",
        ]);
        let exact = home_with(&["stable", "stable-x86_64-unknown-linux-gnu"]);
        assert_eq!(installed_toolchain_dir_name(exact.path(), "stable"), None);
        assert_eq!(installed_toolchain_dir_name(home.path(), "1.98.1"), None);
        assert_eq!(installed_toolchain_dir_name(home.path(), "nightly"), None);
        assert_eq!(installed_toolchain_dir_name(home.path(), "/opt/tc"), None);
    }

    #[test]
    fn command_toolchain_is_rewritten_against_the_commands_own_home() {
        let home = home_with(&["nightly-2026-05-28-x86_64-unknown-linux-gnu"]);
        let mut command = Command::new("cargo");
        command.env("RUSTUP_HOME", home.path());
        command.env("RUSTUP_TOOLCHAIN", "nightly-2026-05-28");
        apply_installed_toolchain_dir_name(&mut command);
        let value = command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("RUSTUP_TOOLCHAIN"))
            .and_then(|(_, value)| value)
            .unwrap();
        assert_eq!(value, "nightly-2026-05-28-x86_64-unknown-linux-gnu");
    }
}
