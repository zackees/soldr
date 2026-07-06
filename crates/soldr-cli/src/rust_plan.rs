//! Rust artifact plan, warm-restore short-circuit, thin-slice manifest, and
//! related zccache `rust-plan` integration. Extracted from `main.rs` as part
//! of issue #339.

use crate::cargo_front_door::{
    build_env_inputs, cargo_config_hash, cargo_feature_inputs, cargo_profile, cargo_target_triple,
    file_hash_or_missing, first_cargo_subcommand, path_string, rustflags_inputs, stable_hash_json,
    workspace_manifest_hashes, CargoProfileDebugDefault,
};
use crate::core::{command_output_with_timeout, suppress_windows_console_window, SoldrError};
use crate::zccache::{
    command_stderr, normalize_path_for_compare,
    run_zccache_command_strings_in_cache_dir_with_daemon_name, ZccacheBuildSession,
};
use crate::{
    apply_implicit_toolchain_homes, non_empty_env_path, SKIP_WARM_RESTORE_ENV_VAR,
    TARGET_CACHE_BACKEND_ENV_VAR, TARGET_CACHE_BUNDLE_DIR_ENV_VAR, TARGET_CACHE_MODE_ENV_VAR,
    TARGET_CACHE_PROFILE_ENV_VAR, TARGET_CACHE_TAR_THREADS_ENV_VAR, THIN_MANIFEST_FILENAME,
    WARM_RESTORE_MAX_AGE_SECONDS, WARM_RESTORE_SENTINEL_FILENAME,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[path = "rust_plan_proto.rs"]
mod rust_plan_proto;

#[derive(Debug, Deserialize)]
pub(crate) struct CargoMetadata {
    pub(crate) packages: Vec<CargoMetadataPackage>,
    pub(crate) workspace_members: Vec<String>,
    pub(crate) workspace_root: std::path::PathBuf,
    pub(crate) target_directory: std::path::PathBuf,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CargoMetadataPackage {
    pub(crate) id: String,
    pub(crate) source: Option<String>,
}

/// Returns `true` when `cache_profile` should be omitted from the serialized
/// plan to keep wire compatibility with zccache builds that do not yet know
/// about the `cache_profile` field (e.g. v1.4.0, which uses
/// `#[serde(deny_unknown_fields)]` on `RustArtifactPlanV1`).
///
/// We keep the value in-memory so internal consumers can still branch on it,
/// but we hide it on the wire for everything except the `thin-v2` opt-in.
fn skip_legacy_cache_profile(value: &Option<&'static str>) -> bool {
    !matches!(value, Some("thin-v2"))
}

#[derive(Debug, Serialize)]
pub(crate) struct RustArtifactPlan {
    pub(crate) schema_version: u32,
    pub(crate) mode: String,
    /// Thin-slice pruning policy in effect, e.g. `thin-v1` (legacy) or
    /// `thin-v2` (fingerprint-aware prune). Only emitted on the wire when
    /// it carries new information (i.e. `thin-v2`). Omitted entirely for
    /// `thin-v1` and `mode == "full"` so zccache builds with
    /// `#[serde(deny_unknown_fields)]` (e.g. v1.4.0) can still parse the
    /// plan unchanged.
    #[serde(skip_serializing_if = "skip_legacy_cache_profile")]
    pub(crate) cache_profile: Option<&'static str>,
    pub(crate) workspace_root: String,
    pub(crate) target_dir: String,
    pub(crate) toolchain: RustToolchainIdentity,
    pub(crate) target_triple: String,
    pub(crate) profile: String,
    pub(crate) inputs: RustPlanInputs,
    pub(crate) packages: RustPlanPackages,
    pub(crate) allowed_artifact_classes: Vec<&'static str>,
    /// Categories soldr explicitly drops from the slice. zccache may use this
    /// to short-circuit walks for files it would otherwise consider keeping.
    /// Empty for legacy `thin-v1` and `full` modes — preserves backwards
    /// compatibility with zccache builds that do not yet understand it.
    /// Skipped from the JSON entirely when empty so older zccache builds
    /// with `#[serde(deny_unknown_fields)]` keep accepting the plan.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) dropped_artifact_classes: Vec<&'static str>,
    pub(crate) cache_schema_version: u32,
    pub(crate) journal_log_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RustToolchainIdentity {
    pub(crate) rustc: String,
    pub(crate) cargo: String,
    pub(crate) channel: String,
    pub(crate) host: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct RustPlanInputs {
    pub(crate) features_hash: String,
    pub(crate) rustflags_hash: String,
    pub(crate) env_hash: String,
    pub(crate) lockfile_hash: String,
    pub(crate) cargo_config_hash: String,
    pub(crate) manifest_hashes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RustPlanPackages {
    pub(crate) selected_package_ids: Vec<String>,
    pub(crate) workspace_package_ids: Vec<String>,
    pub(crate) excluded_path_package_ids: Vec<String>,
}

pub(crate) struct RustArtifactPlanContext {
    pub(crate) path: std::path::PathBuf,
    pub(crate) zccache_binary: std::path::PathBuf,
    pub(crate) cache_dir: std::path::PathBuf,
    pub(crate) zccache_daemon_cache_dir: std::path::PathBuf,
    pub(crate) zccache_daemon_cache_dir_env: bool,
    pub(crate) zccache_daemon_name: Option<String>,
    pub(crate) session_id: String,
    pub(crate) journal_path: std::path::PathBuf,
    pub(crate) backend: String,
    /// Active thin-slice pruning policy. Only `Some` for thin modes; `None`
    /// for `full` so the manifest emitter can short-circuit.
    pub(crate) cache_profile: Option<&'static str>,
    /// Stable digest over the plan inputs (toolchain, lockfile, manifests,
    /// features, env, cargo config, target triple, profile, packages). Used
    /// by the warm-restore sentinel (issue #229) to prove that a previous
    /// step in the same job left `target/` in the exact state `restore`
    /// would produce, so the next `restore` can be skipped.
    pub(crate) plan_inputs_hash: String,
    /// Absolute target dir from the active plan, mirrored into the warm-
    /// restore sentinel so step 2 can verify it is being asked to restore
    /// into the same tree step 1 saved.
    pub(crate) target_dir: String,
}

pub(crate) fn maybe_prepare_rust_artifact_plan(
    cargo: &std::path::Path,
    rustc: &std::path::Path,
    args: &[String],
    session: &ZccacheBuildSession,
    cargo_profile_debug_default: Option<&CargoProfileDebugDefault>,
) -> Result<Option<RustArtifactPlanContext>, SoldrError> {
    let Some(mode) = rust_artifact_cache_mode_from_env()? else {
        return Ok(None);
    };

    if matches!(first_cargo_subcommand(args), Some("install")) {
        eprintln!("soldr: rust artifact cache plan skipped for cargo install");
        return Ok(None);
    }

    let profile = if mode == "thin" {
        Some(rust_artifact_cache_profile_from_env()?)
    } else {
        None
    };

    // Reject a malformed SOLDR_TARGET_CACHE_TAR_THREADS before we kick off
    // cargo metadata. zccache also validates, but failing here keeps the
    // error close to the user's typo and avoids spending seconds resolving
    // the workspace just to die on a one-character env mistake.
    rust_artifact_cache_tar_threads_from_env()?;

    let metadata = cargo_metadata(cargo, args)?;
    let toolchain = rust_toolchain_identity(cargo, rustc)?;
    let plan = build_rust_artifact_plan(
        &metadata,
        &toolchain,
        args,
        &mode,
        profile,
        session,
        cargo_profile_debug_default,
    )?;
    let plan_dir = session.cache_dir.join("plans");
    std::fs::create_dir_all(&plan_dir)?;
    let plan_path = plan_dir.join("last-rust-artifact-plan.pb");
    let plan_bytes = rust_plan_proto::plan_to_proto_bytes(&plan)?;
    std::fs::write(&plan_path, plan_bytes)?;

    let plan_inputs_hash = compute_plan_inputs_hash(&plan);
    let target_dir = plan.target_dir.clone();

    Ok(Some(RustArtifactPlanContext {
        path: plan_path,
        zccache_binary: session.binary_path.clone(),
        cache_dir: rust_artifact_plan_cache_dir(session)?,
        zccache_daemon_cache_dir: session.cache_dir.clone(),
        zccache_daemon_cache_dir_env: session.cache_dir_env,
        zccache_daemon_name: session
            .private_daemon
            .as_ref()
            .map(|private| private.daemon_name.clone()),
        session_id: session.session_id.clone(),
        journal_path: session.journal_path.clone(),
        backend: rust_artifact_cache_backend_from_env()?,
        cache_profile: profile,
        plan_inputs_hash,
        target_dir,
    }))
}

/// Stable digest summarising every plan field cargo would consult to decide
/// whether the cached `target/` tree is still valid. Used by the warm-restore
/// sentinel (issue #229) to prove that an in-job repeat of `soldr cargo ...`
/// is asking to restore into the same tree it just saved.
///
/// We hash a tuple of (toolchain identity, target triple, profile, mode,
/// cache profile, plan inputs, package selection) rather than the whole
/// `RustArtifactPlan` so the sentinel does not falsely diverge on cosmetic
/// fields (`schema_version`, `journal_log_path`, etc.).
pub(crate) fn compute_plan_inputs_hash(plan: &RustArtifactPlan) -> String {
    let payload = serde_json::json!({
        "toolchain": {
            "rustc": plan.toolchain.rustc,
            "cargo": plan.toolchain.cargo,
            "channel": plan.toolchain.channel,
            "host": plan.toolchain.host,
        },
        "target_triple": plan.target_triple,
        "profile": plan.profile,
        "mode": plan.mode,
        "cache_profile": plan.cache_profile,
        "inputs": {
            "features_hash": plan.inputs.features_hash,
            "rustflags_hash": plan.inputs.rustflags_hash,
            "env_hash": plan.inputs.env_hash,
            "lockfile_hash": plan.inputs.lockfile_hash,
            "cargo_config_hash": plan.inputs.cargo_config_hash,
            "manifest_hashes": plan.inputs.manifest_hashes,
        },
        "packages": {
            "selected_package_ids": plan.packages.selected_package_ids,
            "workspace_package_ids": plan.packages.workspace_package_ids,
            "excluded_path_package_ids": plan.packages.excluded_path_package_ids,
        },
        "allowed_artifact_classes": plan.allowed_artifact_classes,
        "dropped_artifact_classes": plan.dropped_artifact_classes,
    });
    stable_hash_json(&payload)
}

#[path = "rust_plan_warm_restore.rs"]
mod warm_restore;
pub(crate) use warm_restore::{
    current_unix_seconds, evaluate_warm_restore_skip, should_skip_warm_restore,
    warm_restore_sentinel_path, warm_restore_skip_enabled, write_warm_restore_sentinel,
    WarmRestoreSentinel, WarmRestoreSkipInputs,
};

/// Walk `deps_dir` shallowly and delete every `.rmeta` file whose filename
/// stem has no matching `.rlib`, `.so`, `.dylib`, or `.dll` sibling.
/// Returns the number of files deleted. See soldr#410: a half-completed
/// rustc invocation (rmeta emitted before codegen finishes) leaves orphan
/// rmetas that poison the next `cargo build` with
/// `E0463: can't find crate`. Best-effort — IO errors are reported via
/// stderr and skipped so the caller can still surface the original cargo
/// failure.
pub(crate) fn prune_orphan_rmetas_in_deps(deps_dir: &std::path::Path) -> usize {
    let entries = match std::fs::read_dir(deps_dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let mut rmeta_paths: Vec<std::path::PathBuf> = Vec::new();
    let mut companion_stems: std::collections::HashSet<std::ffi::OsString> =
        std::collections::HashSet::new();

    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(ext) = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
        else {
            continue;
        };
        let Some(stem) = path.file_stem() else {
            continue;
        };
        match ext.as_str() {
            "rmeta" => rmeta_paths.push(path.clone()),
            "rlib" | "so" | "dylib" | "dll" => {
                companion_stems.insert(stem.to_owned());
            }
            _ => {}
        }
    }

    let mut deleted = 0;
    for rmeta in rmeta_paths {
        let Some(stem) = rmeta.file_stem() else {
            continue;
        };
        if companion_stems.contains(stem) {
            continue;
        }
        match std::fs::remove_file(&rmeta) {
            Ok(()) => deleted += 1,
            Err(e) => eprintln!(
                "soldr warning: failed to prune orphan rmeta {}: {e}",
                rmeta.display()
            ),
        }
    }
    deleted
}

/// Apply [`prune_orphan_rmetas_in_deps`] to the deps directory implied by
/// the active artifact plan. `target_dir` from the plan is the workspace
/// `target/` root (per `cargo metadata`); the actual deps live under
/// `<target>/[<triple>/]<profile>/deps/`. We walk every directory named
/// `deps` up to a small depth so we cover both the host-target layout
/// (`target/<profile>/deps/`) and the explicit-target layout
/// (`target/<triple>/<profile>/deps/`) without needing to thread the
/// triple/profile through the call site.
pub(crate) fn prune_orphan_rmetas_after_failed_build(plan: &RustArtifactPlanContext) {
    let target_root = std::path::PathBuf::from(&plan.target_dir);
    let mut total = 0usize;
    for deps_dir in find_deps_dirs(&target_root, 3) {
        total = total.saturating_add(prune_orphan_rmetas_in_deps(&deps_dir));
    }
    if total > 0 {
        eprintln!(
            "soldr: pruned {total} orphan .rmeta file(s) under {} after failed cargo build (soldr#410)",
            target_root.display()
        );
    }
}

/// Locate `deps/` subdirectories under `root` up to `max_depth` levels
/// deep (inclusive). Designed to find the cargo `target/[<triple>/]<profile>/deps/`
/// trees without descending into unrelated directories like `incremental/`,
/// `build/`, or `doc/`.
fn find_deps_dirs(root: &std::path::Path, max_depth: usize) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    walk_for_deps_dirs(root, max_depth, &mut out);
    out
}

#[cfg(test)]
pub(crate) fn find_deps_dirs_for_test(
    root: &std::path::Path,
    max_depth: usize,
) -> Vec<std::path::PathBuf> {
    find_deps_dirs(root, max_depth)
}

fn walk_for_deps_dirs(
    dir: &std::path::Path,
    remaining_depth: usize,
    out: &mut Vec<std::path::PathBuf>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == "deps") {
            out.push(path.clone());
            // Do not descend further: cargo never nests another `deps/`
            // inside a `deps/`, and we want to keep the walk shallow.
            continue;
        }
        if remaining_depth > 0 {
            walk_for_deps_dirs(&path, remaining_depth - 1, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Pre-populated target/ restore guard (issue #480).
// ---------------------------------------------------------------------------
//
// When `cargo chef cook` (or any prior `soldr cargo build`) has populated
// `target/`, running `zccache rust-plan restore` on top of the existing tree
// can produce the failure mode described in #480: `restored_file_count: 0 /
// artifact_absent_from_restored_plan: 1`, followed by cargo failing because
// the expected `.rmeta` files aren't where it left them.
//
// This guard detects that case (cargo `.fingerprint/` dirs already on disk)
// and skips restore, letting cargo work with the existing target tree. The
// warm-restore short-circuit (#229) covers the in-job repeat case where the
// plan inputs hash matches; this guard covers the cross-context case where
// cook saved one plan and the consumer build computes a different inputs
// hash but reuses the same target/.

/// Env var to override the prepopulated-target restore guard. When set to a
/// truthy value (anything other than "", "0", "false", "no", "off") the
/// guard is bypassed and `rust-plan restore` runs even when the target tree
/// already contains cargo fingerprints. Provided as an escape hatch for users
/// who specifically want the old behavior.
pub(crate) const SOLDR_FORCE_RESTORE_ENV_VAR: &str = "SOLDR_RUST_PLAN_FORCE_RESTORE";

/// Returns `Some(reason)` when the prepopulated-target guard wants to skip
/// `rust-plan restore` for this plan; `None` when restore should proceed.
///
/// "Prepopulated" is detected by walking `<target>/` shallowly for any
/// `.fingerprint/` directory with at least one entry. Cargo writes
/// `.fingerprint/<crate>-<hash>/` for every unit it compiles, and cook's
/// `cargo chef cook` step produces a populated `.fingerprint/` as a
/// side-effect of building the dep stub.
pub(crate) fn should_skip_restore_due_to_prepopulated_target(
    plan: &RustArtifactPlanContext,
) -> Option<String> {
    if force_restore_enabled() {
        return None;
    }
    let target_root = std::path::PathBuf::from(&plan.target_dir);
    if !target_root.exists() {
        return None;
    }
    let populated = count_populated_fingerprint_dirs(&target_root, 3);
    if populated == 0 {
        return None;
    }
    Some(format!(
        "soldr: skipping rust-plan restore; target dir {} is prepopulated \
         with {} cargo .fingerprint dir{} (likely cook output or prior build, #480). \
         Restoring on top of an existing tree can poison the build with \
         `artifact_absent_from_restored_plan`; cargo will work with the \
         existing artifacts instead. Set {}=1 to override.",
        plan.target_dir,
        populated,
        if populated == 1 { "" } else { "s" },
        SOLDR_FORCE_RESTORE_ENV_VAR,
    ))
}

fn force_restore_enabled() -> bool {
    let Some(raw) = std::env::var_os(SOLDR_FORCE_RESTORE_ENV_VAR) else {
        return false;
    };
    let lowered = raw.to_string_lossy().trim().to_ascii_lowercase();
    !matches!(lowered.as_str(), "" | "0" | "false" | "no" | "off")
}

/// Count `.fingerprint/` directories under `root` (up to `max_depth` levels)
/// that contain at least one entry. The walk stops descending once a
/// `.fingerprint/` is encountered — cargo never nests another inside.
pub(crate) fn count_populated_fingerprint_dirs(root: &std::path::Path, max_depth: usize) -> usize {
    let mut count = 0usize;
    walk_for_fingerprint_dirs(root, max_depth, &mut count);
    count
}

fn walk_for_fingerprint_dirs(dir: &std::path::Path, remaining_depth: usize, count: &mut usize) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".fingerprint") {
            // Only count as "populated" if at least one entry exists.
            let populated = std::fs::read_dir(&path)
                .map(|mut iter| iter.next().is_some())
                .unwrap_or(false);
            if populated {
                *count += 1;
            }
            continue;
        }
        if remaining_depth > 0 {
            walk_for_fingerprint_dirs(&path, remaining_depth - 1, count);
        }
    }
}

#[path = "rust_plan_env.rs"]
mod env_resolvers;
pub(crate) use env_resolvers::{
    parse_rust_artifact_cache_tar_threads, rust_artifact_cache_profile_from_env,
};
use env_resolvers::{
    rust_artifact_cache_backend_from_env, rust_artifact_cache_mode_from_env,
    rust_artifact_cache_tar_threads_from_env,
};

fn rust_artifact_plan_cache_dir(
    session: &ZccacheBuildSession,
) -> Result<std::path::PathBuf, SoldrError> {
    let cache_dir = non_empty_env_path(TARGET_CACHE_BUNDLE_DIR_ENV_VAR)
        .unwrap_or_else(|| session.cache_dir.join("rust-plan-cache"));
    let cache_dir = normalize_path_for_compare(&cache_dir)?;
    std::fs::create_dir_all(&cache_dir)?;
    Ok(cache_dir)
}

fn cargo_metadata(cargo: &std::path::Path, args: &[String]) -> Result<CargoMetadata, SoldrError> {
    let mut command = std::process::Command::new(cargo);
    command.args(["metadata", "--format-version", "1"]);
    command.args(cargo_metadata_passthrough_args(args));
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    command.env_remove("MAKEFLAGS");
    command.env_remove("CARGO_MAKEFLAGS");

    let output = command_output_with_timeout(&mut command, "cargo metadata")?;
    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "cargo metadata failed while preparing Rust artifact cache plan: {}",
            command_stderr(&output)
        )));
    }

    serde_json::from_slice(&output.stdout).map_err(|e| {
        SoldrError::Other(format!(
            "failed to parse cargo metadata while preparing Rust artifact cache plan: {e}"
        ))
    })
}

