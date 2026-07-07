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
use crate::{
    apply_implicit_toolchain_homes, gc, resolve_toolchain_binary_for_channel, ZccacheSourceArg,
};
use std::ffi::OsString;
use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wait_timeout::ChildExt;

mod cache_plan;
mod clang_cl_shim;
mod component_install;
pub(crate) mod cook_hydrate;
mod disk;
mod inputs;
mod profile_debug;
mod subcommand;
mod target;
mod zig_shim;

use cache_plan::CargoCachePlan;

const CARGO_WAIT_TIMEOUT_ENV_VAR: &str = "SOLDR_CARGO_WAIT_TIMEOUT_SECS";
const DEFAULT_CARGO_WAIT_TIMEOUT_SECS: u64 = 30 * 60;
const KILLED_CARGO_REAP_TIMEOUT_SECS: u64 = 5;

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

fn cargo_wait_timeout() -> Duration {
    std::env::var(CARGO_WAIT_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_CARGO_WAIT_TIMEOUT_SECS))
}

fn wait_for_cargo_child(
    child: &mut std::process::Child,
    context: &str,
) -> Result<std::process::ExitStatus, SoldrError> {
    let timeout = cargo_wait_timeout();
    match child
        .wait_timeout(timeout)
        .map_err(|err| SoldrError::Other(format!("wait on {context} failed: {err}")))?
    {
        Some(status) => Ok(status),
        None => {
            let kill_result = child.kill();
            let reap_result =
                child.wait_timeout(Duration::from_secs(KILLED_CARGO_REAP_TIMEOUT_SECS));
            let timeout_secs = timeout.as_secs();
            let mut message = format!(
                "{context} timed out after {timeout_secs} seconds \
                 (set {CARGO_WAIT_TIMEOUT_ENV_VAR} to override)"
            );
            match kill_result {
                Ok(()) => message.push_str("; killed child process"),
                Err(err) => message.push_str(&format!("; kill failed: {err}")),
            }
            match reap_result {
                Ok(Some(_)) => {}
                Ok(None) => message.push_str(&format!(
                    "; process did not exit within {KILLED_CARGO_REAP_TIMEOUT_SECS} seconds after kill"
                )),
                Err(err) => message.push_str(&format!("; reap after kill failed: {err}")),
            }
            Err(SoldrError::Other(message))
        }
    }
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

const INHERITED_SOLDR_WORKSPACE_ENV_VARS: &[&str] = &[
    crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR,
    crate::cache_lib::MANAGED_ZCCACHE_CACHE_DIR_ENV_VAR,
    crate::cache_lib::ZCCACHE_SESSION_ID_ENV_VAR,
    crate::wrapper_target::TARGET_REGISTRY_RECORDED_ENV_VAR,
    crate::TARGET_CACHE_MODE_ENV_VAR,
    "SOLDR_TARGET_CACHE_DIR",
    crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR,
    crate::TARGET_CACHE_PROFILE_ENV_VAR,
    crate::TARGET_CACHE_BACKEND_ENV_VAR,
    crate::TARGET_CACHE_TAR_THREADS_ENV_VAR,
    "SOLDR_TARGET_CACHE_COMPRESS",
    "SOLDR_TARGET_CACHE_COMPRESS_LEVEL",
    "SOLDR_BUILD_CACHE_MODE",
];

struct EnvRestore {
    key: OsString,
    previous: Option<OsString>,
}

struct FreshSoldrWorkspaceEnvGuard {
    entries: Vec<EnvRestore>,
}

impl FreshSoldrWorkspaceEnvGuard {
    fn apply_unless_trusted(trust_inherited_soldr_env: bool) -> Self {
        if trust_inherited_soldr_env {
            return Self {
                entries: Vec::new(),
            };
        }

        let mut keys: Vec<OsString> = INHERITED_SOLDR_WORKSPACE_ENV_VARS
            .iter()
            .map(OsString::from)
            .collect();
        keys.extend(
            std::env::vars_os()
                .map(|(key, _)| key)
                .filter(|key| key.to_string_lossy().starts_with("SETUP_SOLDR_")),
        );
        keys.sort();
        keys.dedup();

        let mut entries = Vec::new();
        for key in keys {
            let previous = std::env::var_os(&key);
            if previous.is_some() {
                std::env::remove_var(&key);
                entries.push(EnvRestore { key, previous });
            }
        }
        Self { entries }
    }
}

impl Drop for FreshSoldrWorkspaceEnvGuard {
    fn drop(&mut self) {
        for entry in self.entries.iter().rev() {
            if let Some(value) = &entry.previous {
                std::env::set_var(&entry.key, value);
            }
        }
    }
}

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

fn env_flag_truthy(key: &str) -> bool {
    std::env::var_os(key)
        .map(|v| {
            let s = v.to_string_lossy();
            let t = s.trim();
            !t.is_empty() && !t.eq_ignore_ascii_case("0") && !t.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

/// Issue #1364: a truthy `ZCCACHE_DISABLE` in the caller's environment is
/// treated as `--no-cache`.
///
/// `ZCCACHE_DISABLE` is the standard zccache kill-switch, but soldr never
/// consulted it, so users who set it saw no effect (the build still went
/// through the wrapper/daemon). Mapping it onto the existing `--no-cache`
/// path fully bypasses the wrapper + daemon (and propagates
/// `SOLDR_CACHE_ENABLED=0` to the child cargo), which is also the
/// recovery path when a build hangs on a wedged cache.
pub(crate) fn zccache_disable_requested() -> bool {
    env_flag_truthy("ZCCACHE_DISABLE")
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

fn scrub_inherited_soldr_workspace_env_for_child_cargo(command: &mut std::process::Command) {
    for key in INHERITED_SOLDR_WORKSPACE_ENV_VARS {
        command.env_remove(key);
    }
    for key in std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| key.to_string_lossy().starts_with("SETUP_SOLDR_"))
    {
        command.env_remove(key);
    }
}

fn maybe_apply_rustfmt_zccache_shim(
    command: &mut std::process::Command,
    args: &[String],
    cache_enabled: bool,
) -> Option<crate::shim_dir::ShimDirGuard> {
    if !cache_enabled
        || first_cargo_subcommand(args) != Some("fmt")
        || std::env::var_os("RUSTFMT").is_some()
    {
        return None;
    }

    match crate::shim_dir::build_shim_dir() {
        Ok(guard) => {
            command.env(
                "RUSTFMT",
                crate::shim_dir::shim_tool_path(&guard.path, "rustfmt"),
            );
            command.env(crate::shim_dir::SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR, "1");
            Some(guard)
        }
        Err(err) => {
            eprintln!(
                "soldr warning: failed to build rustfmt shim for cargo fmt; rustfmt will run without zccache format caching: {err}"
            );
            None
        }
    }
}

pub(crate) async fn run_cargo_front_door(
    args: &[String],
    cache_enabled: bool,
    zccache_source: ZccacheSourceArg,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    if cargo_args_use_reserved_no_cache(args) {
        return Err(SoldrError::Other(
            "`--no-cache` must appear before `cargo`, as in `soldr --no-cache cargo build`".into(),
        ));
    }

    let trust_inherited_soldr_env =
        trust_inherited_soldr_env || env_flag_truthy(crate::TRUST_INHERITED_SOLDR_ENV_VAR);
    let _fresh_workspace_env =
        FreshSoldrWorkspaceEnvGuard::apply_unless_trusted(trust_inherited_soldr_env);

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
    let (args_owned, explicit_toolchain) = subcommand::strip_cargo_toolchain_directive(&args_owned);
    let explicit_toolchain = explicit_toolchain.as_deref();
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

    let cargo = resolve_toolchain_binary_for_channel("cargo", explicit_toolchain)?;
    let rustc = resolve_toolchain_binary_for_channel("rustc", explicit_toolchain)?;
    let cargo_bin_dir = cargo
        .parent()
        .ok_or_else(|| SoldrError::Other("failed to resolve cargo bin directory".into()))?
        .to_path_buf();
    let existing_path = std::env::var_os("PATH");
    let paths = SoldrPaths::new()?;
    paths.ensure_dirs()?;

    // L3 (soldr#980): kick off the managed zccache binary fetch +
    // extract + redb init on a background tokio task NOW. The rest of
    // this front-door pipeline — known-subcommand fetch, env scrub,
    // session-id stamp, target-registry memoization, pre-GC, low-disk
    // probe, profile_debug detection, linker injection — does not
    // depend on the resolved zccache path. Overlapping its wall-clock
    // cost with that synchronous setup is worth ~1-2 s on cold builds
    // where the binary is not already on disk. On warm builds the
    // background future resolves effectively immediately so the join
    // at `CargoCachePlan::finalize` is free.
    //
    // We *intentionally* spawn after both trampoline branches above
    // because those paths exit without spawning cargo, and we don't
    // want to start a fetch we'll just drop. `cache_enabled` here is
    // the same flag the original synchronous `CargoCachePlan::prepare`
    // gated on; passing `false` produces a no-op `Disabled` prefetch.
    let cache_plan_prefetch =
        cache_plan::CargoCachePlanPrefetch::start(cache_enabled, &paths, zccache_source);

    // If the user invoked a known ecosystem subcommand (e.g. `cargo nextest`),
    // fetch the corresponding `cargo-<sub>` binary and prepend its directory to
    // PATH so cargo's subcommand dispatch finds it. Also collect transitive
    // bootstrap env (e.g. SDKROOT for explicit legacy
    // `cargo zigbuild --target *-apple-darwin`).
    let subcommand_tool_bootstrap = ensure_known_subcommand_tool(args, &paths).await?;
    let extra_bin_dirs = subcommand_tool_bootstrap.bin_dirs;
    let transitive_env_overrides = subcommand_tool_bootstrap.env;
    // Compute env-var overrides keyed off the subcommand + its
    // --target argument. Today this fixes ring's build.rs on
    // `cargo xwin build --target *-pc-windows-msvc` by routing cc-rs
    // to `clang-cl` instead of the GNU-flavoured `clang`. See
    // `compute_subcommand_env_overrides` for the full rule set.
    let subcommand_env_overrides = compute_subcommand_env_overrides(args);

    let mut command = std::process::Command::new(&cargo);
    command.args(args);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    // These soldr control variables are consumed by this front-door
    // process. Letting cargo inherit them leaks daemon lifecycle policy
    // into build scripts and test binaries that may spawn nested soldr.
    scrub_soldr_cache_lifecycle_env_for_child_cargo(&mut command);
    if !trust_inherited_soldr_env {
        scrub_inherited_soldr_workspace_env_for_child_cargo(&mut command);
    }
    // soldr cargo is the top of the invocation tree, so any inherited
    // MAKEFLAGS/CARGO_MAKEFLAGS points at jobserver fds that aren't open in
    // our process. Stripping them lets cargo start a fresh jobserver instead
    // of printing the "failed to connect to jobserver" warning (see #283).
    command.env_remove("MAKEFLAGS");
    command.env_remove("CARGO_MAKEFLAGS");
    command.env("RUSTC", &rustc);

    // Issue #836 (sub of #835): pin the rust toolchain explicitly via
    // RUSTUP_TOOLCHAIN so rustup does NOT consult `rust-toolchain.toml`
    // on the cargo side and try to install the manifest's declared
    // `components = [...]` automatically.
    //
    // Why this matters in CI: many runner images (the GitHub-hosted
    // ubuntu-* lineage especially) ship a pre-existing `bin/cargo-fmt`
    // that conflicts with rustup's `rustfmt-preview` component install,
    // producing the well-known
    //
    //     error: failed to install component:
    //       'rustfmt-preview-x86_64-unknown-linux-gnu',
    //       detected conflict: 'bin/cargo-fmt'
    //
    // which kills the build before cargo even starts compiling. The
    // soldr bootstrap is supposed to short-circuit this — soldr itself
    // already knows the manifest channel (via
    // `read_rust_toolchain_manifest`), so passing it explicitly to
    // rustup with `RUSTUP_TOOLCHAIN` skips the manifest read on the
    // child cargo, and with it the entire auto-component-install path.
    //
    // Honor an explicit caller-set RUSTUP_TOOLCHAIN (don't clobber).
    // For users who genuinely need rustfmt / clippy at build time,
    // `soldr cargo fmt` / `clippy` still self-install via
    // `component_install::maybe_install_component_for_subcommand`.
    if let Some(toolchain) = explicit_toolchain {
        command.env("RUSTUP_TOOLCHAIN", toolchain);
    } else if std::env::var_os("RUSTUP_TOOLCHAIN").is_none() {
        let manifest_dir =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        if let Ok(manifest) = crate::core::read_rust_toolchain_manifest(&manifest_dir) {
            if let Some(channel) = manifest.channel {
                let channel = channel.trim();
                if !channel.is_empty() {
                    command.env("RUSTUP_TOOLCHAIN", channel);
                }
            }
        }
    }

    // Apply subcommand-derived env overrides (e.g. CC_<triple>=clang-cl
    // for `cargo xwin build --target *-pc-windows-msvc`). Honor a
    // caller-set value — don't clobber if the user already exported
    // their own CC / CXX / AR.
    for (key, value) in &subcommand_env_overrides {
        if std::env::var_os(key).is_none() {
            command.env(key, value);
        }
    }
    // Apply transitive-bootstrap env overrides (e.g. SDKROOT for explicit
    // legacy `cargo zigbuild --target *-apple-darwin`). These come from
    // `ensure_known_subcommand_tool` which calls into ensure_apple_sdk
    // / ensure_zig / etc. The functions themselves already gate on
    // `var_os` being unset before pushing, so just apply them.
    for (key, value) in &transitive_env_overrides {
        command.env(key, value);
    }

    let build_like_cargo = cargo_args_are_cacheable(args);
    // Issue #824 follow-up: always engage RUSTC_WRAPPER + the zccache
    // session when caching is enabled, regardless of whether the cargo
    // subcommand is in our known-compiling set. The previous policy
    // (`cache_enabled && build_like_cargo`) silently dropped rustc
    // observations whenever soldr's classifier said "this subcommand
    // doesn't compile" — but build scripts, third-party cargo subcommand
    // plugins not yet in `known_tools`, and even some normally-non-
    // compiling verbs *can* re-shell to rustc through paths we don't
    // model. We always want zccache to see the call, then have zccache
    // itself decide whether to cache or pass through (its "non-cacheable"
    // classifier already handles read-only / non-hashable inputs).
    //
    // The trade-off is a small session-start/stop overhead (~hundreds of
    // ms) for cargo subcommands that don't end up spawning rustc — but
    // the observability win is "no rustc call goes unrecorded". The other
    // hooks (cook hydrate, disk watchdog, target-registry memo) still
    // gate on `build_like_cargo` because those have nothing to do with
    // rustc wrapping — they care about whether `target/` will be touched.
    let cache_enabled_for_cargo = cache_enabled;

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
    // Issue #980 L7: gate the in-process auto-GC orchestrator (and any
    // other long-running workers) so they sleep while this cargo
    // invocation is running. The flag is cleared below right after
    // `build_session_end` so the next throttle tick can resume normal
    // operation. The daemon process maintains its own copy via the
    // matching IPC dispatch in `daemon/server.rs`.
    crate::cache_lib::build_active::set(true);
    if build_like_cargo {
        // Cargo front door only: keep startup/low-disk warnings off unrelated
        // commands and out of the rustc-wrapper hot path.
        gc::emit_startup_target_warning_if_due();
        // Issue #1286 (F5): the auto-GC trigger moved to build END (see
        // `gc::maybe_spawn_auto_gc_sweeper` below). Kicking here — right
        // after `build_active::set(true)` — always deferred with
        // `reason=build_active`, so the sweep never ran on machines
        // that only invoke soldr for builds.
    }
    let mut path_dirs: Vec<std::path::PathBuf> = Vec::with_capacity(1 + extra_bin_dirs.len());
    path_dirs.push(cargo_bin_dir);
    path_dirs.extend(extra_bin_dirs);
    command.env(
        "PATH",
        disk::prepend_paths(&path_dirs, existing_path.as_deref())?,
    );
    let _rustfmt_shim_guard =
        maybe_apply_rustfmt_zccache_shim(&mut command, args, cache_enabled_for_cargo);
    let explicit_target = target::default_cargo_build_target(args)?;
    if let Some(target) = explicit_target.as_deref() {
        command.env("CARGO_BUILD_TARGET", target);
    }
    let native_cache_target = target::known_cargo_build_target(args, explicit_target.as_deref())
        .filter(|target| target.ends_with("-apple-darwin"));

    target::apply_linker_override(&mut command, args, explicit_target.as_deref(), &paths)?;

    // L3 (soldr#980): await the background zccache prefetch we kicked
    // off near the top of this function. Up until this point the cargo
    // command has been built without any wrapper env, so the prefetch
    // has been overlapping the entire setup pipeline. On a cold build
    // (binary not yet on disk) this is where the ~1-2 s saving falls
    // out — on a warm build the await is a near-no-op.
    //
    // Note: `cache_enabled_for_cargo` is currently `cache_enabled` (see
    // the comment above its assignment for the #824 follow-up
    // rationale). We thread it through `finalize` for symmetry with the
    // old synchronous API so that future divergence between the two
    // flags doesn't silently rewire the prefetch decision.
    let mut cache_plan =
        CargoCachePlan::finalize(cache_enabled_for_cargo, cache_plan_prefetch).await?;
    cache_plan.apply_to_command(&mut command, native_cache_target.as_deref())?;

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
        // Issue #574: host-volume disk watchdog. Distinct from the
        // legacy 2 GiB advisory above — this layer warns at 10 GiB and
        // aborts at 5 GiB so cross-repo target/ bloat surfaces before
        // the build sets the disk on fire. Returning Err here lets the
        // top-level dispatch print the error and exit with a non-zero
        // code (same path as any other SoldrError from the front door).
        let watchdog_path = cache_plan
            .target_dir_for_hooks(args)
            .unwrap_or_else(|| disk::cargo_disk_space_probe_path(args));
        match gc::disk::check_disk_or_warn_or_block(&watchdog_path) {
            gc::disk::DiskCheckOutcome::Disabled | gc::disk::DiskCheckOutcome::Ok { .. } => {}
            gc::disk::DiskCheckOutcome::Warn {
                free_bytes,
                threshold_gib,
            } => {
                eprintln!("{}", gc::disk::render_warn_line(free_bytes, threshold_gib));
            }
            gc::disk::DiskCheckOutcome::Block {
                free_bytes,
                threshold_gib,
            } => {
                return Err(SoldrError::Other(gc::disk::render_block_message(
                    free_bytes,
                    threshold_gib,
                )));
            }
        }
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
    // just inherit fds while still enforcing the cargo wait timeout.
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
    // soldr#1368 observability restore: snapshot the embedded zccache
    // compile counters just before cargo runs so `finish_zccache_session`
    // can diff start-vs-end into the per-build hit/miss summary written to
    // `last-session-stats.json`.
    if let Some(session) = cache_plan.zccache_session() {
        crate::cache::capture_build_baseline(&session.cache_dir, &session.session_id);
    }
    let (status, clippy_capture, diagnostic_capture) = if needs_clippy_capture {
        let (status, capture) = run_command_capturing_clippy(&mut command)?;
        (status, Some(capture), None)
    } else if capture_for_diagnostics {
        let (status, captured) = run_command_capturing_diagnostic_tail(&mut command)?;
        (status, None, Some(captured))
    } else {
        (run_command_inheriting_stdio(&mut command)?, None, None)
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
    // Issue #980 L7: paired with the `set(true)` above. Clearing here
    // (before `post_cargo_result`) lets the post-build target-GC pass
    // run normally without thinking it's still inside the build.
    crate::cache_lib::build_active::set(false);
    // Issue #1286 (F5): the build just ended — this is the idle
    // transition, so fire the auto-GC sweep now, as a detached process
    // that survives this wrapper's imminent exit. Throttled to once
    // per 5-minute window by the marker inside.
    if build_like_cargo {
        gc::maybe_spawn_auto_gc_sweeper(&paths);
    }

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

    let status = wait_for_cargo_child(&mut child, "cargo clippy")?;
    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();
    let capture = RawClippyCapture {
        stdout,
        stderr,
        exit_code: status.code().unwrap_or(1),
    };
    Ok((status, capture))
}

fn run_command_inheriting_stdio(
    command: &mut std::process::Command,
) -> Result<std::process::ExitStatus, SoldrError> {
    let mut child = command
        .spawn()
        .map_err(|err| SoldrError::Other(format!("spawn cargo failed: {err}")))?;
    wait_for_cargo_child(&mut child, "cargo")
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

    let status = wait_for_cargo_child(&mut child, "cargo diagnostic capture")?;
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

/// Decide whether the "did you mean: cargo X?" hint applies to a typed
/// subcommand that isn't in `known_tools`. Returns `Some(suggestion)`
/// only when `sub` looks like a typo of a registered cargo subcommand
/// AND is not itself a legitimate cargo built-in verb.
///
/// Issue #755: without the built-in guard, `soldr cargo check` falsely
/// suggested `cargo chef` (Levenshtein distance 2). Built-in verbs are
/// routed through the External arm by `cargo` itself; treating them as
/// typos contradicts the contract documented in `CARGO_BUILTIN_VERBS`.
fn suggest_cargo_subcommand_typo(sub: &str) -> Option<String> {
    if crate::cli_args::is_cargo_builtin_verb(sub) {
        return None;
    }
    let known = crate::fetch::known_cargo_subcommands();
    crate::fuzzy_match::suggest_close_match(sub, &known).map(|s| s.to_string())
}

/// Env var name for the PATH-first override (issue #816). Reads as truthy
/// when set to any value except an empty string or `0`/`false`/`no`.
pub(crate) const FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR: &str =
    "SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS";

fn force_managed_cargo_subcommands() -> bool {
    match std::env::var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR) {
        Ok(value) => {
            let trimmed = value.trim();
            !matches!(trimmed, "" | "0" | "false" | "no" | "off")
        }
        Err(_) => false,
    }
}

/// Walk `$PATH` looking for an executable named `tool`. Mirrors the
/// hand-rolled lookup in `core::toolchain_resolve::path_bin_dir` —
/// duplicated rather than re-exported to keep the cargo-front-door
/// independent of `core::toolchain_resolve`'s internals. On Windows the
/// `PATHEXT` suffix sweep matches what the toolchain resolver does so
/// `cargo-zigbuild.exe` is found even when the caller typed `cargo-zigbuild`.
fn find_on_path(tool: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(tool);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            if std::path::Path::new(tool).extension().is_some() {
                continue;
            }
            let pathext = std::env::var_os("PATHEXT")
                .and_then(|value| value.into_string().ok())
                .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
            for suffix in pathext.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                let suffixed = dir.join(format!("{tool}{suffix}"));
                if suffixed.is_file() {
                    return Some(suffixed);
                }
            }
        }
    }
    None
}

/// Result of subcommand tool resolution: PATH-prepended bin dirs +
/// env-var overrides for the child cargo invocation.
pub(crate) struct SubcommandToolBootstrap {
    pub bin_dirs: Vec<std::path::PathBuf>,
    pub env: Vec<(String, String)>,
}

async fn ensure_known_subcommand_tool(
    args: &[String],
    paths: &SoldrPaths,
) -> Result<SubcommandToolBootstrap, SoldrError> {
    let Some(sub) = first_cargo_subcommand(args) else {
        return Ok(SubcommandToolBootstrap {
            bin_dirs: Vec::new(),
            env: Vec::new(),
        });
    };
    let Some(spec) = crate::fetch::lookup_by_cargo_subcommand(sub) else {
        // Issue #412: when the typed subcommand isn't in
        // `known_tools` but LOOKS like a typo of one that IS, drop a
        // "did you mean?" hint on stderr. We still return empty so the
        // underlying cargo invocation continues as today — the
        // suggestion is advisory and cargo's own external-command
        // dispatch may still find the tool on PATH.
        if let Some(suggestion) = suggest_cargo_subcommand_typo(sub) {
            eprintln!("soldr: '{sub}' is not a cargo subcommand soldr ships a prebuilt for.");
            eprintln!("soldr: did you mean: cargo {suggestion}?");
        }
        return Ok(SubcommandToolBootstrap {
            bin_dirs: Vec::new(),
            env: Vec::new(),
        });
    };

    // Issue #816: if `cargo-<sub>` is already on PATH, defer to it instead
    // of running the managed fetch. This matches the discipline
    // `ensure_rustup_available` uses for rustup and avoids two failure modes:
    //   1. The managed fetcher writing an unrunnable artifact (the original
    //      #816 / #810 cargo-zigbuild bug, now fixed by xz2 extraction —
    //      but PATH-first is a structural belt-and-suspenders).
    //   2. Bypassing a user who deliberately installed a specific version
    //      via `cargo install <name> --locked` or their distro package
    //      manager. cargo's own external-subcommand dispatch will find the
    //      PATH binary; soldr returning Ok(empty) here leaves that path
    //      open without prepending its own bin dir.
    // Escape hatch: SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS=1 forces the
    // managed fetch even when PATH has the tool — useful for CI runs that
    // want byte-identical pinned binaries.
    let mut extra_bin_dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut extra_env: Vec<(String, String)> = Vec::new();

    if !force_managed_cargo_subcommands() {
        let exe_name = format!("cargo-{sub}");
        if let Some(path) = find_on_path(&exe_name) {
            eprintln!(
                "soldr: deferring to {exe_name} on PATH at {} (set SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS=1 to override)",
                path.display()
            );
            // Even when cargo-zigbuild is provided by the host, it
            // still shells out to `zig`. Run the transitive bootstrap
            // before returning so the deferred-on-PATH branch doesn't
            // silently regress.
            append_subcommand_transitive_bin_dirs(
                sub,
                args,
                paths,
                &mut extra_bin_dirs,
                &mut extra_env,
            )
            .await?;
            return Ok(SubcommandToolBootstrap {
                bin_dirs: extra_bin_dirs,
                env: extra_env,
            });
        }
    }

    let version = spec
        .pinned_version
        .map(|v| VersionSpec::Exact(v.to_string()))
        .unwrap_or(VersionSpec::Latest);

    eprintln!("soldr: fetching {}...", spec.crate_name);
    let result =
        crate::fetch::fetch_tool_for_host_with_paths(spec.crate_name, &version, paths).await?;

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
    extra_bin_dirs.push(dir);
    append_subcommand_transitive_bin_dirs(sub, args, paths, &mut extra_bin_dirs, &mut extra_env)
        .await?;
    Ok(SubcommandToolBootstrap {
        bin_dirs: extra_bin_dirs,
        env: extra_env,
    })
}

/// Resolve transitive runtime dependencies for `cargo-<sub>` and append
/// their bin directories to `extra_bin_dirs` (PATH-prepended on the
/// child cargo) and any required env overrides to `extra_env`.
///
/// Registered bootstraps:
///   - `cargo zigbuild` → ensure `zig` is on PATH (PR #841).
///   - explicit legacy `cargo zigbuild --target *-apple-darwin` → ensure
///     Apple SDK on disk + set `SDKROOT` env (issue #854).
///   - `cargo xwin build --target *-pc-windows-msvc` → ensure `clang`
///     shim on PATH that forces `--driver-mode=cl` (PR #849).
async fn append_subcommand_transitive_bin_dirs(
    sub: &str,
    args: &[String],
    paths: &SoldrPaths,
    extra_bin_dirs: &mut Vec<std::path::PathBuf>,
    extra_env: &mut Vec<(String, String)>,
) -> Result<(), SoldrError> {
    if sub == "zigbuild" {
        let zig_dir = crate::fetch::ensure_zig(paths).await?;
        extra_bin_dirs.push(zig_dir.clone());
        // Explicit legacy `soldr cargo zigbuild --target *-apple-darwin`
        // needs the Apple SDK on disk + `SDKROOT` exported so cargo-zigbuild's
        // mach-O linker can resolve `-framework IOKit` / etc.
        // Without this, every Rust dep with an Apple-framework
        // dependency (ring, sysinfo, dirs, …) fails to link.
        if let Some(triple) = extract_target_arg(args) {
            append_zigbuild_env_overrides(paths, triple, extra_env)?;
            if triple.ends_with("-apple-darwin") {
                let sdk_dir = crate::fetch::ensure_apple_sdk(paths, Some(triple)).await?;
                // Don't clobber a caller-set SDKROOT — escape hatch
                // for users with their own Xcode SDK or a custom one.
                if std::env::var_os("SDKROOT").is_none() {
                    extra_env.push((
                        "SDKROOT".to_string(),
                        sdk_dir.to_string_lossy().into_owned(),
                    ));
                }
                if std::env::var_os("PKG_CONFIG_SYSROOT_DIR").is_none() {
                    extra_env.push((
                        "PKG_CONFIG_SYSROOT_DIR".to_string(),
                        sdk_dir.to_string_lossy().into_owned(),
                    ));
                }
            }
        }
    }
    // `soldr cargo xwin build --target *-pc-windows-msvc` needs a
    // `clang` shim that forces `--driver-mode=cl`. ring's build.rs
    // hard-codes `c.compiler("clang")` for the aarch64 target which
    // overrides cc-rs's env-driven compiler choice (so just setting
    // CC_<triple>=clang-cl doesn't help). Putting our shim on PATH
    // wins ring's `clang` PATH lookup. See `clang_cl_shim` for the
    // full rationale.
    //
    // In addition, the same lane needs a real LLVM toolchain on PATH
    // — clang-cl / lld-link / llvm-lib — for cargo-xwin's link step
    // to succeed on a stock Linux runner that doesn't have `apt install
    // llvm clang lld` baked in. soldr fetches the toolchain from
    // `zackees/clang-tool-chain-bins` (closes #855, sub of meta #853)
    // and prepends its `bin/` to PATH alongside the clang shim. Env
    // overrides for CC_<triple> / CXX_<triple> / AR_<triple> / LD_<triple>
    // are set to absolute paths inside the fetched bin dir so cc-rs
    // and rustc can find the right driver even if PATH ordering shifts.
    //
    // On hosts not in the managed LLVM matrix (today: macOS), we log
    // and skip — those hosts ship Apple's `clang` via Xcode, and the
    // xwin lane is not a primary mac flow. Workflow-side YAML hedges
    // (apt-installed llvm/clang/lld) remain in place; sub-issue #857
    // removes them once this auto-bootstrap proves out across the
    // matrix.
    if sub == "xwin" {
        if let Some(triple) = extract_target_arg(args) {
            if triple.ends_with("-pc-windows-msvc") {
                match crate::fetch::ensure_llvm_toolchain(paths).await {
                    Ok(llvm_bin_dir) => {
                        let ext = std::env::consts::EXE_SUFFIX;
                        let clang = llvm_bin_dir.join(format!("clang{ext}"));
                        let clang_cl = llvm_bin_dir.join(format!("clang-cl{ext}"));
                        let llvm_lib = llvm_bin_dir.join(format!("llvm-lib{ext}"));
                        let lld_link = llvm_bin_dir.join(format!("lld-link{ext}"));
                        let shim_dir =
                            clang_cl_shim::ensure_clang_cl_shim_for_real_clang(paths, &clang)?;
                        extra_bin_dirs.push(shim_dir);
                        let suffix = triple.replace('-', "_");
                        // Don't clobber caller-set values — escape hatch
                        // for users who pinned their own LLVM build.
                        // Note: `compute_subcommand_env_overrides` sets
                        // bare names (`clang-cl` / `llvm-lib`) for the
                        // same triple; the absolute paths we set here
                        // win because they're pushed into `extra_env`
                        // and applied AFTER the env loop checks
                        // `var_os().is_none()` (transitive env applies
                        // unconditionally — see the apply loop below).
                        // To avoid that double-set racing, gate on
                        // `var_os` here too: the bare-name fallback
                        // still hits PATH which now contains the
                        // LLVM bin dir.
                        for (key, val) in [
                            (format!("CC_{suffix}"), &clang_cl),
                            (format!("CXX_{suffix}"), &clang_cl),
                            (format!("AR_{suffix}"), &llvm_lib),
                            (format!("LD_{suffix}"), &lld_link),
                        ] {
                            if std::env::var_os(&key).is_none() {
                                extra_env.push((key, val.to_string_lossy().into_owned()));
                            }
                        }
                        extra_bin_dirs.push(llvm_bin_dir);
                    }
                    Err(SoldrError::UnsupportedPlatform(msg)) => {
                        let shim_dir = clang_cl_shim::ensure_clang_cl_shim(paths)?;
                        extra_bin_dirs.push(shim_dir);
                        eprintln!(
                            "soldr: skipping managed LLVM bootstrap: {msg}; \
                             falling back to system clang/lld-link/llvm-lib on PATH"
                        );
                    }
                    Err(err) => return Err(err),
                }

                // zlib-ng's ARM optimizations are unbuildable under
                // clang-cl — chain a toolchain-file wrapper that turns
                // them off for the aarch64 lane. See
                // `ensure_zlib_ng_arm_cmake_wrapper` for the full
                // root-cause writeup (cross-run 28574600982 fix).
                if let Some((key, value)) = ensure_zlib_ng_arm_cmake_wrapper(paths, triple)? {
                    if std::env::var_os(&key).is_none() {
                        extra_env.push((key, value));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Write the cmake toolchain-file WRAPPER that disables zlib-ng's ARM
/// optimizations for `aarch64-pc-windows-msvc` cross-compiles, and
/// return the `(env var, path)` pair to export on the child cargo.
/// Returns `Ok(None)` for every other triple.
///
/// ## Why (run 28574600982, windows-arm lane)
///
/// libz-ng-sys builds vendored zlib-ng via cmake. Under clang-cl
/// (`_MSC_VER` defined, but none of MSVC's compiler intrinsics exist),
/// zlib-ng's ARM feature detection is self-INCONSISTENT:
///
/// * `HAVE_ARMV8_INTRIN` probes `__crc32w` from `<intrin.h>` — an
///   MSVC-only declaration clang-cl doesn't ship → probe fails. But
///   the GNU-asm probe `HAVE_ARMV8_INLINE_ASM` passes, so cmake
///   enables `ARM_CRC32` anyway — and `acle_intrins.h`'s `_MSC_VER`
///   code path then requires exactly the missing `__crc32b/h/w/d`
///   intrinsics: `crc32_armv8.c` fails with "call to undeclared
///   function '__crc32b'".
/// * The NEON path includes MSVC's `arm64_neon.h`, whose
///   `vld1q_*_x4` macros expand to `neon_ld1m4_*` compiler magic that
///   clang-cl neither declares nor lowers. The `NEON_HAS_LD4` probe
///   dies at link ("undefined symbol: neon_ld1m4_q32"), and the
///   fallback inline functions zlib-ng then compiles collide with the
///   still-defined macros ("type specifier missing" in
///   `adler32_neon.c` via `neon_intrins.h`).
///
/// Turning `WITH_NEON` / `WITH_ARMV8` / `WITH_ARMV6` off makes
/// zlib-ng build its portable C fallbacks — the only combination
/// clang-cl can actually compile until upstream zlib-ng grows real
/// clang-cl ARM64 support.
///
/// ## How the wrapper reaches cmake
///
/// cargo-xwin exports `CMAKE_TOOLCHAIN_FILE_<underscore-triple>` on
/// the child cargo. The `cmake` crate (used by libz-ng-sys's
/// build.rs) checks the DASH-triple form of that variable FIRST
/// (`getenv_target_os` in cmake-rs), so soldr exports
/// `CMAKE_TOOLCHAIN_FILE_aarch64-pc-windows-msvc` pointing at this
/// wrapper. The wrapper chain-includes cargo-xwin's real clang-cl
/// toolchain file via `$ENV{...}` (still present in the build-script
/// environment) so compiler/linker setup is byte-identical, then
/// force-caches the three `WITH_*` toggles.
fn ensure_zlib_ng_arm_cmake_wrapper(
    paths: &SoldrPaths,
    triple: &str,
) -> Result<Option<(String, String)>, SoldrError> {
    if !(triple.starts_with("aarch64-") && triple.ends_with("-pc-windows-msvc")) {
        return Ok(None);
    }
    let dir = paths.root.join("cmake").join(triple);
    std::fs::create_dir_all(&dir).map_err(|e| {
        SoldrError::Other(format!("create cmake wrapper dir {}: {e}", dir.display()))
    })?;
    let wrapper = dir.join("clang-cl-arm-toolchain.cmake");
    let underscore = triple.replace('-', "_");
    let content = format!(
        r#"# Written by soldr (cross-run 28574600982 fix) — do not edit; regenerated each run.
# Chain-include cargo-xwin's generated clang-cl toolchain file (it
# exports the underscore-form env var on the child cargo) so the
# compiler/linker setup is unchanged.
if(DEFINED ENV{{CMAKE_TOOLCHAIN_FILE_{underscore}}})
    include("$ENV{{CMAKE_TOOLCHAIN_FILE_{underscore}}}")
endif()

# zlib-ng's ARM optimizations require MSVC-only compiler intrinsics
# (__crc32*, neon_ld1m4_*) that clang-cl does not implement; its cmake
# feature detection half-enables them anyway and the build dies in
# crc32_armv8.c / adler32_neon.c. Force the portable C fallbacks.
set(WITH_NEON OFF CACHE BOOL "soldr: MSVC-intrinsic-only under clang-cl" FORCE)
set(WITH_ARMV8 OFF CACHE BOOL "soldr: MSVC-intrinsic-only under clang-cl" FORCE)
set(WITH_ARMV6 OFF CACHE BOOL "soldr: MSVC-intrinsic-only under clang-cl" FORCE)
"#
    );
    std::fs::write(&wrapper, content).map_err(|e| {
        SoldrError::Other(format!("write cmake wrapper {}: {e}", wrapper.display()))
    })?;
    Ok(Some((
        format!("CMAKE_TOOLCHAIN_FILE_{triple}"),
        wrapper.to_string_lossy().into_owned(),
    )))
}

fn append_zigbuild_env_overrides(
    paths: &SoldrPaths,
    triple: &str,
    extra_env: &mut Vec<(String, String)>,
) -> Result<(), SoldrError> {
    let wrappers = match zig_shim::ensure_zig_wrappers(paths, triple) {
        Ok(wrappers) => wrappers,
        Err(SoldrError::UnsupportedPlatform(_)) => return Ok(()),
        Err(err) => return Err(err),
    };
    let suffix = triple.replace('-', "_");
    let upper = suffix.to_uppercase();
    for (key, val) in [
        (format!("CC_{suffix}"), wrappers.cc.as_path()),
        (format!("CXX_{suffix}"), wrappers.cxx.as_path()),
        (format!("AR_{suffix}"), wrappers.ar.as_path()),
        (format!("RANLIB_{suffix}"), wrappers.ranlib.as_path()),
        (
            format!("CARGO_TARGET_{upper}_LINKER"),
            wrappers.cc.as_path(),
        ),
    ] {
        if std::env::var_os(&key).is_none() {
            extra_env.push((key, val.to_string_lossy().into_owned()));
        }
    }
    Ok(())
}

/// Compute environment-variable overrides for the child cargo invocation
/// based on the subcommand and its `--target` argument.
///
/// The only rule today: `cargo xwin build --target <triple>` for any
/// `<triple>` ending in `-pc-windows-msvc` injects
///
///   CC_<triple-underscored>  = clang-cl
///   CXX_<triple-underscored> = clang-cl
///   AR_<triple-underscored>  = llvm-lib
///
/// Why: cc-rs (used by ring, blake3, and other C-FFI crates) detects
/// `target=*-pc-windows-msvc` and formats include flags MSVC-style
/// (`/imsvc <path>`). But cc-rs's default driver for that triple,
/// when `cl.exe` isn't on PATH (typical on Linux cross-compile hosts),
/// is the GNU-flavoured `clang` driver — which interprets `/imsvc`
/// as a literal filename. The result is the build.rs error
///   `clang: error: no such file or directory: '/imsvc'`
/// observed when cross-compiling soldr to windows-arm64 via cargo-xwin
/// on a linux runner.
///
/// The fix is to tell cc-rs to use `clang-cl` (clang's MSVC-compatible
/// driver) explicitly. cc-rs reads `CC_<triple-underscored>` ahead of
/// its default heuristic; setting it routes ring's assembly compilation
/// to the right driver and the build succeeds.
///
/// The caller's existing `CC_*` / `CXX_*` / `AR_*` env vars take
/// precedence (the apply loop checks `std::env::var_os` first); this
/// hook only fills in the gaps.
fn compute_subcommand_env_overrides(args: &[String]) -> Vec<(String, String)> {
    let Some(sub) = first_cargo_subcommand(args) else {
        return Vec::new();
    };
    if sub != "xwin" {
        return Vec::new();
    }
    // Verify the inner verb is `build` / `test` / etc. (anything cc-rs
    // would invoke a build script for). cargo-xwin's other verbs
    // (`download`, `pre-download`) don't compile anything.
    let mut after_sub = args.iter().skip_while(|a| a.as_str() != sub);
    after_sub.next(); // consume the matched `xwin`
    let needs_cc = matches!(
        after_sub.clone().next().map(String::as_str),
        Some("build" | "test" | "check" | "run" | "bench" | "doc" | "clippy" | "rustc"),
    );
    if !needs_cc {
        return Vec::new();
    }
    let Some(triple) = extract_target_arg(args) else {
        return Vec::new();
    };
    if !triple.ends_with("-pc-windows-msvc") {
        return Vec::new();
    }
    let suffix = triple.replace('-', "_");
    vec![
        (format!("CC_{suffix}"), "clang-cl".to_string()),
        (format!("CXX_{suffix}"), "clang-cl".to_string()),
        (format!("AR_{suffix}"), "llvm-lib".to_string()),
    ]
}

/// Find the value of `--target <triple>` or `--target=<triple>` in a
/// cargo arg vector. Returns `None` if the arg isn't present. Used by
/// `compute_subcommand_env_overrides` to decide whether to inject
/// MSVC-target cc-rs env vars.
fn extract_target_arg(args: &[String]) -> Option<&str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--target" {
            return it.next().map(String::as_str);
        }
        if let Some(rest) = a.strip_prefix("--target=") {
            return Some(rest);
        }
    }
    None
}

#[cfg(test)]
mod tests;
