//! `soldr cargo ...` front door, profile-debug-default detection, linker
//! injection, low-disk warning, and the cargo arg-parsing helpers shared
//! with `rust_plan`. Extracted from `main.rs` as part of issue #339.

use crate::trampoline::{refresh_sidecar_after_cargo, try_run_trampoline, TrampolineDecision};
use crate::trampoline_workspace::{
    detect_workspace_verb, refresh_workspace_sidecar_after_cargo, try_workspace_trampoline,
    RawClippyCapture, WorkspaceDecision, WorkspaceVerb,
};
use crate::zccache::{finish_zccache_build, prepare_rustc_wrapper};
use crate::{
    apply_implicit_toolchain_homes, gc, linker, non_empty_env_path, resolve_toolchain_binary,
    rust_plan, ZccacheSourceArg, CARGO_PROFILE_DEV_DEBUG_ENV_VAR, CARGO_PROFILE_TEST_DEBUG_ENV_VAR,
    LINKER_ENV_VAR, LOW_DISK_WARNING_THRESHOLD_BYTES, TEST_FREE_DISK_BYTES_ENV_VAR,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use soldr_core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use soldr_fetch::VersionSpec;
use std::collections::BTreeSet;
use std::io::Write;

pub(crate) async fn run_cargo_front_door(
    args: &[String],
    cache_enabled: bool,
    zccache_source: ZccacheSourceArg,
) -> Result<i32, SoldrError> {
    if cargo_args_use_reserved_no_cache(args) {
        return Err(SoldrError::Other(
            "`--no-cache` must appear before `cargo`, as in `soldr --no-cache cargo build`".into(),
        ));
    }

    // `cargo run` trampoline (issue #344). When the binary is already
    // up-to-date with the recorded sources, this exec's the binary
    // directly and never spawns cargo. Otherwise we get back a plan that
    // strips the soldr-private `--no-trampoline` flag from the arg list
    // and lets us refresh the sidecar after cargo succeeds.
    let trampoline_plan = if is_cargo_run_invocation(args) {
        match try_run_trampoline(args)? {
            TrampolineDecision::Executed(code) => return Ok(code),
            TrampolineDecision::FellThrough(plan) => Some(plan),
        }
    } else {
        None
    };

    // Workspace-level trampoline (issue #354, Tier L3 of #352). Covers
    // `build`, `check`, `clippy` — same model as the run trampoline but
    // multi-output. Skipping these verbs means "exit 0 with no on-disk
    // changes" (build/check) or "replay captured diagnostics" (clippy).
    //
    // The trampoline is suppressed when a fake-cargo test override is set
    // (`SOLDR_TEST_CARGO_BIN` or `SOLDR_REAL_CARGO`) so the cargo-front-door
    // integration tests — which intentionally invoke `cargo build` from the
    // soldr worktree CWD with a fake cargo — never get short-circuited by
    // a workspace sidecar from a previous real build of the worktree. The
    // `cargo run` trampoline is left enabled in test mode because the
    // run-trampoline tests rely on `SOLDR_TEST_CARGO_BIN=<broken>` to
    // *prove* the fast path took effect.
    let workspace_plan =
        if trampoline_plan.is_none() && !workspace_trampoline_suppressed_for_tests() {
            match detect_workspace_verb(args) {
                Some(verb) => match try_workspace_trampoline(verb, args)? {
                    WorkspaceDecision::Skipped(code) => return Ok(code),
                    WorkspaceDecision::FellThrough(plan) => Some(plan),
                },
                None => None,
            }
        } else {
            None
        };

    // Use the cleaned arg vector from here on so `--no-trampoline` is
    // not forwarded to cargo.
    let owned_cleaned_args;
    let args: &[String] = match (trampoline_plan.as_ref(), workspace_plan.as_ref()) {
        (Some(plan), _) => {
            owned_cleaned_args = plan.cleaned_args.clone();
            &owned_cleaned_args
        }
        (None, Some(plan)) => {
            owned_cleaned_args = plan.cleaned_args.clone();
            &owned_cleaned_args
        }
        (None, None) => args,
    };

    let cargo = resolve_toolchain_binary("cargo")?;
    let rustc = resolve_toolchain_binary("rustc")?;
    let cargo_bin_dir = cargo
        .parent()
        .ok_or_else(|| SoldrError::Other("failed to resolve cargo bin directory".into()))?
        .to_path_buf();
    let existing_path = std::env::var_os("PATH");
    let paths = SoldrPaths::new()?;
    paths.ensure_dirs()?;

    // If the user invoked a known ecosystem subcommand (e.g. `cargo nextest`),
    // fetch the corresponding `cargo-<sub>` binary and prepend its directory to
    // PATH so cargo's subcommand dispatch finds it.
    let extra_bin_dirs = ensure_known_subcommand_tool(args, &paths).await?;

    let mut command = std::process::Command::new(&cargo);
    command.args(args);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    // soldr cargo is the top of the invocation tree, so any inherited
    // MAKEFLAGS/CARGO_MAKEFLAGS points at jobserver fds that aren't open in
    // our process. Stripping them lets cargo start a fresh jobserver instead
    // of printing the "failed to connect to jobserver" warning (see #283).
    command.env_remove("MAKEFLAGS");
    command.env_remove("CARGO_MAKEFLAGS");
    command.env("RUSTC", &rustc);
    let build_like_cargo = cargo_args_are_cacheable(args);
    let cache_enabled_for_cargo = cache_enabled && build_like_cargo;
    let cargo_profile_debug_default = if build_like_cargo {
        maybe_apply_cargo_profile_debug_default(&mut command, args, &paths)?
    } else {
        None
    };

    command.env(
        soldr_cache::CACHE_ENABLED_ENV_VAR,
        soldr_cache::cache_enabled_env_value(cache_enabled_for_cargo),
    );
    if build_like_cargo {
        // Cargo front door only: keep startup/low-disk warnings off unrelated
        // commands and out of the rustc-wrapper hot path.
        gc::emit_startup_target_warning_if_due();
        // Best-effort auto-GC trigger (issue #323). Runs on a detached
        // background thread; never blocks the build.
        gc::maybe_kick_auto_gc(&paths);
    }
    let mut path_dirs: Vec<std::path::PathBuf> = Vec::with_capacity(1 + extra_bin_dirs.len());
    path_dirs.push(cargo_bin_dir);
    path_dirs.extend(extra_bin_dirs);
    command.env("PATH", prepend_paths(&path_dirs, existing_path.as_deref())?);
    let explicit_target = default_cargo_build_target(args)?;
    if let Some(target) = explicit_target.as_deref() {
        command.env("CARGO_BUILD_TARGET", target);
    }

    apply_linker_override(&mut command, args, explicit_target.as_deref(), &paths)?;

    let session = if cache_enabled_for_cargo {
        prepare_rustc_wrapper(&mut command, &paths, zccache_source).await?
    } else {
        None
    };

    let plan_ctx = if let Some(session) = session.as_ref() {
        rust_plan::maybe_prepare_rust_artifact_plan(
            &cargo,
            &rustc,
            args,
            session,
            cargo_profile_debug_default.as_ref(),
        )?
    } else {
        None
    };
    if build_like_cargo {
        let probe_path = plan_ctx
            .as_ref()
            .map(|plan| std::path::PathBuf::from(&plan.target_dir))
            .unwrap_or_else(|| cargo_disk_space_probe_path(args));
        maybe_emit_low_disk_warning(&probe_path);
    }
    if let Some(plan) = plan_ctx.as_ref() {
        if let Some(reason) = rust_plan::should_skip_warm_restore(plan) {
            eprintln!("{reason}");
        } else {
            rust_plan::run_zccache_rust_plan(plan, "restore", false)?;
        }
    }

    // For clippy fall-through we need to TEE cargo's stdout/stderr so the
    // user sees diagnostics live AND we have a copy to store in the
    // sidecar for the next run. For build/check we don't need the output;
    // just inherit fds via .status().
    let needs_clippy_capture = matches!(
        workspace_plan.as_ref().map(|p| p.verb),
        Some(WorkspaceVerb::Clippy)
    );
    let (status, clippy_capture) = if needs_clippy_capture {
        let (status, capture) = run_command_capturing_clippy(&mut command)?;
        (status, Some(capture))
    } else {
        (command.status()?, None)
    };
    if status.success() {
        if let Some(plan) = plan_ctx.as_ref() {
            rust_plan::run_zccache_rust_plan(plan, "save", true)?;
            rust_plan::write_warm_restore_sentinel(plan);
        }
        if let Some(plan) = trampoline_plan.as_ref() {
            refresh_sidecar_after_cargo(plan);
        }
        if let Some(plan) = workspace_plan.as_ref() {
            refresh_workspace_sidecar_after_cargo(plan, clippy_capture);
        }
    } else if let Some(plan) = plan_ctx.as_ref() {
        // A non-zero cargo exit can leave orphan `.rmeta` files (rmeta
        // emitted, then rustc aborted before the `.rlib` codegen pass)
        // in `target/<triple>/<profile>/deps/`. Subsequent invocations
        // then fail with `E0463: can't find crate` because cargo passes
        // `--extern X=orphan.rmeta` to dependents and rustc cannot link
        // an rmeta-only crate. Sweep them so the next build rebuilds
        // cleanly. See soldr#410.
        rust_plan::prune_orphan_rmetas_after_failed_build(plan);
    }
    if let Some(session) = session {
        finish_zccache_build(&session)?;
    }
    drop(trampoline_plan);
    drop(workspace_plan);
    Ok(status.code().unwrap_or(1))
}

/// Run cargo while capturing its stdout/stderr into in-memory buffers and
/// also tee'ing the bytes back to our own stdout/stderr so the user sees
/// them in real time. Returns the captured buffers plus the exit status so
/// the sidecar refresh can persist them for the next clippy invocation.
fn run_command_capturing_clippy(
    command: &mut std::process::Command,
) -> Result<(std::process::ExitStatus, RawClippyCapture), SoldrError> {
    use std::io::Read;

    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command.spawn().map_err(|err| {
        SoldrError::Other(format!("spawn cargo for clippy capture failed: {err}"))
    })?;
    let mut child_stdout = child.stdout.take().expect("piped");
    let mut child_stderr = child.stderr.take().expect("piped");

    // Read both streams in parallel using OS threads so neither side
    // deadlocks if it fills its pipe before the consumer drains it.
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let stdout = std::io::stdout();
        loop {
            match child_stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    let _ = stdout.lock().write_all(&chunk[..n]);
                }
                Err(_) => break,
            }
        }
        let _ = stdout.lock().flush();
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let stderr = std::io::stderr();
        loop {
            match child_stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    let _ = stderr.lock().write_all(&chunk[..n]);
                }
                Err(_) => break,
            }
        }
        let _ = stderr.lock().flush();
        buf
    });

    let status = child
        .wait()
        .map_err(|err| SoldrError::Other(format!("wait on cargo clippy failed: {err}")))?;
    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();
    let capture = RawClippyCapture {
        stdout,
        stderr,
        exit_code: status.code().unwrap_or(1),
    };
    Ok((status, capture))
}