pub(crate) fn cargo_metadata_passthrough_args(args: &[String]) -> Vec<std::ffi::OsString> {
    let mut values = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        match arg.as_str() {
            "--locked" | "--offline" | "--frozen" | "--all-features" | "--no-default-features" => {
                values.push(arg.as_str().into())
            }
            "--manifest-path" | "--config" | "--features" | "--filter-platform" => {
                if let Some(value) = iter.next() {
                    values.push(arg.as_str().into());
                    values.push(value.as_str().into());
                }
            }
            _ => {
                for flag in [
                    "--manifest-path=",
                    "--config=",
                    "--features=",
                    "--filter-platform=",
                ] {
                    if arg.starts_with(flag) {
                        values.push(arg.as_str().into());
                    }
                }
            }
        }
    }
    values
}

fn rust_toolchain_identity(
    cargo: &std::path::Path,
    rustc: &std::path::Path,
) -> Result<RustToolchainIdentity, SoldrError> {
    let rustc_output = tool_output(rustc, &["-Vv"])?;
    let cargo_output = tool_output(cargo, &["--version"])?;
    let host = rustc_output
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap_or("unknown")
        .to_string();
    let channel = std::env::var("RUSTUP_TOOLCHAIN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            rustc_output
                .lines()
                .find_map(|line| line.strip_prefix("release: "))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());

    Ok(RustToolchainIdentity {
        rustc: rustc_output.trim().to_string(),
        cargo: cargo_output.trim().to_string(),
        channel,
        host,
    })
}

