//! Which directory Cargo builds into (soldr#3203).
//!
//! soldr answered this in at least seven places, and they disagreed: one took
//! the nearest `Cargo.toml` rather than the workspace root, several ignored
//! `.cargo/config.toml`, one read nothing but `CARGO_TARGET_DIR`. A wrong answer
//! is silent -- hooks, cleanup and the per-unit cache lines just look at a
//! `target/` Cargo never writes.
//!
//! This is the one implementation. Its rules were measured against the
//! `target_directory` that `cargo metadata` reports rather than recalled, and
//! `crates/soldr-cli/tests/toolchain_env/cargo_target_dir_parity.rs` keeps them
//! measured:
//!
//! 1. `--target-dir DIR`, relative to the working directory.
//! 2. `CARGO_TARGET_DIR`, relative to the working directory.
//! 3. `--config build.target-dir=VALUE`, relative to the working directory.
//! 4. `CARGO_BUILD_TARGET_DIR`, relative to the working directory.
//! 5. `build.target-dir` from the nearest `.cargo/config[.toml]` found walking
//!    up from the **working directory** (not the manifest), then
//!    `$CARGO_HOME/config[.toml]`. A relative value is relative to the
//!    directory holding `.cargo` -- for `$CARGO_HOME`, its parent -- and is not
//!    normalized, exactly as Cargo reports it.
//! 6. `<workspace root>/target`: the manifest's own `[workspace]`, else its
//!    `package.workspace` path, else the nearest ancestor `Cargo.toml` declaring
//!    a `[workspace]` that does not exclude it, else the manifest's directory.
//!
//! Not modelled: `--config <file>` (a config file on the command line) and
//! workspace membership globs, which only matter for manifests Cargo itself
//! rejects. A caller that needs Cargo's exact answer can still ask
//! `cargo metadata`, as the no-cache detach does.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

/// Test-only tripwire (soldr#3203): the absolute path of the running test
/// binary, set by the nextest wrapper for every test process.
///
/// Fixtures that run soldr from the crate directory used to resolve a
/// nonexistent `crates/soldr-cli/target`, so their target hooks did nothing.
/// Correct resolution points them at the repository's own `target/` -- the tree
/// the suite runs out of -- where the no-cache preflight and cleanup hooks
/// modified live test binaries mid-run. A target directory that contains the
/// running test binary is therefore refused, naming the fixture that needs its
/// own `CARGO_TARGET_DIR`. Never set outside tests.
pub const FORBID_TARGET_CONTAINING_ENV_VAR: &str = "SOLDR_TEST_FORBID_TARGET_CONTAINING";

/// Whether `dir` holds the running test binary named by `binary`. Pure, so it
/// is testable without the process environment.
pub fn target_dir_holds_test_binary(dir: &Path, binary: Option<&OsStr>) -> bool {
    binary
        .filter(|binary| !binary.is_empty())
        .is_some_and(|binary| Path::new(binary).starts_with(dir))
}

/// Refuse a build whose target directory holds the running test suite.
pub fn forbid_test_suite_target_tripwire(dir: &Path) -> Result<(), super::SoldrError> {
    let binary = std::env::var_os(FORBID_TARGET_CONTAINING_ENV_VAR);
    if target_dir_holds_test_binary(dir, binary.as_deref()) {
        return Err(super::SoldrError::Other(format!(
            "test tripwire: this build resolved Cargo target directory {}, which holds \
             the running test binary {}; its hooks would modify the suite's own \
             target tree. Give the fixture its own CARGO_TARGET_DIR or run it from a \
             temporary workspace ({FORBID_TARGET_CONTAINING_ENV_VAR}, soldr#3203)",
            dir.display(),
            Path::new(binary.as_deref().unwrap_or_default()).display()
        )));
    }
    Ok(())
}

/// Everything the resolution reads, gathered up front so the rules are testable
/// without touching this process's environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CargoTargetDirInputs {
    /// Cargo's working directory: config discovery and every relative CLI or
    /// environment path start here.
    pub cwd: PathBuf,
    /// Cargo's argv after the binary. Only `--target-dir`, `--config` and
    /// `--manifest-path` are read, and nothing after `--`.
    pub args: Vec<String>,
    pub cargo_target_dir: Option<OsString>,
    pub cargo_build_target_dir: Option<OsString>,
    pub cargo_home: Option<PathBuf>,
}