/// True when the cargo argv resolves to `cargo run` (or `cargo r`).
fn is_cargo_run_invocation(args: &[String]) -> bool {
    matches!(first_cargo_subcommand(args), Some("run" | "r"))
}

/// Detect a fake-cargo test harness via the same env vars the binaries
/// resolver consults. When one is set we skip the workspace trampoline so
/// tests that invoke `cargo build`/`check`/`clippy` from the soldr worktree
/// CWD never get short-circuited by a stale workspace sidecar in the
/// repo's own `target/` directory. Tests that explicitly want to exercise
/// the workspace trampoline path (with a broken-cargo stub proving the
/// trampoline took effect) set `SOLDR_TEST_FORCE_WORKSPACE_TRAMPOLINE=1`
/// to opt back in.
fn workspace_trampoline_suppressed_for_tests() -> bool {
    if std::env::var_os(TEST_FORCE_WORKSPACE_TRAMPOLINE_ENV_VAR).is_some() {
        return false;
    }
    fn non_empty(name: &str) -> bool {
        std::env::var_os(name)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }
    non_empty(crate::TEST_CARGO_BIN_ENV_VAR)
        || non_empty(&format!("{}CARGO", crate::REAL_TOOLCHAIN_BINARY_ENV_PREFIX))
}

const TEST_FORCE_WORKSPACE_TRAMPOLINE_ENV_VAR: &str = "SOLDR_TEST_FORCE_WORKSPACE_TRAMPOLINE";