fn tool_output(tool: &std::path::Path, args: &[&str]) -> Result<String, SoldrError> {
    let mut command = std::process::Command::new(tool);
    command.args(args);
    apply_implicit_toolchain_homes(&mut command);
    suppress_windows_console_window(&mut command);
    let output = command_output_with_timeout(
        &mut command,
        &format!("{} {}", tool.display(), args.join(" ")),
    )?;
    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "{} {} failed: {}",
            tool.display(),
            args.join(" "),
            command_stderr(&output)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub(crate) fn build_rust_artifact_plan(
    metadata: &CargoMetadata,
    toolchain: &RustToolchainIdentity,
    args: &[String],
    mode: &str,
    cache_profile: Option<&'static str>,
    session: &ZccacheBuildSession,
    cargo_profile_debug_default: Option<&CargoProfileDebugDefault>,
) -> Result<RustArtifactPlan, SoldrError> {
    let workspace_root = normalize_path_for_compare(&metadata.workspace_root)?;
    let target_dir = normalize_path_for_compare(&metadata.target_directory)?;
    let workspace_members: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    let mut selected_package_ids = Vec::new();
    let mut excluded_path_package_ids = Vec::new();

    for package in &metadata.packages {
        if workspace_members.contains(package.id.as_str()) {
            continue;
        }
        match package.source.as_deref() {
            Some(source) if source.starts_with("registry+") || source.starts_with("git+") => {
                selected_package_ids.push(package.id.clone());
            }
            _ => excluded_path_package_ids.push(package.id.clone()),
        }
    }

    selected_package_ids.sort();
    excluded_path_package_ids.sort();
    let mut workspace_package_ids = metadata.workspace_members.clone();
    workspace_package_ids.sort();

    let allowed = allowed_artifact_classes(mode, cache_profile);
    let dropped = dropped_artifact_classes(mode, cache_profile);
    let cache_schema_version = match cache_profile {
        Some("thin-v2") => 2,
        _ => 1,
    };

    Ok(RustArtifactPlan {
        schema_version: 1,
        mode: mode.to_string(),
        cache_profile,
        workspace_root: path_string(&workspace_root),
        target_dir: path_string(&target_dir),
        toolchain: RustToolchainIdentity {
            rustc: toolchain.rustc.clone(),
            cargo: toolchain.cargo.clone(),
            channel: toolchain.channel.clone(),
            host: toolchain.host.clone(),
        },
        target_triple: cargo_target_triple(args, &toolchain.host),
        profile: cargo_profile(args).to_string(),
        inputs: RustPlanInputs {
            features_hash: stable_hash_json(&cargo_feature_inputs(args)),
            rustflags_hash: stable_hash_json(&rustflags_inputs()),
            env_hash: stable_hash_json(&build_env_inputs(cargo_profile_debug_default)),
            lockfile_hash: file_hash_or_missing(&workspace_root.join("Cargo.lock"))?,
            cargo_config_hash: cargo_config_hash(&workspace_root)?,
            manifest_hashes: workspace_manifest_hashes(&workspace_root)?,
        },
        packages: RustPlanPackages {
            selected_package_ids,
            workspace_package_ids,
            excluded_path_package_ids,
        },
        allowed_artifact_classes: allowed,
        dropped_artifact_classes: dropped,
        cache_schema_version,
        journal_log_path: Some(path_string(&session.journal_path)),
    })
}

/// Artifact classes the thin-slice walker is permitted to copy into the bundle.
///
/// `thin-v1` (legacy) preserves the historical contents that ship `.rlib`/
/// `.rmeta`/proc-macro library bytes alongside the freshness inputs. This is
/// kept as the safety-net default while the in-CI verification job from
/// `docs/THIN_TARGET_CACHE_PRUNING.md` Section 5 is being rolled out.
///
/// `thin-v2` is the fingerprint-aware aggressive prune. It keeps only what
/// cargo actually consults to make a fresh-vs-rebuild decision (fingerprints,
/// dep-info, build-script `out_dir/` contents, small build-script metadata).
/// The dropped library bytes are reproduced on demand by zccache's compilation
/// cache when cargo asks rustc to rebuild the missing unit.
pub(crate) fn allowed_artifact_classes(
    mode: &str,
    cache_profile: Option<&'static str>,
) -> Vec<&'static str> {
    if mode == "full" {
        return Vec::new();
    }
    match cache_profile {
        Some("thin-v2") => vec![
            // Fingerprint metadata cargo reads to decide skip-vs-rebuild.
            // Split from the legacy `cargo_fingerprint` umbrella per
            // `docs/THIN_TARGET_CACHE_PRUNING.md` Section 4.3.
            "cargo_fingerprint_meta",
            "dep_info",
            "build_script_metadata",
            "build_script_output",
        ],
        // thin-v1 (default) and any unrecognized profile that arrived via a
        // future zccache that does not yet branch on `cache_profile` get the
        // legacy class list so behavior is unchanged on rollout day 0.
        _ => vec![
            "rlib",
            "rmeta",
            "dep_info",
            "proc_macro",
            "cargo_fingerprint",
            "build_script_metadata",
            "build_script_output",
        ],
    }
}

