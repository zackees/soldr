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
use std::path::Path;

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

/// Build a project root whose `Cargo.toml` declares
/// `[workspace.metadata.soldr] linker = "reld"` (soldr#3276 §6 dogfooding),
/// then run `apply_linker_override` from inside it.
fn apply_from_project_with_cargo_toml_metadata_linker(reld_bin: &Path) -> std::process::Command {
    // `.keep()` rather than a `TempDir` guard: the linker shim is materialized
    // under `paths.bin` and read back by the caller after this function
    // returns, so the directory must outlive the local `TempDir` drop.
    let root = tempfile::tempdir().expect("tempdir").keep();
    let paths = SoldrPaths::with_root(root.join("soldr"));
    let project = root.join("project");
    std::fs::create_dir_all(&project).expect("mkdir project");
    std::fs::write(
        project.join("Cargo.toml"),
        "[workspace]\nmembers = []\n\n[workspace.metadata.soldr]\nlinker = \"reld\"\n",
    )
    .expect("write Cargo.toml");

    let _reld_bin = EnvVarGuard::set(
        crate::fetch::RELD_BIN_ENV_VAR,
        reld_bin.to_str().expect("utf8 reld path"),
    );

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

/// soldr#3276 §6: the dogfooding gap. A project that declares
/// `linker = "reld"` in its own `Cargo.toml` `[workspace.metadata.soldr]`
/// table must never hand clang a *bare* `reld` name in `--ld-path=reld` —
/// clang requires `--ld-path=` to be an absolute path
/// (`clang: error: invalid linker name in argument '--ld-path=reld'`).
/// The resolved managed/fetched reld's absolute path must be substituted in
/// before the linker shim is materialized. This exercises the
/// `LinkerSource::CargoTomlMetadata` branch end to end through
/// `apply_linker_override`, which no other test in this file or
/// `cli_cargo_linker.rs` covers (those cover env, `.cargo/config.toml`, and
/// `~/.soldr/config.toml`, not `Cargo.toml` metadata).
#[test]
fn cargo_toml_metadata_linker_reld_resolves_absolute_path_end_to_end() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _soldr_linker = EnvVarGuard::remove("SOLDR_LINKER");
    let _build_target = EnvVarGuard::remove("CARGO_BUILD_TARGET");
    let _parent_linker = EnvVarGuard::remove(LINKER_KEY);
    let _parent_rustflags = EnvVarGuard::remove(RUSTFLAGS_KEY);

    let reld_dir = tempfile::tempdir().expect("reld tempdir");
    let reld_bin = reld_dir.path().join(format!(
        "reld{}",
        crate::platform::executable::name::script_suffix()
    ));
    std::fs::write(&reld_bin, "#!/bin/sh\nexit 0\n").expect("write fake reld");
    crate::platform::fs::permissions::make_executable(&reld_bin).expect("chmod +x");

    let command = apply_from_project_with_cargo_toml_metadata_linker(&reld_bin);

    let linker = command_env_override(&command, LINKER_KEY)
        .flatten()
        .expect("CARGO_TARGET_<TRIPLE>_LINKER must be injected for a declared reld linker");
    let linker = linker.to_string_lossy();

    // The bug this guards against: a bare `--ld-path=reld` (or a bare
    // `reld` CARGO_TARGET_*_LINKER value) reaching clang/cargo unresolved.
    assert!(
        !linker.ends_with("reld") && linker != "reld",
        "linker injection must not be a bare `reld` name, got: {linker}"
    );

    // On Linux the value is a generated content-addressed clang driver shim
    // (materialize_linker_driver_shim); everywhere else it is the direct
    // absolute reld path. Either way it must never be the literal `reld`.
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Linux {
        assert!(
            linker.contains("linker-shims"),
            "linux reld uses a generated clang driver shim: {linker}"
        );
        let shim_body = std::fs::read_to_string(linker.as_ref()).expect("read shim");
        assert!(
            shim_body.contains(&format!("--ld-path={}", reld_bin.display())),
            "shim must embed the resolved absolute reld path, not a bare name: {shim_body}"
        );
        assert!(
            !shim_body.contains("--ld-path=reld\n") && !shim_body.contains("--ld-path=reld "),
            "shim must not contain the unresolved bare `--ld-path=reld`: {shim_body}"
        );
    } else {
        assert_eq!(linker, reld_bin.to_string_lossy());
    }
}