pub(crate) fn cargo_profile(args: &[String]) -> &str {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--release" {
            return "release";
        }
        if arg == "--profile" {
            return iter.next().map(String::as_str).unwrap_or("debug");
        }
        if let Some(value) = arg.strip_prefix("--profile=") {
            return value;
        }
    }
    "debug"
}

pub(crate) fn cargo_target_triple(args: &[String], host: &str) -> String {
    cargo_target_arg(args)
        .or_else(|| std::env::var("CARGO_BUILD_TARGET").ok())
        .unwrap_or_else(|| host.to_string())
}

fn cargo_target_arg(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--target" {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--target=") {
            return Some(value.to_string());
        }
    }
    None
}

pub(crate) fn cargo_feature_inputs(args: &[String]) -> Vec<String> {
    selected_cargo_args(
        args,
        &[
            "--features",
            "--all-features",
            "--no-default-features",
            "--package",
            "-p",
            "--workspace",
            "--exclude",
            "--all-targets",
            "--lib",
            "--bins",
            "--bin",
            "--examples",
            "--example",
            "--tests",
            "--test",
            "--benches",
            "--bench",
        ],
    )
}

pub(crate) fn selected_cargo_args(args: &[String], names: &[&str]) -> Vec<String> {
    let mut selected = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if names.contains(&arg.as_str()) {
            selected.push(arg.clone());
            if !matches!(
                arg.as_str(),
                "--all-features"
                    | "--no-default-features"
                    | "--workspace"
                    | "--all-targets"
                    | "--lib"
                    | "--bins"
                    | "--examples"
                    | "--tests"
                    | "--benches"
            ) {
                if let Some(value) = iter.next() {
                    selected.push(value.clone());
                }
            }
            continue;
        }
        if names
            .iter()
            .any(|name| arg.starts_with(&format!("{name}=")))
        {
            selected.push(arg.clone());
        }
    }
    selected
}

pub(crate) fn rustflags_inputs() -> Vec<(String, String)> {
    sorted_env_vars(|name| {
        name == "RUSTFLAGS"
            || name == "CARGO_ENCODED_RUSTFLAGS"
            || (name.starts_with("CARGO_TARGET_") && name.ends_with("_RUSTFLAGS"))
    })
}