/// Artifact classes the thin-slice walker must explicitly skip in the active
/// profile. Surfaced to zccache so it can short-circuit walks for paths it
/// would otherwise copy. Returning the drop list as data (rather than baking
/// it into zccache) keeps the policy decision in soldr where the design
/// discussion already lives.
pub(crate) fn dropped_artifact_classes(
    mode: &str,
    cache_profile: Option<&'static str>,
) -> Vec<&'static str> {
    if mode == "full" {
        return Vec::new();
    }
    match cache_profile {
        Some("thin-v2") => vec![
            // Multi-GB rustc incremental DB. Churns per-commit, low CI hit
            // rate. Cargo never reads it to decide freshness.
            "incremental",
            // Compiled build-script binaries. Cheap to regenerate from
            // cached deps; bytes live in zccache's content store when needed.
            "build_script_build",
            // Library output bytes. zccache repopulates on rustc miss.
            "rlib",
            "rmeta",
            // proc-macro shared libraries. Same story as `.rlib`.
            "proc_macro",
            // Split debug-info / pdb / macOS dSYM bundles.
            "dwo",
            "pdb",
            "dsym",
            // The fingerprint *outputs* (not the metadata). The metadata is
            // tiny and load-bearing for freshness; the outputs are large.
            "cargo_fingerprint_outputs",
        ],
        _ => Vec::new(),
    }
}