impl CargoTargetDirInputs {
    /// The inputs Cargo would see if launched from `cwd` with `args` under this
    /// process's environment.
    pub fn from_process(cwd: &Path, args: &[String]) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            args: args.to_vec(),
            cargo_target_dir: std::env::var_os("CARGO_TARGET_DIR"),
            cargo_build_target_dir: std::env::var_os("CARGO_BUILD_TARGET_DIR"),
            cargo_home: super::resolve_cargo_home(),
        }
    }
}

/// The directory Cargo will build into, or `None` when nothing sets it
/// explicitly and no `Cargo.toml` anchors the default.
pub fn resolve_cargo_target_dir(inputs: &CargoTargetDirInputs) -> Option<PathBuf> {
    let from_cwd = |value: &OsStr| relative_to(&inputs.cwd, value);
    if let Some(dir) = arg_value(&inputs.args, "--target-dir") {
        return Some(from_cwd(OsStr::new(&dir)));
    }
    if let Some(dir) = non_empty(&inputs.cargo_target_dir) {
        return Some(from_cwd(dir));
    }
    if let Some(dir) = cli_config_target_dir(&inputs.args) {
        return Some(from_cwd(OsStr::new(&dir)));
    }
    if let Some(dir) = non_empty(&inputs.cargo_build_target_dir) {
        return Some(from_cwd(dir));
    }
    if let Some(dir) = config_file_target_dir(&inputs.cwd, inputs.cargo_home.as_deref()) {
        return Some(dir);
    }
    workspace_root(inputs).map(|root| root.join("target"))
}

fn relative_to(base: &Path, value: &OsStr) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn non_empty(value: &Option<OsString>) -> Option<&OsStr> {
    value.as_deref().filter(|value| !value.is_empty())
}

/// Every `FLAG VALUE` / `FLAG=VALUE` before `--`, in order.
fn arg_values<'a>(args: &'a [String], flag: &'a str) -> impl Iterator<Item = String> + 'a {
    let mut iter = args.iter().take_while(|arg| arg.as_str() != "--");
    std::iter::from_fn(move || loop {
        let arg = iter.next()?;
        if arg == flag {
            return iter.next().cloned();
        }
        if let Some(value) = arg
            .strip_prefix(flag)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return Some(value.to_string());
        }
    })
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    arg_values(args, flag).last()
}

/// `build.target-dir` from `--config build.target-dir=VALUE`, the last one
/// winning. A `--config <file>` has no `=` and is not modelled.
fn cli_config_target_dir(args: &[String]) -> Option<String> {
    arg_values(args, "--config")
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            if key.trim() != "build.target-dir" {
                return None;
            }
            let parsed: toml::Value = toml::from_str(&format!("value = {}", value.trim())).ok()?;
            parsed.get("value")?.as_str().map(str::to_string)
        })
        .last()
}

fn config_file_target_dir(cwd: &Path, cargo_home: Option<&Path>) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        if let Some(value) = dot_cargo_target_dir(&dir.join(".cargo")) {
            return Some(relative_to(dir, OsStr::new(&value)));
        }
    }
    let home = cargo_home?;
    let value = dot_cargo_target_dir(home)?;
    Some(relative_to(
        home.parent().unwrap_or(home),
        OsStr::new(&value),
    ))
}

/// `build.target-dir` from the config file in a `.cargo`-style directory. Cargo
/// reads the legacy extension-less `config` when both files exist.
fn dot_cargo_target_dir(dir: &Path) -> Option<String> {
    let file = ["config", "config.toml"]
        .into_iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())?;
    let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    parsed
        .get("build")?
        .get("target-dir")?
        .as_str()
        .map(str::to_string)
}

fn workspace_root(inputs: &CargoTargetDirInputs) -> Option<PathBuf> {
    let manifest = match arg_value(&inputs.args, "--manifest-path") {
        Some(path) => relative_to(&inputs.cwd, OsStr::new(&path)),
        None => inputs
            .cwd
            .ancestors()
            .map(|dir| dir.join("Cargo.toml"))
            .find(|path| path.is_file())?,
    };
    let manifest_dir = manifest.parent()?.to_path_buf();
    let own = read_toml(&manifest);
    if own
        .as_ref()
        .is_some_and(|manifest| manifest.get("workspace").is_some())
    {
        return Some(manifest_dir);
    }
    if let Some(pointer) = own
        .as_ref()
        .and_then(|manifest| manifest.get("package")?.get("workspace")?.as_str())
    {
        return Some(lexically_normal(&manifest_dir.join(pointer)));
    }
    for dir in manifest_dir.ancestors().skip(1) {
        let Some(candidate) = read_toml(&dir.join("Cargo.toml")) else {
            continue;
        };
        let Some(workspace) = candidate.get("workspace") else {
            continue;
        };
        if !excludes(workspace, dir, &manifest_dir) {
            return Some(dir.to_path_buf());
        }
    }
    Some(manifest_dir)
}

