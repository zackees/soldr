//! soldr#3277: the cargo front door must not override a linker the project
//! declared for itself in `[target.<triple>]` of its own `.cargo/config.toml`.
//!
//! A new file rather than more of the sibling `tests.rs` for the same reason
//! `backtrace_policy_tests.rs` and `scrub_pool_tests.rs` exist: that file is
//! already past the 1,500-line ceiling, so the ratchet refuses to let it grow.

use super::target;
use crate::core::SoldrPaths;
use crate::EnvVarGuard;
use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;
use std::ffi::{OsStr, OsString};

/// The cross target every case below uses. Deliberate: `resolve_for_target`
/// returns a non-empty injection for every Linux target regardless of the
/// reld-on-PATH probe (`clang` either way), so "nothing was injected" can only
/// mean the soldr#3277 guard fired and never an unlucky probe result.
const TARGET: &str = "aarch64-unknown-linux-gnu";
const LINKER_KEY: &str = "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER";
const RUSTFLAGS_KEY: &str = "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS";

fn command_env_override(
    command: &std::process::Command,
    key: &'static str,
) -> Option<Option<OsString>> {
    command
        .get_envs()
        .find(|(candidate, _)| *candidate == OsStr::new(key))
        .map(|(_, value)| value.map(OsString::from))
}

fn argvec(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

fn assert_generated_linux_linker(command: &std::process::Command, context: &str) {
    let linker = command_env_override(command, LINKER_KEY)
        .and_then(|value| value)
        .expect("Linux linker injection");
    assert!(
        linker.to_string_lossy().contains("linker-shims"),
        "{context}: expected generated linker shim, got {}",
        linker.to_string_lossy(),
    );
    assert_eq!(
        command_env_override(command, RUSTFLAGS_KEY),
        None,
        "{context}: linker selection must preserve project rustflags",
    );
}

/// Build a project root whose `.cargo/config.toml` declares `[target.TARGET]`
/// with `body`, then run `apply_linker_override` from inside it.
///
/// The root is a fresh tempdir with no `Cargo.toml` anywhere above it, so
/// `linker::project_root`'s ancestor walk falls back to the fixture directory
/// itself — the config file soldr reads is the one written here.
fn apply_from_project_with_config(body: &str) -> std::process::Command {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().join("soldr"));
    let project = root.path().join("project");
    std::fs::create_dir_all(project.join(".cargo")).expect("mkdir .cargo");
    std::fs::write(project.join(".cargo").join("config.toml"), body).expect("write config.toml");

    let mut command = std::process::Command::new("cargo");
    let _cwd = crate::CwdGuard::enter(&project);
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(target::apply_linker_override(
            &mut command,
            &argvec(&format!("build --target {TARGET}")),
            None,
            &paths,
        ))
        .expect("apply_linker_override");
    command
}

/// Cargo gives `CARGO_TARGET_<TRIPLE>_LINKER` precedence over the config file,
/// so the *automatic* default (no `SOLDR_LINKER`, no `~/.soldr/config.toml`
/// `linker =`) must not inject one at all. Before this guard the soldr#3262
/// reld default silently replaced the project's declared linker.
#[test]
fn automatic_linker_defers_to_project_target_linker() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _soldr_linker = EnvVarGuard::remove("SOLDR_LINKER");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);

    let command = apply_from_project_with_config(&format!("[target.{TARGET}]\nlinker = \"cc\"\n"));

    assert_eq!(
        command_env_override(&command, LINKER_KEY),
        None,
        "the automatic default must not clobber the project's [target.{TARGET}] linker",
    );
    assert_eq!(
        command_env_override(&command, RUSTFLAGS_KEY),
        None,
        "and must not clobber it through rustflags either",
    );
}

/// `rustflags` alone is enough to suppress the automatic default: Cargo's
/// `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` replaces the config file's target
/// rustflags outright rather than merging with them.
#[test]
fn automatic_linker_defers_to_project_target_rustflags() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _soldr_linker = EnvVarGuard::remove("SOLDR_LINKER");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);

    let command = apply_from_project_with_config(&format!(
        "[target.{TARGET}]\nrustflags = [\"-C\", \"target-cpu=native\"]\n"
    ));

    assert_eq!(command_env_override(&command, LINKER_KEY), None);
    assert_eq!(command_env_override(&command, RUSTFLAGS_KEY), None);
}

/// The guard is scoped to the *matching* triple: a config section for some
/// other target says nothing about this build, so the default still applies.
#[test]
fn automatic_linker_still_injects_for_an_unrelated_target_section() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _soldr_linker = EnvVarGuard::remove("SOLDR_LINKER");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);

    let command =
        apply_from_project_with_config("[target.x86_64-pc-windows-msvc]\nlinker = \"cc\"\n");

    assert_generated_linux_linker(
        &command,
        "a config section for another triple must not suppress the default",
    );
}

/// The other half of the soldr#3277 rule: an explicit `SOLDR_LINKER` request is
/// a user decision too, and a more direct one, so it still wins over the
/// project's `.cargo/config.toml`.
#[test]
fn explicit_linker_request_still_overrides_project_target_config() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);
    let _soldr_linker = EnvVarGuard::set("SOLDR_LINKER", "fast");

    let command = apply_from_project_with_config(&format!("[target.{TARGET}]\nlinker = \"cc\"\n"));

    assert_generated_linux_linker(
        &command,
        "an explicit SOLDR_LINKER request outranks the project's config",
    );
}