pub(crate) fn build_env_inputs(
    cargo_profile_debug_default: Option<&CargoProfileDebugDefault>,
) -> Vec<(String, String)> {
    let mut vars = sorted_env_vars(|name| {
        name == "CARGO_BUILD_TARGET"
            || name == "CARGO_TARGET_DIR"
            || name.starts_with("CARGO_PROFILE_")
            || name.starts_with("CARGO_CFG_")
    });
    if let Some(default) = cargo_profile_debug_default {
        if !vars.iter().any(|(name, _)| name == default.env_var) {
            vars.push((default.env_var.to_string(), "false".to_string()));
        }
        vars.sort_by(|a, b| a.0.cmp(&b.0));
    }
    vars
}

fn sorted_env_vars<F>(include: F) -> Vec<(String, String)>
where
    F: Fn(&str) -> bool,
{
    let mut vars = std::env::vars()
        .filter(|(name, _)| include(name))
        .collect::<Vec<_>>();
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    vars
}

pub(crate) fn workspace_manifest_hashes(
    workspace_root: &std::path::Path,
) -> Result<Vec<String>, SoldrError> {
    let mut hashes = Vec::new();
    collect_manifest_hashes(workspace_root, workspace_root, &mut hashes)?;
    hashes.sort();
    Ok(hashes)
}

fn collect_manifest_hashes(
    workspace_root: &std::path::Path,
    dir: &std::path::Path,
    hashes: &mut Vec<String>,
) -> Result<(), SoldrError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if matches!(
                entry.file_name().to_str(),
                Some(".git" | "target" | ".soldr" | "node_modules")
            ) {
                continue;
            }
            collect_manifest_hashes(workspace_root, &path, hashes)?;
        } else if file_type.is_file() && entry.file_name() == std::ffi::OsStr::new("Cargo.toml") {
            let relative = path
                .strip_prefix(workspace_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            hashes.push(format!("{relative}:{}", file_hash_or_missing(&path)?));
        }
    }
    Ok(())
}

pub(crate) fn cargo_config_hash(workspace_root: &std::path::Path) -> Result<String, SoldrError> {
    let mut inputs = Vec::new();
    for relative in [".cargo/config.toml", ".cargo/config"] {
        let path = workspace_root.join(relative);
        if path.exists() {
            inputs.push(format!("{relative}:{}", file_hash_or_missing(&path)?));
        }
    }
    Ok(stable_hash_json(&inputs))
}

pub(crate) fn file_hash_or_missing(path: &std::path::Path) -> Result<String, SoldrError> {
    if !path.exists() {
        return Ok("missing".to_string());
    }
    Ok(sha256_bytes(&std::fs::read(path)?))
}

pub(crate) fn stable_hash_json<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    sha256_bytes(&bytes)
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

pub(crate) fn path_string(path: &std::path::Path) -> String {
    path.display().to_string()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CargoProfileDebugDefault {
    pub(crate) profile: &'static str,
    pub(crate) env_var: &'static str,
}

impl CargoProfileDebugDefault {
    fn for_profile(profile: &str) -> Option<Self> {
        match profile {
            "dev" | "debug" => Some(Self {
                profile: "dev",
                env_var: CARGO_PROFILE_DEV_DEBUG_ENV_VAR,
            }),
            "test" => Some(Self {
                profile: "test",
                env_var: CARGO_PROFILE_TEST_DEBUG_ENV_VAR,
            }),
            _ => None,
        }
    }

    fn lookup_profiles(self) -> &'static [&'static str] {
        match self.profile {
            "test" => &["test", "dev"],
            _ => &["dev"],
        }
    }
}

fn maybe_apply_cargo_profile_debug_default(
    command: &mut std::process::Command,
    args: &[String],
    paths: &SoldrPaths,
) -> Result<Option<CargoProfileDebugDefault>, SoldrError> {
    let Some(default) = cargo_profile_debug_default_for_args(args) else {
        return Ok(None);
    };
    if cargo_profile_debug_is_specified(args, default)? {
        return Ok(None);
    }

    command.env(default.env_var, "false");
    let repo_path = cargo_debug_warning_repo_path(args);
    if should_emit_cargo_debug_default_warning(paths, &repo_path) {
        eprintln!(
            "soldr: warning: Cargo profile.{}.debug is unspecified for {}; setting {}=false for this invocation. Add `debug = true` or `debug = false` under `[profile.{}]` in Cargo.toml or .cargo/config.toml to make this explicit.",
            default.profile,
            repo_path.display(),
            default.env_var,
            default.profile
        );
    }

    Ok(Some(default))
}

fn cargo_profile_debug_default_for_args(args: &[String]) -> Option<CargoProfileDebugDefault> {
    let subcommand = first_cargo_subcommand(args)?;

    if subcommand == "nextest" {
        return if cargo_args_contain_release(args) {
            None
        } else {
            CargoProfileDebugDefault::for_profile("test")
        };
    }

    if cargo_args_contain_release(args) {
        return None;
    }

    if let Some(profile) = cargo_profile_arg_value(args) {
        return CargoProfileDebugDefault::for_profile(&profile);
    }

    match subcommand {
        "t" | "test" => CargoProfileDebugDefault::for_profile("test"),
        "install" if cargo_install_args_contain_debug(args) => {
            CargoProfileDebugDefault::for_profile("dev")
        }
        "install" | "bench" => None,
        "b" | "build" | "c" | "check" | "d" | "doc" | "r" | "run" | "rustc" | "clippy" | "fix" => {
            CargoProfileDebugDefault::for_profile("dev")
        }
        _ => None,
    }
}