fn read_toml(path: &Path) -> Option<toml::Value> {
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn excludes(workspace: &toml::Value, root: &Path, member: &Path) -> bool {
    let Ok(relative) = member.strip_prefix(root) else {
        return false;
    };
    workspace
        .get("exclude")
        .and_then(toml::Value::as_array)
        .is_some_and(|entries| {
            entries
                .iter()
                .filter_map(toml::Value::as_str)
                .any(|entry| relative.starts_with(entry))
        })
}

/// Cargo reports a `package.workspace` root with `..` resolved, so this path
/// is normalized; config-relative target dirs are not.
fn lexically_normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(args: &[&str]) -> CargoTargetDirInputs {
        CargoTargetDirInputs {
            cwd: PathBuf::from("/work/member"),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            ..CargoTargetDirInputs::default()
        }
    }

    #[test]
    fn explicit_sources_win_in_cargos_order() {
        let mut all = inputs(&[
            "build",
            "--target-dir",
            "flag",
            "--config",
            "build.target-dir=\"cli\"",
        ]);
        all.cargo_target_dir = Some("env".into());
        all.cargo_build_target_dir = Some("build-env".into());
        assert_eq!(
            resolve_cargo_target_dir(&all),
            Some(PathBuf::from("/work/member/flag"))
        );
        all.args = vec![
            "build".into(),
            "--config".into(),
            "build.target-dir=\"cli\"".into(),
        ];
        assert_eq!(
            resolve_cargo_target_dir(&all),
            Some(PathBuf::from("/work/member/env"))
        );
        all.cargo_target_dir = None;
        assert_eq!(
            resolve_cargo_target_dir(&all),
            Some(PathBuf::from("/work/member/cli"))
        );
        all.args = vec!["build".into()];
        assert_eq!(
            resolve_cargo_target_dir(&all),
            Some(PathBuf::from("/work/member/build-env"))
        );
    }

    #[test]
    fn empty_environment_values_are_ignored() {
        let mut empty = inputs(&["build", "--target-dir=flag"]);
        empty.cargo_target_dir = Some(OsString::new());
        assert_eq!(
            resolve_cargo_target_dir(&empty),
            Some(PathBuf::from("/work/member/flag"))
        );
    }

    #[test]
    fn flags_after_the_double_dash_belong_to_the_program() {
        assert_eq!(
            arg_value(
                &inputs(&["run", "--", "--target-dir", "x"]).args,
                "--target-dir"
            ),
            None
        );
    }

    #[test]
    fn only_a_build_target_dir_config_entry_counts() {
        let args: Vec<String> = [
            "--config",
            "build.jobs=4",
            "--config=build.target-dir = 'a'",
            "--config",
            "path/to/config.toml",
        ]
        .iter()
        .map(|arg| (*arg).to_string())
        .collect();
        assert_eq!(cli_config_target_dir(&args), Some("a".to_string()));
    }

    #[test]
    fn only_a_target_dir_holding_the_test_binary_trips() {
        let binary = OsStr::new("/repo/target/x86_64-unknown-linux-gnu/debug/deps/suite-1");
        assert!(target_dir_holds_test_binary(
            Path::new("/repo/target"),
            Some(binary)
        ));
        assert!(!target_dir_holds_test_binary(
            Path::new("/tmp/fixture/target"),
            Some(binary)
        ));
        assert!(!target_dir_holds_test_binary(
            Path::new("/repo/target-other"),
            Some(binary)
        ));
        assert!(!target_dir_holds_test_binary(
            Path::new("/repo/target"),
            None
        ));
        assert!(!target_dir_holds_test_binary(
            Path::new("/repo/target"),
            Some(OsStr::new(""))
        ));
    }

    #[test]
    fn a_workspace_pointer_is_normalized() {
        assert_eq!(
            lexically_normal(Path::new("/ws/m/inner/../..")),
            PathBuf::from("/ws")
        );
    }
}
