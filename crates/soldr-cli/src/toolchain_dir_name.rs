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

/// Export the `rust-toolchain.toml` channel of the current directory as the
/// command's `RUSTUP_TOOLCHAIN` (soldr#836). A missing or channel-less manifest
/// exports nothing.
pub(crate) fn export_manifest_channel(command: &mut Command) {
    let manifest_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let Ok(manifest) = crate::core::read_rust_toolchain_manifest_from_ancestors(&manifest_dir)
    else {
        return;
    };
    let channel = manifest.channel.as_deref().map(str::trim).unwrap_or("");
    if !channel.is_empty() {
        command.env("RUSTUP_TOOLCHAIN", channel);
    }
}

/// Carry the toolchain into a metadata probe (`cargo metadata` and friends)
/// the way the eventual cargo child gets it: the caller's `RUSTUP_TOOLCHAIN`
/// when set and non-empty, otherwise the channel of the nearest
/// `rust-toolchain.toml` at or above `workspace_root`. Without it a probe that
/// reaches rustc through a rustup proxy reports that no default toolchain is
/// configured when the pinned toolchain lives in a private rustup home. The
/// installed directory name is used under an explicit `RUSTUP_HOME`
/// (soldr#3394). One implementation for every probe: the PyO3 plan and the
/// `links` provider each carried their own copy that read the current
/// directory only.
pub(crate) fn carry_toolchain_into_probe(command: &mut Command, workspace_root: &Path) {
    carry_toolchain_into_probe_with(
        command,
        workspace_root,
        std::env::var_os("RUSTUP_TOOLCHAIN"),
    );
}

/// [`carry_toolchain_into_probe`] with the caller's `RUSTUP_TOOLCHAIN` passed
/// in, so tests do not depend on the process environment.
fn carry_toolchain_into_probe_with(
    command: &mut Command,
    workspace_root: &Path,
    caller_toolchain: Option<OsString>,
) {
    match caller_toolchain.filter(|value| !value.is_empty()) {
        Some(toolchain) => {
            command.env("RUSTUP_TOOLCHAIN", toolchain);
        }
        None => {
            if let Ok(manifest) =
                crate::core::read_rust_toolchain_manifest_from_ancestors(workspace_root)
            {
                let channel = manifest.channel.as_deref().map(str::trim).unwrap_or("");
                if !channel.is_empty() {
                    command.env("RUSTUP_TOOLCHAIN", channel);
                }
            }
        }
    }
    apply_installed_toolchain_dir_name(command);
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

    #[test]
    fn a_probe_gets_the_ancestor_pin_and_the_installed_directory_name() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"nightly-2026-05-28\"\n",
        )
        .unwrap();
        let nested = root.path().join("crates").join("member");
        std::fs::create_dir_all(&nested).unwrap();
        let home = home_with(&["nightly-2026-05-28-x86_64-unknown-linux-gnu"]);

        let mut command = Command::new("cargo");
        command.env("RUSTUP_HOME", home.path());
        carry_toolchain_into_probe_with(&mut command, &nested, None);
        let value = command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("RUSTUP_TOOLCHAIN"))
            .and_then(|(_, value)| value)
            .unwrap();
        assert_eq!(value, "nightly-2026-05-28-x86_64-unknown-linux-gnu");
    }
}
