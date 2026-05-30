//! `soldr cargo ...` front door, profile-debug-default detection, linker
//! injection, low-disk warning, and the cargo arg-parsing helpers shared
//! with `rust_plan`. Extracted from `main.rs` as part of issue #339.
//!
//! Split into sub-modules under `cargo_front_door/`:
//! - [`subcommand`] — argv-level cargo subcommand sniffing, cacheability
//!   classification, and target-flag detection.
//! - [`inputs`] — hashing inputs shared with `rust_plan` (profile,
//!   target, feature, env-var, manifest, and config hashes).
//! - [`profile_debug`] — `[profile.<P>].debug` default detection and
//!   the `CARGO_PROFILE_<P>_DEBUG=false` injection / one-shot warning.
//! - [`target`] — target-triple resolution and `SOLDR_LINKER` injection.
//! - [`disk`] — low-disk warning, free-space probing, PATH/arg helpers.
//!
//! This file owns the cross-cutting `run_cargo_front_door` entry, the
//! `--no-gc-target*` flag stripping, the cargo output-capture wrappers,
//! the known-subcommand fetch hook, and the build-session bookkeeping.

use crate::cache_lib::auto_target_gc::{auto_prune_target, render_summary, AutoPrunePhase};
use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use crate::fetch::VersionSpec;
use crate::trampoline::{refresh_sidecar_after_cargo, try_run_trampoline, TrampolineDecision};
use crate::trampoline_workspace::{
    detect_workspace_verb, refresh_workspace_sidecar_after_cargo, try_workspace_trampoline,
    RawClippyCapture, WorkspaceDecision, WorkspaceVerb,
};
use crate::zccache::{
    cache_lifecycle_from_env, command_lifetime_shutdown_timeout, CacheLifecycle,
    SOLDR_CACHE_LIFECYCLE_ENV_VAR, SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR,
};
use crate::{apply_implicit_toolchain_homes, gc, resolve_toolchain_binary, ZccacheSourceArg};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

mod cache_plan;
mod component_install;
pub(crate) mod cook_hydrate;
mod disk;
mod inputs;
mod profile_debug;
mod subcommand;
mod target;

use cache_plan::CargoCachePlan;

// -- Re-exports for cross-module callers --
//
// External modules (`gc`, `rust_plan`, `trampoline_workspace`, `main`)
// reach into `crate::cargo_front_door::*` using the names that existed
// on the flat file. Re-export them from the sub-modules so the public
// API is byte-for-byte identical after the split.
pub(crate) use disk::{
    available_space, existing_filesystem_probe_path, low_disk_warning_for_free_bytes,
    low_disk_warning_for_path,
};
pub(crate) use inputs::{
    build_env_inputs, cargo_config_hash, cargo_feature_inputs, cargo_profile, cargo_target_triple,
    file_hash_or_missing, path_string, rustflags_inputs, selected_cargo_args, sha256_bytes,
    stable_hash_json, workspace_manifest_hashes,
};
pub(crate) use profile_debug::CargoProfileDebugDefault;
pub(crate) use subcommand::{
    cargo_args_are_cacheable, cargo_args_specify_target, cargo_args_use_reserved_no_cache,
    first_cargo_subcommand,
};

/// 64-bit build session id: high 32 bits = unix-ms truncated, low 32
/// bits = pid-XOR-nanos so two concurrent builds in the same ms never
/// collide. Cheap and good enough for in-process correlation.
fn generate_build_session_id() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let high = ((nanos / 1_000_000) as u64) & 0xFFFF_FFFF;
    let low = ((nanos as u64) ^ (std::process::id() as u64)) & 0xFFFF_FFFF;
    (high << 32) | low
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Soldr-private opt-out flags for the auto target-GC hooks (#485).
/// Stripped from the arg vector before forwarding to cargo, since
/// cargo doesn't understand them.
pub(crate) const NO_GC_TARGET_FLAG: &str = "--no-gc-target";
pub(crate) const NO_GC_TARGET_BEFORE_FLAG: &str = "--no-gc-target-before";
pub(crate) const NO_GC_TARGET_AFTER_FLAG: &str = "--no-gc-target-after";
/// Env-var fallback for the wrapper-side path where cargo can't
/// forward flags to soldr. Treated as equivalent to `--no-gc-target`
/// when set to a non-empty value.
pub(crate) const NO_GC_TARGET_ENV_VAR: &str = "SOLDR_NO_GC_TARGET";

const TEST_FORCE_WORKSPACE_TRAMPOLINE_ENV_VAR: &str = "SOLDR_TEST_FORCE_WORKSPACE_TRAMPOLINE";

/// Outcome of stripping the `--no-gc-target*` flags from a cargo arg
/// vector. Mirrors the env-var fallback so callers can union all
/// inputs into a single before/after decision.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GcTargetOptOut {
    pub before: bool,
    pub after: bool,
}