fn cargo_profile_debug_is_specified(
    args: &[String],
    default: CargoProfileDebugDefault,
) -> Result<bool, SoldrError> {
    let profiles = default.lookup_profiles();
    if profiles.iter().any(|profile| {
        cargo_profile_debug_env_var(profile)
            .is_some_and(|env_var| std::env::var_os(env_var).is_some())
    }) {
        return Ok(true);
    }

    if cargo_config_args_specify_profile_debug(args, profiles)? {
        return Ok(true);
    }

    let start_dir = cargo_profile_lookup_start_dir(args)?;
    if cargo_manifest_specifies_profile_debug(&start_dir, profiles) {
        return Ok(true);
    }
    if cargo_config_files_specify_profile_debug(&start_dir, profiles) {
        return Ok(true);
    }

    Ok(false)
}

fn cargo_profile_debug_env_var(profile: &str) -> Option<&'static str> {
    match profile {
        "dev" => Some(CARGO_PROFILE_DEV_DEBUG_ENV_VAR),
        "test" => Some(CARGO_PROFILE_TEST_DEBUG_ENV_VAR),
        _ => None,
    }
}

fn cargo_args_contain_release(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--release")
}

fn cargo_profile_arg_value(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--profile" {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--profile=") {
            return Some(value.to_string());
        }
    }
    None
}

fn cargo_install_args_contain_debug(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--debug")
}

fn cargo_config_args_specify_profile_debug(
    args: &[String],
    profiles: &[&str],
) -> Result<bool, SoldrError> {
    let cwd = std::env::current_dir()?;
    for value in cargo_config_arg_values(args) {
        if cargo_config_arg_specifies_profile_debug(&value, &cwd, profiles) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn cargo_config_arg_values(args: &[String]) -> Vec<String> {
    let mut values = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--config" {
            if let Some(value) = iter.next() {
                values.push(value.clone());
            }
            continue;
        }
        if let Some(value) = arg.strip_prefix("--config=") {
            values.push(value.to_string());
        }
    }
    values
}

fn cargo_config_arg_specifies_profile_debug(
    value: &str,
    cwd: &std::path::Path,
    profiles: &[&str],
) -> bool {
    let raw = value.trim();
    if raw.is_empty() {
        return false;
    }

    let path = std::path::Path::new(raw);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if path.is_file() {
        return toml_file_specifies_profile_debug(&path, profiles);
    }

    toml_text_specifies_profile_debug(raw, profiles)
        .unwrap_or_else(|| raw_may_specify_profile_debug(raw, profiles))
}

fn raw_may_specify_profile_debug(raw: &str, profiles: &[&str]) -> bool {
    let lowered = raw.to_ascii_lowercase();
    profiles.iter().any(|profile| {
        lowered.contains(&format!("profile.{profile}.debug"))
            || (lowered.contains(&format!("[profile.{profile}]")) && lowered.contains("debug"))
    })
}

fn cargo_manifest_specifies_profile_debug(start_dir: &std::path::Path, profiles: &[&str]) -> bool {
    find_workspace_manifest_path(start_dir)
        .is_some_and(|manifest| toml_file_specifies_profile_debug(&manifest, profiles))
}

fn cargo_config_files_specify_profile_debug(
    start_dir: &std::path::Path,
    profiles: &[&str],
) -> bool {
    cargo_config_paths(start_dir)
        .iter()
        .any(|path| toml_file_specifies_profile_debug(path, profiles))
}

fn cargo_config_paths(start_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths = BTreeSet::new();
    let mut current = Some(start_dir.to_path_buf());
    while let Some(dir) = current {
        for relative in [".cargo/config.toml", ".cargo/config"] {
            let path = dir.join(relative);
            if path.is_file() {
                paths.insert(path);
            }
        }
        current = dir.parent().map(std::path::Path::to_path_buf);
    }

    if let Some(cargo_home) = cargo_home_dir_for_config() {
        for name in ["config.toml", "config"] {
            let path = cargo_home.join(name);
            if path.is_file() {
                paths.insert(path);
            }
        }
    }

    paths.into_iter().collect()
}

fn cargo_home_dir_for_config() -> Option<std::path::PathBuf> {
    std::env::var_os(soldr_core::CARGO_HOME_ENV_VAR)
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            soldr_core::user_home_dir()
                .ok()
                .map(|home| home.join(".cargo"))
        })
}

fn toml_file_specifies_profile_debug(path: &std::path::Path, profiles: &[&str]) -> bool {
    match std::fs::read_to_string(path) {
        Ok(text) => toml_text_specifies_profile_debug(&text, profiles).unwrap_or(true),
        Err(_) => true,
    }
}

fn toml_text_specifies_profile_debug(text: &str, profiles: &[&str]) -> Option<bool> {
    let value: toml::Value = text.parse().ok()?;
    let Some(profile_table) = value.get("profile") else {
        return Some(false);
    };
    Some(profiles.iter().any(|profile| {
        profile_table
            .get(*profile)
            .and_then(|section| section.get("debug"))
            .is_some()
    }))
}