pub(crate) fn run_zccache_rust_plan(
    plan: &RustArtifactPlanContext,
    operation: &'static str,
    _include_session: bool,
) -> Result<(), SoldrError> {
    // soldr#1368: rust-plan save/restore runs in-process against the
    // compiled-in zccache artifact library — no `zccache rust-plan`
    // subprocess and no managed daemon. The library exposes the LOCAL
    // backend only; `gha` / `auto` resolve to local (the GHA rust-plan
    // path lives in zccache's CLI, not its library — see the #1368
    // report's GHA verdict). The journal + session id ride in the plan
    // file itself, so the old `--journal` / `--session-id` / `--backend`
    // plumbing is dropped for the local path.
    if plan.backend == "gha" {
        eprintln!(
            "soldr: rust-plan `gha` backend is unavailable in-process (the embedded \
             zccache library exposes only the local backend); using local for {operation}."
        );
    }

    let loaded = zccache::artifact::RustArtifactPlanV1::load(&plan.path).map_err(|e| {
        SoldrError::Other(format!(
            "failed to load rust-plan {}: {e}",
            plan.path.display()
        ))
    })?;

    let summary = match operation {
        "save" => zccache::artifact::save_rust_plan_local(&loaded, &plan.cache_dir),
        "restore" => zccache::artifact::restore_rust_plan_local(&loaded, &plan.cache_dir),
        other => {
            return Err(SoldrError::Other(format!(
                "unknown rust-plan operation {other:?}"
            )))
        }
    }
    .map_err(|e| SoldrError::Other(format!("zccache rust-plan {operation} failed: {e}")))?;

    if let Ok(rendered) = serde_json::to_string(&summary) {
        eprintln!("soldr: zccache rust-plan {operation} summary");
        eprintln!("{rendered}");
    }
    if operation == "restore" {
        warn_if_rust_plan_restore_incomplete(&summary);
    }

    if operation == "save" && plan.cache_profile == Some("thin-v2") {
        if let Err(e) = write_thin_manifest(&plan.cache_dir, plan.cache_profile) {
            // Manifest emission is diagnostic; never fail the build because
            // we could not write it. Log so it shows up in CI logs.
            eprintln!(
                "soldr warning: failed to write thin-slice manifest at {}: {e}",
                plan.cache_dir.display()
            );
        }
    }
    Ok(())
}