impl GcTargetOptOut {
    fn merged_with_env(mut self) -> Self {
        if env_disables_target_gc() {
            self.before = true;
            self.after = true;
        }
        self
    }
}

fn env_disables_target_gc() -> bool {
    std::env::var_os(NO_GC_TARGET_ENV_VAR)
        .map(|v| {
            let s = v.to_string_lossy();
            let t = s.trim();
            !t.is_empty() && !t.eq_ignore_ascii_case("0") && !t.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

/// Remove soldr-private `--no-gc-target*` flags from the arg vector and
/// return the cleaned slice plus which passes the caller asked to skip.
/// Flags after the `--` separator are passed through untouched.
pub(crate) fn strip_no_gc_target_flags(args: &[String]) -> (Vec<String>, GcTargetOptOut) {
    let mut cleaned = Vec::with_capacity(args.len());
    let mut opt_out = GcTargetOptOut::default();
    let mut past_separator = false;
    for arg in args {
        if past_separator {
            cleaned.push(arg.clone());
            continue;
        }
        if arg == "--" {
            past_separator = true;
            cleaned.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            NO_GC_TARGET_FLAG => {
                opt_out.before = true;
                opt_out.after = true;
            }
            NO_GC_TARGET_BEFORE_FLAG => opt_out.before = true,
            NO_GC_TARGET_AFTER_FLAG => opt_out.after = true,
            _ => cleaned.push(arg.clone()),
        }
    }
    (cleaned, opt_out)
}

/// Resolve the cargo `target/` directory that an auto-prune pass should
/// operate on. Mirrors cargo's resolution order:
/// 1. `--target-dir <DIR>` inside the arg list.
/// 2. `CARGO_TARGET_DIR` env var (if non-empty).
/// 3. `<workspace_root>/target` derived from the nearest enclosing
///    `Cargo.toml` to cwd.
///
/// Returns `None` when no manifest can be found cheaply — the auto-hook
/// silently skips in that case rather than guessing.
fn resolve_target_dir_for_gc(args: &[String]) -> Option<std::path::PathBuf> {
    if let Some(value) = disk::cargo_arg_value(args, "--target-dir") {
        return Some(disk::absolutize_path(std::path::PathBuf::from(value)));
    }
    if let Some(env_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        let s = env_dir.to_string_lossy().trim().to_string();
        if !s.is_empty() {
            return Some(disk::absolutize_path(std::path::PathBuf::from(s)));
        }
    }
    let manifest = crate::trampoline::find_nearest_manifest()?;
    let manifest_dir = manifest.parent()?.to_path_buf();
    Some(manifest_dir.join("target"))
}

fn emit_auto_prune_summary(outcome: &crate::cache_lib::auto_target_gc::AutoPruneOutcome) {
    if let Some(line) = render_summary(outcome) {
        eprintln!("{line}");
    }
}

fn scrub_soldr_cache_lifecycle_env_for_child_cargo(command: &mut std::process::Command) {
    command.env_remove(SOLDR_CACHE_LIFECYCLE_ENV_VAR);
    command.env_remove(SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR);
}

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

    let cache_lifecycle = cache_lifecycle_from_env()?;
    let command_lifetime_shutdown_timeout = if cache_lifecycle == CacheLifecycle::Command {
        Some(command_lifetime_shutdown_timeout()?)
    } else {
        None
    };

    // Strip soldr-private auto target-GC opt-out flags before any other
    // arg-vector handling so downstream code (trampolines, cargo spawn)
    // never sees them. The env-var fallback is unioned in below.
    let (args_owned, gc_opt_out) = strip_no_gc_target_flags(args);
    let gc_opt_out = gc_opt_out.merged_with_env();
    let args: &[String] = &args_owned;

    // `cargo run` trampoline (issue #344). When the binary is already
    // up-to-date with the recorded sources, this exec's the binary
    // directly and never spawns cargo. Otherwise we get back a plan that
    // strips the soldr-private `--no-trampoline` flag from the arg list
    // and lets us refresh the sidecar after cargo succeeds.
    let trampoline_plan = if subcommand::is_cargo_run_invocation(args) {
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
    // These soldr control variables are consumed by this front-door
    // process. Letting cargo inherit them leaks daemon lifecycle policy
    // into build scripts and test binaries that may spawn nested soldr.
    scrub_soldr_cache_lifecycle_env_for_child_cargo(&mut command);
    // soldr cargo is the top of the invocation tree, so any inherited
    // MAKEFLAGS/CARGO_MAKEFLAGS points at jobserver fds that aren't open in
    // our process. Stripping them lets cargo start a fresh jobserver instead
    // of printing the "failed to connect to jobserver" warning (see #283).
    command.env_remove("MAKEFLAGS");
    command.env_remove("CARGO_MAKEFLAGS");
    command.env("RUSTC", &rustc);
    let build_like_cargo = cargo_args_are_cacheable(args);
    let cache_enabled_for_cargo = cache_enabled && build_like_cargo;

    // Issue #597: auto-install rustup components for `soldr cargo {fmt,
    // clippy,miri}` when they're missing. Best-effort and silent on
    // failure — cargo's own error surfaces if the auto-install fails.
    // Honors SOLDR_NO_AUTO_COMPONENT=1.
    component_install::maybe_install_component_for_subcommand(args, &paths);

    // PR 3 (#578, meta #579): cross-repo cook-index pre-flight hydrate.
    // Best-effort — every failure path is silent so a missing daemon,
    // missing Cargo.lock, mismatched sha, or extract error never
    // breaks the cargo build. Only fires for build-like cargo
    // commands; `cargo metadata` / `cargo search` / etc. don't need
    // target/ to be populated.
    if build_like_cargo {
        cook_hydrate::maybe_hydrate(args, &paths);
    }

    let cargo_profile_debug_default = if build_like_cargo {
        profile_debug::maybe_apply_cargo_profile_debug_default(&mut command, args, &paths)?
    } else {
        None
    };

    // Phase 2: per-build session correlation. Stamp every wrapper
    // invocation with a u64 session id and fire BuildSessionStart to
    // the daemon (fire-and-forget). On exit we fire BuildSessionEnd
    // so the daemon can finalize the per-crate timing aggregate.
    let session_id = generate_build_session_id();
    command.env(
        crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR,
        session_id.to_string(),
    );
    let session_started_at_ms = current_unix_ms();
    let session_repo_root =
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::daemon::client::build_session_start(
        &paths,
        session_id,
        &session_repo_root,
        session_started_at_ms,
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
    command.env(
        "PATH",
        disk::prepend_paths(&path_dirs, existing_path.as_deref())?,
    );
    let explicit_target = target::default_cargo_build_target(args)?;
    if let Some(target) = explicit_target.as_deref() {
        command.env("CARGO_BUILD_TARGET", target);
    }

    target::apply_linker_override(&mut command, args, explicit_target.as_deref(), &paths)?;

    let mut cache_plan =
        CargoCachePlan::prepare(cache_enabled_for_cargo, &paths, zccache_source).await?;
    cache_plan.apply_to_command(&mut command, explicit_target.as_deref())?;

    cache_plan.prepare_rust_artifact_plan(
        &cargo,
        &rustc,
        args,
        cargo_profile_debug_default.as_ref(),
    )?;
    if build_like_cargo {
        let probe_path = cache_plan
            .target_dir_for_hooks(args)
            .unwrap_or_else(|| disk::cargo_disk_space_probe_path(args));
        disk::maybe_emit_low_disk_warning(&probe_path);
    }
    cache_plan.restore_rust_artifacts()?;

    // Target-registry memoization for the wrapper hot path (#440).
    // Without this, every rustc invocation re-opens redb and writes
    // the same target row (~14 ms p50 on Windows in the issue #440
    // profile). The cargo front door runs once per build session and
    // already knows the target dir, so do the upsert here and
    // propagate a recorded-marker env var that lets the wrapper skip
    // its own redb work + daemon target-touch IPC.
    if build_like_cargo {
        let target_dir_for_memo: Option<std::path::PathBuf> = cache_plan.target_dir_for_hooks(args);
        if let Some(dir) = target_dir_for_memo.as_deref() {
            if dir.is_dir() {
                let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
                let db_path = crate::cache_lib::data_db_path(&paths);
                if let Ok(registry) =
                    crate::cache_lib::target_registry::TargetRegistry::open(&db_path)
                {
                    let _ = registry.upsert(&canon);
                }
                command.env(
                    crate::wrapper_target::TARGET_REGISTRY_RECORDED_ENV_VAR,
                    canon.as_os_str(),
                );
            }
        }
    }

    // Pre-compile target-GC (#485). Only on build-like cargo invocations
    // (build/check/test/run/...) and only when the user hasn't opted out
    // via --no-gc-target / --no-gc-target-before / SOLDR_NO_GC_TARGET.
    // Uses the rust_plan target_dir when available so the hook respects
    // any CARGO_TARGET_DIR / --target-dir override the same way cargo
    // and rust_plan do.
    if build_like_cargo && !gc_opt_out.before {
        let target_dir = cache_plan.target_dir_for_hooks(args);
        if let Some(dir) = target_dir.as_deref() {
            let outcome = auto_prune_target(dir, AutoPrunePhase::Before);
            emit_auto_prune_summary(&outcome);
        }
    }

    // For clippy fall-through we need to TEE cargo's stdout/stderr so the
    // user sees diagnostics live AND we have a copy to store in the
    // sidecar for the next run. For build/check we don't need the output;
    // just inherit fds via .status().
    //
    // We ALSO opt into capture when stderr is not a terminal (CI / Docker
    // / `soldr cargo build 2>file`) so the cargo_diagnostics scanner can
    // recognize the missing-host-tool failure pattern from #422 and
    // rewrap cargo's terse `failed to execute command: (os error 2)`
    // with platform-aware install hints. Interactive TTY users keep
    // `.status()` inheritance (and therefore cargo's live progress bar)
    // since changing stderr to a pipe would force cargo into its
    // non-TTY rendering mode.
    use std::io::IsTerminal;
    let needs_clippy_capture = matches!(
        workspace_plan.as_ref().map(|p| p.verb),
        Some(WorkspaceVerb::Clippy)
    );
    let capture_for_diagnostics = !needs_clippy_capture && !std::io::stderr().is_terminal();
    let (status, clippy_capture, diagnostic_capture) = if needs_clippy_capture {
        let (status, capture) = run_command_capturing_clippy(&mut command)?;
        (status, Some(capture), None)
    } else if capture_for_diagnostics {
        let (status, captured) = run_command_capturing_diagnostic_tail(&mut command)?;
        (status, None, Some(captured))
    } else {
        (command.status()?, None, None)
    };
    // Extract whatever stderr text we captured BEFORE the success
    // branch moves `clippy_capture` into `refresh_workspace_sidecar_after_cargo`.
    // Used below in the !status.success() block by cargo_diagnostics.
    let captured_stderr_for_diagnosis: Option<String> =
        if let Some(capture) = clippy_capture.as_ref() {
            Some(String::from_utf8_lossy(&capture.stderr).into_owned())
        } else {
            diagnostic_capture
        };

    // Phase 2: fire BuildSessionEnd before the success/failure
    // branches do any further work. Best-effort — never affects the
    // build's own outcome.
    crate::daemon::client::build_session_end(
        &paths,
        session_id,
        status.code().unwrap_or(-1),
        current_unix_ms(),
    );

    let post_cargo_result: Result<(), SoldrError> = (|| {
        if status.success() {
            cache_plan.save_rust_artifacts()?;
            // Post-compile target-GC (#485). Same gating as the pre-pass —
            // build-like cargo, no opt-out, resolve dir consistently with the
            // pre-pass. The active-cargo-lock guard inside `auto_prune_target`
            // is what keeps a parallel `cargo` in the same `target/` from
            // racing this pass; we never emit a stderr line when that guard
            // engages.
            if build_like_cargo && !gc_opt_out.after {
                let target_dir = cache_plan.target_dir_for_hooks(args);
                if let Some(dir) = target_dir.as_deref() {
                    let outcome = auto_prune_target(dir, AutoPrunePhase::After);
                    emit_auto_prune_summary(&outcome);
                }
            }
            if let Some(plan) = trampoline_plan.as_ref() {
                refresh_sidecar_after_cargo(plan);
            }
            if let Some(plan) = workspace_plan.as_ref() {
                refresh_workspace_sidecar_after_cargo(plan, clippy_capture);
            }
        } else {
            // A non-zero cargo exit can leave orphan `.rmeta` files (rmeta
            // emitted, then rustc aborted before the `.rlib` codegen pass)
            // in `target/<triple>/<profile>/deps/`. Subsequent invocations
            // then fail with `E0463: can't find crate` because cargo passes
            // `--extern X=orphan.rmeta` to dependents and rustc cannot link
            // an rmeta-only crate. Sweep them so the next build rebuilds
            // cleanly. See soldr#410.
            cache_plan.prune_orphan_rmetas_after_failed_build();
        }
        Ok(())
    })();

    // After cargo fails, look at whatever stderr we captured for a
    // recognizable build-script-spawn-ENOENT pattern (#422 — minimal
    // Rust containers without a host C toolchain). The capture
    // sources are `clippy_capture.stderr` for clippy runs and the
    // dedicated diagnostic-tail buffer otherwise. TTY users captured
    // nothing — they see cargo's own error untouched and skip this
    // path.
    if !status.success() {
        if let Some(stderr_text) = captured_stderr_for_diagnosis.as_deref() {
            if let Some(diag) = crate::cargo_diagnostics::detect_build_script_failure(stderr_text) {
                let rendered = crate::cargo_diagnostics::render_diagnosis(&diag);
                let stderr = std::io::stderr();
                let _ = stderr.lock().write_all(rendered.as_bytes());
            }
        }
    }

    cache_plan.finish_zccache_session(command_lifetime_shutdown_timeout)?;
    post_cargo_result?;
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

/// Run cargo with both streams tee'd to the user's stdout/stderr AND
/// stderr accumulated into a [`String`] for post-failure scanning by
/// [`crate::cargo_diagnostics`]. Stdout is NOT buffered — we only need
/// stderr for diagnosis, and cargo can emit megabytes of compile
/// progress to stdout that would just sit unused in RAM.
///
/// Used in the non-clippy, non-TTY branch of `run_cargo_front_door`
/// (#422): when stderr is piped to a CI log / Docker stream / file,
/// cargo's progress-bar UX is already gone, so the extra
/// pipe-and-tee doesn't degrade interactive output.
fn run_command_capturing_diagnostic_tail(
    command: &mut std::process::Command,
) -> Result<(std::process::ExitStatus, String), SoldrError> {
    use std::io::Read;

    command.stderr(std::process::Stdio::piped());
    // stdout stays inherited — we don't need its bytes.
    let mut child = command.spawn().map_err(|err| {
        SoldrError::Other(format!("spawn cargo for diagnostic capture failed: {err}"))
    })?;
    let mut child_stderr = child.stderr.take().expect("piped");

    let stderr_handle = std::thread::spawn(move || -> Vec<u8> {
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

    let status = child.wait().map_err(|err| {
        SoldrError::Other(format!("wait on cargo diagnostic capture failed: {err}"))
    })?;
    let bytes = stderr_handle.join().unwrap_or_default();
    let captured = String::from_utf8_lossy(&bytes).into_owned();
    Ok((status, captured))
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

async fn ensure_known_subcommand_tool(
    args: &[String],
    paths: &SoldrPaths,
) -> Result<Vec<std::path::PathBuf>, SoldrError> {
    let Some(sub) = first_cargo_subcommand(args) else {
        return Ok(Vec::new());
    };
    let Some(spec) = crate::fetch::lookup_by_cargo_subcommand(sub) else {
        // Issue #412: when the typed subcommand isn't in
        // `known_tools` but LOOKS like a typo of one that IS, drop a
        // "did you mean?" hint on stderr. We still return Ok(empty)
        // so the underlying cargo invocation continues as today —
        // the suggestion is advisory and cargo's own external-command
        // dispatch may still find the tool on PATH.
        let known = crate::fetch::known_cargo_subcommands();
        if let Some(suggestion) = crate::fuzzy_match::suggest_close_match(sub, &known) {
            eprintln!("soldr: '{sub}' is not a cargo subcommand soldr ships a prebuilt for.");
            eprintln!("soldr: did you mean: cargo {suggestion}?");
        }
        return Ok(Vec::new());
    };

    let version = spec
        .pinned_version
        .map(|v| VersionSpec::Exact(v.to_string()))
        .unwrap_or(VersionSpec::Latest);

    eprintln!("soldr: fetching {}...", spec.crate_name);
    let result = crate::fetch::fetch_tool_with_paths(spec.crate_name, &version, paths).await?;

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
mod tests;