fn cargo_profile_lookup_start_dir(args: &[String]) -> Result<std::path::PathBuf, SoldrError> {
    let cwd = std::env::current_dir()?;
    let Some(manifest_path) = cargo_manifest_path_arg(args) else {
        return Ok(cwd);
    };
    let manifest_path = if manifest_path.is_absolute() {
        manifest_path
    } else {
        cwd.join(manifest_path)
    };
    let parent = manifest_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or(cwd);
    Ok(parent)
}

fn cargo_manifest_path_arg(args: &[String]) -> Option<std::path::PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--manifest-path" {
            return iter.next().map(std::path::PathBuf::from);
        }
        if let Some(value) = arg.strip_prefix("--manifest-path=") {
            return Some(std::path::PathBuf::from(value));
        }
    }
    None
}

fn find_workspace_manifest_path(start_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = start_dir.to_path_buf();
    let mut nearest_manifest = None;
    let mut workspace_manifest = None;

    loop {
        let candidate = current.join("Cargo.toml");
        if candidate.is_file() {
            if nearest_manifest.is_none() {
                nearest_manifest = Some(candidate.clone());
            }
            if cargo_manifest_declares_workspace(&candidate) {
                workspace_manifest = Some(candidate);
            }
        }
        if !current.pop() {
            break;
        }
    }

    workspace_manifest.or(nearest_manifest)
}

fn cargo_manifest_declares_workspace(path: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = text.parse::<toml::Value>() else {
        return false;
    };
    value.get("workspace").is_some()
}

fn cargo_debug_warning_repo_path(args: &[String]) -> std::path::PathBuf {
    let start_dir = cargo_profile_lookup_start_dir(args)
        .or_else(|_| std::env::current_dir().map_err(SoldrError::from))
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    find_git_root(&start_dir)
        .or_else(|| {
            find_workspace_manifest_path(&start_dir)
                .and_then(|manifest| manifest.parent().map(std::path::Path::to_path_buf))
        })
        .unwrap_or(start_dir)
}

fn find_git_root(start_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = start_dir.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn should_emit_cargo_debug_default_warning(
    paths: &SoldrPaths,
    repo_path: &std::path::Path,
) -> bool {
    let db_path = soldr_cache::state_db_path(paths);
    soldr_cache::state_db::StateDb::open(&db_path)
        .and_then(|db| db.should_emit_cargo_debug_default_warning(repo_path))
        .unwrap_or(true)
}

/// Apply the `SOLDR_LINKER` / `config.toml linker = ...` override (issue
/// #285) to the cargo subprocess command.
///
/// The active target triple is resolved in the same order as cargo:
/// 1. an explicit `CARGO_BUILD_TARGET` injected by `default_cargo_build_target`,
/// 2. a `CARGO_BUILD_TARGET` already in the parent env,
/// 3. an `--target` flag inside `args`,
/// 4. the auto-detected host triple from `TargetTriple::detect()`.
fn apply_linker_override(
    command: &mut std::process::Command,
    args: &[String],
    explicit_target: Option<&str>,
    paths: &SoldrPaths,
) -> Result<(), SoldrError> {
    let config = paths.load_config();
    let choice = linker::from_env_and_config(
        std::env::var_os(LINKER_ENV_VAR).as_deref(),
        config.linker.as_deref(),
    )?;
    if matches!(choice, linker::LinkerChoice::Default) {
        // Fast-path: skip target detection entirely when there is nothing
        // to inject. Keeps `soldr cargo` no-ops on platforms where target
        // detection might fail or be slow.
        return Ok(());
    }

    let target = resolve_active_target_triple(args, explicit_target)?;
    let injection = linker::resolve_for_target(choice, &target)?;
    let prefix = linker::cargo_target_env_prefix(&target);
    if let Some(linker_path) = injection.linker {
        command.env(format!("CARGO_TARGET_{prefix}_LINKER"), linker_path);
    }
    if let Some(rustflags) = injection.rustflags {
        command.env(format!("CARGO_TARGET_{prefix}_RUSTFLAGS"), rustflags);
    }
    Ok(())
}

fn resolve_active_target_triple(
    args: &[String],
    explicit_target: Option<&str>,
) -> Result<String, SoldrError> {
    if let Some(target) = explicit_target {
        return Ok(target.to_string());
    }
    if let Some(target) = std::env::var_os("CARGO_BUILD_TARGET") {
        if let Some(s) = target.to_str() {
            let s = s.trim();
            if !s.is_empty() {
                return Ok(s.to_string());
            }
        }
    }
    if let Some(target) = cargo_args_target_value(args) {
        return Ok(target);
    }
    Ok(soldr_core::TargetTriple::detect()?.triple())
}

fn cargo_args_target_value(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--target" {
            return iter.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix("--target=") {
            return Some(rest.to_string());
        }
    }
    None
}