/// Schema for `<thin-root>/manifest.v2.json`.
///
/// Written by soldr after `zccache rust-plan save` produces the bundle.
/// Downstream tooling (e.g. setup-soldr verification jobs) reads this to
/// prove what landed in the slice without unpacking it.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ThinSliceManifest {
    /// Manifest format version. `2` for the file-list manifest produced by
    /// the `thin-v2` profile.
    pub(crate) schema_version: u32,
    /// Active thin-slice pruning policy when this manifest was written.
    pub(crate) cache_profile: String,
    /// Absolute path of the bundle root the entries are relative to.
    pub(crate) bundle_root: String,
    /// Timestamp of manifest emission, RFC 3339 / seconds since epoch.
    pub(crate) generated_at_unix_seconds: u64,
    /// Every file in the bundle, sorted by relative path for stable diffs.
    pub(crate) files: Vec<ThinSliceManifestEntry>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ThinSliceManifestEntry {
    /// Path relative to `bundle_root`, forward-slashed for cross-platform diffability.
    pub(crate) path: String,
    /// File size in bytes. Optional because broken symlinks etc. may not
    /// have a usable size; serialized as `null` rather than skipped so the
    /// shape is uniform across entries.
    pub(crate) size_bytes: Option<u64>,
}

pub(crate) fn write_thin_manifest(
    bundle_root: &std::path::Path,
    cache_profile: Option<&'static str>,
) -> Result<(), SoldrError> {
    let profile = cache_profile.unwrap_or("thin-v1").to_string();
    if !bundle_root.exists() {
        // Nothing to manifest; skip rather than spamming an empty file.
        return Ok(());
    }
    let manifest = build_thin_manifest(bundle_root, &profile)?;
    let manifest_path = bundle_root.join(THIN_MANIFEST_FILENAME);
    let json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| SoldrError::Other(format!("failed to serialize thin-slice manifest: {e}")))?;
    std::fs::write(&manifest_path, json)?;
    Ok(())
}