pub(crate) fn cargo_args_specify_target(args: &[String]) -> bool {
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

pub(crate) fn cargo_args_use_reserved_no_cache(args: &[String]) -> bool {
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

pub(crate) fn cargo_args_are_cacheable(args: &[String]) -> bool {
    let Some(subcommand) = first_cargo_subcommand(args) else {
        return false;
    };

    if is_cacheable_cargo_subcommand(subcommand) {
        return true;
    }

    // cargo-watch (issue #341): the outer cargo invocation is `watch`, but the
    // process it spawns on every file change is `cargo <inner>`. If the inner
    // subcommand is itself cacheable, we want to seed `RUSTC_WRAPPER=zccache`
    // (and friends) on the cargo-watch process so the children inherit the
    // wrapper. Only `watch` is handled here; do not add other wrappers
    // (e.g. bacon) without verifying their argv shape.
    if subcommand == "watch" {
        return cargo_watch_inner_is_cacheable(args);
    }

    false
}

fn is_cacheable_cargo_subcommand(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "b" | "build"
            | "c"
            | "check"
            | "t"
            | "test"
            | "bench"
            | "d"
            | "doc"
            | "r"
            | "run"
            | "rustc"
            | "clippy"
            | "fix"
            | "install"
            | "nextest"
    )
}

/// Scan a `cargo watch ...` arg list for `-x` / `--exec` / `-s` / `--shell`
/// values whose first whitespace-tokenized word names a cacheable cargo
/// subcommand. cargo-watch accepts multiple `-x` flags and runs them in
/// sequence; we treat the invocation as cacheable if ANY of them targets a
/// cacheable subcommand so the children get the wrapper. Shell form (`-s`)
/// may include a literal leading `cargo` token, which we strip before
/// classifying.
fn cargo_watch_inner_is_cacheable(args: &[String]) -> bool {
    cargo_watch_inner_subcommands(args)
        .iter()
        .any(|sub| is_cacheable_cargo_subcommand(sub))
}

fn cargo_watch_inner_subcommands(args: &[String]) -> Vec<String> {
    let mut subcommands = Vec::new();
    // Locate the outer `watch` subcommand using the same skip-flags-and-toolchain
    // pass `first_cargo_subcommand` uses, then walk only the args after it. This
    // keeps `-x` flags that happen to appear earlier (e.g. global flag values)
    // from being misread.
    let watch_idx = match cargo_subcommand_index(args, "watch") {
        Some(idx) => idx,
        None => return subcommands,
    };
    let mut iter = args.iter().skip(watch_idx + 1);
    while let Some(arg) = iter.next() {
        if arg == "--" {
            return subcommands;
        }

        // Long-form `--exec`/`--shell` with separate value.
        if arg == "--exec" || arg == "--shell" {
            if let Some(value) = iter.next() {
                if let Some(sub) = inner_subcommand_from_exec_value(value) {
                    subcommands.push(sub);
                }
            }
            continue;
        }
        // Long-form `--exec=...` / `--shell=...`.
        if let Some(value) = arg
            .strip_prefix("--exec=")
            .or_else(|| arg.strip_prefix("--shell="))
        {
            if let Some(sub) = inner_subcommand_from_exec_value(value) {
                subcommands.push(sub);
            }
            continue;
        }
        // Short-form `-x`/`-s` with separate value.
        if arg == "-x" || arg == "-s" {
            if let Some(value) = iter.next() {
                if let Some(sub) = inner_subcommand_from_exec_value(value) {
                    subcommands.push(sub);
                }
            }
            continue;
        }
        // Short-form `-x=...` / `-s=...`. cargo-watch typically uses
        // `-x VALUE`, but accept the `=` form defensively.
        if let Some(value) = arg.strip_prefix("-x=").or_else(|| arg.strip_prefix("-s=")) {
            if let Some(sub) = inner_subcommand_from_exec_value(value) {
                subcommands.push(sub);
            }
            continue;
        }
    }
    subcommands
}

/// Locate the index in `args` of `target`, skipping the same leading flags
/// (and `+toolchain` shorthand) that `first_cargo_subcommand` skips. Returns
/// `None` if no such positional appears before a `--` separator.
fn cargo_subcommand_index(args: &[String], target: &str) -> Option<usize> {
    let mut skip_next = false;
    for (idx, arg) in args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            return None;
        }
        if arg.starts_with('+') && arg.len() > 1 {
            continue;
        }
        if cargo_global_arg_takes_value(arg) {
            skip_next = !arg.contains('=');
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        if arg == target {
            return Some(idx);
        }
        return None;
    }
    None
}

fn inner_subcommand_from_exec_value(value: &str) -> Option<String> {
    let mut tokens = value.split_whitespace();
    let first = tokens.next()?;
    // `-s 'cargo build --release'` form: peel off a literal leading `cargo`
    // so the next token is the inner subcommand.
    let candidate = if first == "cargo" {
        tokens.next()?
    } else {
        first
    };
    Some(candidate.to_string())
}

fn maybe_emit_low_disk_warning(path: &std::path::Path) {
    if let Some(message) =
        low_disk_warning_for_path(path, stderr_should_use_color(), available_space)
    {
        eprintln!("{message}");
    }
}

pub(crate) fn low_disk_warning_for_path<F>(
    path: &std::path::Path,
    use_color: bool,
    available_space: F,
) -> Option<String>
where
    F: FnOnce(&std::path::Path) -> std::io::Result<u64>,
{
    let probe_path = existing_filesystem_probe_path(path);
    let free_bytes = available_space(&probe_path).ok()?;
    low_disk_warning_for_free_bytes(free_bytes, use_color)
}

pub(crate) fn low_disk_warning_for_free_bytes(free_bytes: u64, use_color: bool) -> Option<String> {
    if free_bytes >= LOW_DISK_WARNING_THRESHOLD_BYTES {
        return None;
    }
    let warning = if use_color {
        "\x1b[33mwarning\x1b[0m"
    } else {
        "warning"
    };
    Some(format!(
        "soldr: {warning}: disk space is low ({} free). Run `soldr gc` to review reclaimable Rust target directories.",
        soldr_cache::target_registry::human_size(free_bytes),
    ))
}

fn stderr_should_use_color() -> bool {
    use std::io::IsTerminal;

    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}

pub(crate) fn available_space(path: &std::path::Path) -> std::io::Result<u64> {
    if let Some(raw) = std::env::var_os(TEST_FREE_DISK_BYTES_ENV_VAR) {
        let raw = raw.to_string_lossy();
        if raw.eq_ignore_ascii_case("error") {
            return Err(std::io::Error::other("test disk-space failure"));
        }
        return raw.parse::<u64>().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid {TEST_FREE_DISK_BYTES_ENV_VAR}: {e}"),
            )
        });
    }
    fs2::available_space(path)
}

pub(crate) fn existing_filesystem_probe_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut cursor = if path.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    } else {
        path.to_path_buf()
    };
    loop {
        if cursor.exists() {
            return cursor;
        }
        if !cursor.pop() {
            return std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        }
    }
}

fn cargo_disk_space_probe_path(args: &[String]) -> std::path::PathBuf {
    if let Some(target_dir) = cargo_arg_value(args, "--target-dir") {
        return absolutize_path(std::path::PathBuf::from(target_dir));
    }
    if let Some(target_dir) = non_empty_env_path("CARGO_TARGET_DIR") {
        return absolutize_path(target_dir);
    }
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn cargo_arg_value(args: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == flag {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix(&prefix) {
            return Some(value.to_string());
        }
    }
    None
}

fn absolutize_path(path: std::path::PathBuf) -> std::path::PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    }
}

fn prepend_paths(
    dirs: &[std::path::PathBuf],
    existing_path: Option<&std::ffi::OsStr>,
) -> Result<std::ffi::OsString, SoldrError> {
    let mut paths: Vec<std::path::PathBuf> = dirs.to_vec();
    if let Some(existing_path) = existing_path {
        paths.extend(std::env::split_paths(existing_path));
    }
    std::env::join_paths(paths).map_err(|e| SoldrError::Other(format!("invalid PATH: {e}")))
}

/// Return the first positional argument (skipping flags) of the cargo
/// front-door args, which is conventionally the cargo subcommand.
pub(crate) fn first_cargo_subcommand(args: &[String]) -> Option<&str> {
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            break;
        }
        if arg.starts_with('+') && arg.len() > 1 {
            continue;
        }
        if cargo_global_arg_takes_value(arg) {
            skip_next = !arg.contains('=');
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.as_str());
    }
    None
}

fn cargo_global_arg_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-C" | "-Z"
            | "-j"
            | "--color"
            | "--config"
            | "--jobs"
            | "--manifest-path"
            | "--message-format"
            | "--target-dir"
    ) || arg.starts_with("-C=")
        || arg.starts_with("-Z=")
        || arg.starts_with("-j=")
        || arg.starts_with("--color=")
        || arg.starts_with("--config=")
        || arg.starts_with("--jobs=")
        || arg.starts_with("--manifest-path=")
        || arg.starts_with("--message-format=")
        || arg.starts_with("--target-dir=")
}

async fn ensure_known_subcommand_tool(
    args: &[String],
    paths: &SoldrPaths,
) -> Result<Vec<std::path::PathBuf>, SoldrError> {
    let Some(sub) = first_cargo_subcommand(args) else {
        return Ok(Vec::new());
    };
    let Some(spec) = soldr_fetch::lookup_by_cargo_subcommand(sub) else {
        return Ok(Vec::new());
    };

    eprintln!("soldr: fetching {}...", spec.crate_name);
    let result =
        soldr_fetch::fetch_tool_with_paths(spec.crate_name, &VersionSpec::Latest, paths).await?;

    if result.cached {
        eprintln!(
            "soldr: using cached {} v{}",
            spec.crate_name, result.version
        );
    } else {
        eprintln!("soldr: downloaded {} v{}", spec.crate_name, result.version);
    }

    let dir = result
        .binary_path
        .parent()
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "failed to resolve bin dir for fetched {}",
                spec.crate_name
            ))
        })?
        .to_path_buf();
    Ok(vec![dir])
}

#[cfg(test)]
#[path = "cargo_front_door_tests.rs"]
mod tests;