pub(crate) fn build_thin_manifest(
    bundle_root: &std::path::Path,
    cache_profile: &str,
) -> Result<ThinSliceManifest, SoldrError> {
    let thread_count = resolve_bundle_walk_thread_count(
        &std::env::var(TARGET_CACHE_TAR_THREADS_ENV_VAR).unwrap_or_default(),
    )?;
    let mut files = walk_bundle_files(bundle_root, thread_count)?;
    // Drop any prior manifest so the file list does not chase its own tail
    // across repeated saves into the same bundle directory.
    files.retain(|entry| entry.path != THIN_MANIFEST_FILENAME);
    // Sort so the manifest is byte-identical regardless of walk order
    // (sequential vs parallel must produce the same output).
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let generated_at_unix_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(ThinSliceManifest {
        schema_version: 2,
        cache_profile: cache_profile.to_string(),
        bundle_root: path_string(bundle_root),
        generated_at_unix_seconds,
        files,
    })
}

/// Cap on the number of metadata-stat threads soldr will spin up for the
/// bundle walk, matching the documented zccache cap. Past ~8 threads the
/// per-file `GetFileInformation` syscall stops being the bottleneck (the
/// directory iteration becomes one), so additional workers just contend.
pub(crate) const BUNDLE_WALK_THREAD_CAP: usize = 8;

/// Resolve the reader-thread count for the bundle walk from a raw
/// `SOLDR_TARGET_CACHE_TAR_THREADS` value.
///
/// The env var has already been validated by
/// [`parse_rust_artifact_cache_tar_threads`] at the cargo front door, so
/// the raw input here is expected to be well-formed. We re-validate
/// defensively because [`build_thin_manifest`] can also run on the bare
/// `RUSTC_WRAPPER` passthrough path that does not flow through the front
/// door check.
///
/// Returns:
/// - `None` for `auto` / unset — use rayon's global pool, capped at the
///   smaller of the system parallelism and [`BUNDLE_WALK_THREAD_CAP`].
/// - `Some(1)` to force the sequential fallback (no rayon overhead).
/// - `Some(n)` for an explicit thread count, clamped to
///   `[1, BUNDLE_WALK_THREAD_CAP]`.
pub(crate) fn resolve_bundle_walk_thread_count(raw: &str) -> Result<Option<usize>, SoldrError> {
    let parsed = parse_rust_artifact_cache_tar_threads(raw)?;
    let Some(token) = parsed else {
        // Unset → auto.
        return Ok(None);
    };
    if token == "auto" {
        return Ok(None);
    }
    // parse_rust_artifact_cache_tar_threads already rejected zero / negative /
    // non-integer values, so an integer-or-bust parse here is sound.
    let n: usize = token.parse().map_err(|_| {
        SoldrError::Other(format!(
            "invalid {TARGET_CACHE_TAR_THREADS_ENV_VAR} value {raw:?}; expected `auto` or a positive integer (use `1` to disable parallelism)"
        ))
    })?;
    Ok(Some(n.clamp(1, BUNDLE_WALK_THREAD_CAP)))
}

/// Walk every file under `root` and return one [`ThinSliceManifestEntry`]
/// per regular file.
///
/// Implementation is two-phase:
/// 1. Serial directory traversal (`read_dir`) collects every file path. Per
///    `read_dir` is cheap; the per-entry cost is dominated by the metadata
///    stat in phase 2 (which on Windows pays a Defender callback per file).
/// 2. Parallel `std::fs::metadata` over the collected paths via rayon.
///    Output order is non-deterministic — the caller MUST sort.
///
/// `thread_count`:
/// - `None` → use rayon's global thread pool.
/// - `Some(1)` → fully sequential (no rayon overhead at all).
/// - `Some(n)` for `n > 1` → run inside a scoped thread pool of `n`
///   workers so the env var actually controls something soldr-side.
pub(crate) fn walk_bundle_files(
    root: &std::path::Path,
    thread_count: Option<usize>,
) -> Result<Vec<ThinSliceManifestEntry>, SoldrError> {
    // Phase 1: serial DFS collects (absolute_path, relative_string) pairs.
    let mut paths: Vec<(std::path::PathBuf, String)> = Vec::new();
    collect_bundle_file_paths(root, root, &mut paths)?;

    // Phase 2: stat each file. Sequential when only one worker is wanted,
    // rayon-parallel otherwise.
    let stat = |(path, rel): &(std::path::PathBuf, String)| -> ThinSliceManifestEntry {
        let size_bytes = std::fs::metadata(path).ok().map(|m| m.len());
        ThinSliceManifestEntry {
            path: rel.clone(),
            size_bytes,
        }
    };

    let files = match thread_count {
        Some(1) => paths.iter().map(stat).collect(),
        Some(n) => {
            use rayon::prelude::*;
            // Build a scoped pool so the explicit thread count actually
            // bounds this walk instead of leaking onto rayon's global pool.
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(n)
                .thread_name(|i| format!("soldr-bundle-walk-{i}"))
                .build()
                .map_err(|e| {
                    SoldrError::Other(format!("failed to build bundle-walk thread pool: {e}"))
                })?;
            pool.install(|| paths.par_iter().map(stat).collect())
        }
        None => {
            use rayon::prelude::*;
            paths.par_iter().map(stat).collect()
        }
    };

    Ok(files)
}

/// Recursively walk `dir`, pushing `(absolute_path, root-relative string)`
/// for every regular file under it. Used by [`walk_bundle_files`] as the
/// directory-iteration phase before per-file metadata stats are fanned out.
fn collect_bundle_file_paths(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(std::path::PathBuf, String)>,
) -> Result<(), SoldrError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(SoldrError::from(e)),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_bundle_file_paths(root, &path, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|_| path.clone());
            let rel_string = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((path, rel_string));
        }
    }
    Ok(())
}

fn warn_if_rust_plan_restore_incomplete(summary: &zccache::artifact::RustPlanSummary) {
    // soldr#1368: read the in-process summary directly. The
    // "artifact absent from the restored plan" signal is tracked as a
    // skip reason in the library summary.
    let absent = summary
        .skipped_reasons
        .get("artifact_absent_from_restored_plan")
        .copied()
        .unwrap_or(0);
    if absent == 0 {
        return;
    }
    let restored = summary.restored_file_count.to_string();
    eprintln!(
        "soldr warning: rust-plan restore is partial \
         (artifact_absent_from_restored_plan={absent}, restored_file_count={restored}); \
         Cargo is likely to fail with missing .rmeta errors. This usually means two \
         `soldr cargo build` invocations are sharing the same --target-dir. Use a \
         distinct --target-dir for each build or clear the target directory before \
         re-running. See https://github.com/zackees/soldr/issues/228 for context."
    );
}

#[cfg(test)]
#[path = "rust_plan_tests/mod.rs"]
mod tests;
