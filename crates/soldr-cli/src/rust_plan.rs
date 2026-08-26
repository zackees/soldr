//! Rust artifact plan, warm-restore short-circuit, thin-slice manifest, and
//! related zccache `rust-plan` integration. Extracted from `main.rs` as part
//! of issue #339.

use crate::build_cache_session::{command_stderr, BuildCacheSession};
use crate::cargo_front_door::{
    build_env_inputs, cargo_feature_inputs, cargo_profile, cargo_target_triple,
    first_cargo_subcommand, path_string, rustflags_inputs, stable_hash_json,
    CargoProfileDebugDefault,
};
use crate::core::{command_output_with_timeout, suppress_windows_console_window, SoldrError};
use crate::zccache::normalize_path_for_compare;
use crate::{
    non_empty_env_path, SKIP_WARM_RESTORE_ENV_VAR, TARGET_CACHE_BACKEND_ENV_VAR,
    TARGET_CACHE_BUNDLE_DIR_ENV_VAR, TARGET_CACHE_MODE_ENV_VAR, TARGET_CACHE_PROFILE_ENV_VAR,
    TARGET_CACHE_TAR_THREADS_ENV_VAR, THIN_MANIFEST_FILENAME, WARM_RESTORE_MAX_AGE_SECONDS,
    WARM_RESTORE_SENTINEL_FILENAME,
};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[path = "rust_plan_proto.rs"]
mod rust_plan_proto;

#[path = "rust_plan_memo.rs"]
mod rust_plan_memo;
pub(crate) use rust_plan_memo::{ToolchainProbe, WorkspaceFileHashes};

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
    /// Absolute manifest path as reported by `cargo metadata`. Consumed by
    /// the prep memo (soldr#1540) to re-hash out-of-workspace path-dep
    /// manifests during memo validation. `None` tolerated for synthetic
    /// test metadata.
    #[serde(default)]
    pub(crate) manifest_path: Option<String>,
}

/// Returns `true` when `cache_profile` should be omitted from the serialized
/// plan to keep wire compatibility with zccache builds that do not yet know
/// about the `cache_profile` field (e.g. v1.4.0, which uses
/// `#[serde(deny_unknown_fields)]` on `RustArtifactPlanV1`).
///
/// We keep the value in-memory so internal consumers can still branch on it,
/// but we hide it on the wire for everything except the `thin-v2` opt-in.
fn skip_legacy_cache_profile(value: &Option<&'static str>) -> bool {
    !matches!(value, Some("thin-v2" | "thin-v3"))
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
    /// Relative target paths observed in Cargo's JSON message stream. These
    /// are written after a successful build and consumed by zccache on save.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) cargo_artifact_paths: Vec<String>,
    pub(crate) cargo_artifacts_complete: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ownership_policy: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ownership_mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) artifact_owners: Vec<RustPlanArtifactOwner>,
    pub(crate) ownership_complete: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct RustPlanArtifactOwner {
    pub(crate) relative_path: String,
    pub(crate) package_id: String,
    pub(crate) owner: &'static str,
}

pub(crate) struct RustArtifactPlanContext {
    pub(crate) path: std::path::PathBuf,
    pub(crate) cache_dir: std::path::PathBuf,
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

pub(crate) fn record_cargo_artifact_closure(
    plan_path: &std::path::Path,
    paths: &[String],
    complete: bool,
) -> Result<(), SoldrError> {
    let raw = std::fs::read(plan_path)?;
    let mut proto = rust_plan_proto::wire::RustArtifactPlanV1::decode(raw.as_slice())
        .map_err(|err| SoldrError::Other(format!("failed to decode Rust artifact plan: {err}")))?;
    proto.cargo_artifact_paths = paths.to_vec();
    proto.cargo_artifacts_complete = complete;
    let mut bytes = Vec::with_capacity(proto.encoded_len());
    proto
        .encode(&mut bytes)
        .map_err(|err| SoldrError::Other(format!("failed to encode Rust artifact plan: {err}")))?;
    std::fs::write(plan_path, bytes)?;
    Ok(())
}

pub(crate) fn maybe_prepare_rust_artifact_plan(
    cargo: &std::path::Path,
    rustc: &std::path::Path,
    args: &[String],
    session: &BuildCacheSession,
    cargo_profile_debug_default: Option<&CargoProfileDebugDefault>,
    toolchain_channel_override: Option<&str>,
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

    let plan_dir = session.cache_dir.join("plans");

    // Prep memo (soldr#1540): serve `cargo metadata` + toolchain probe
    // outputs from the versioned on-disk memo when the full content
    // identity (manifests, lock, config, toolchain binaries, env, args)
    // is provably unchanged. Any gather/load/key mismatch falls back to
    // the authoritative subprocesses below.
    let memo_context = if rust_plan_memo::prep_memo_enabled() {
        rust_plan_memo::MemoContext::gather(cargo, rustc, args).ok()
    } else {
        None
    };
    let (metadata, probe, file_hashes) = match memo_context
        .as_ref()
        .and_then(|context| context.try_load(&plan_dir))
    {
        Some(hit) => hit,
        None => {
            let metadata = cargo_metadata(cargo, args)?;
            let probe = toolchain_probe(cargo, rustc)?;
            let workspace_root = normalize_path_for_compare(&metadata.workspace_root)?;
            let file_hashes = WorkspaceFileHashes::collect(&workspace_root)?;
            if let Some(context) = memo_context.as_ref() {
                // Best-effort persist; a read-only cache dir must never
                // fail the build.
                let _ = context.store(
                    &plan_dir,
                    &metadata,
                    &probe,
                    &path_string(&workspace_root),
                    &file_hashes,
                );
            }
            (metadata, probe, file_hashes)
        }
    };
    let toolchain = derive_toolchain_identity(&probe, toolchain_channel_override);
    let plan = build_rust_artifact_plan(
        &metadata,
        &toolchain,
        args,
        &mode,
        profile,
        session,
        cargo_profile_debug_default,
        &file_hashes,
    )?;
    std::fs::create_dir_all(&plan_dir)?;
    let plan_path = plan_dir.join("last-rust-artifact-plan.pb");
    let plan_bytes = rust_plan_proto::plan_to_proto_bytes(&plan)?;
    std::fs::write(&plan_path, plan_bytes)?;

    let plan_inputs_hash = compute_plan_inputs_hash(&plan);
    let target_dir = plan.target_dir.clone();

    Ok(Some(RustArtifactPlanContext {
        path: plan_path,
        cache_dir: rust_artifact_plan_cache_dir(session)?,
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
/// is asking to restore into the same tree it just saved. The sentinel gates
/// on tree identity *separately* (via [`RustArtifactPlanContext::target_dir`]
/// alongside this hash) so the same-tree contract stays intact even though
/// this digest itself never reads `workspace_root` / `target_dir`.
///
/// We hash a tuple of (toolchain identity, target triple, profile, mode,
/// cache profile, plan inputs, package selection) rather than the whole
/// `RustArtifactPlan` so the sentinel does not falsely diverge on cosmetic
/// fields (`schema_version`, `journal_log_path`, etc.).
///
/// Deliberately excludes `workspace_root` and `target_dir` — see
/// [`compute_plan_content_identity`], which piggybacks on this property.
pub(crate) fn compute_plan_inputs_hash(plan: &RustArtifactPlan) -> String {
    stable_hash_json(&plan_content_identity_payload(plan))
}

fn plan_content_identity_payload(plan: &RustArtifactPlan) -> serde_json::Value {
    serde_json::json!({
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
            "ownership_policy": plan.packages.ownership_policy,
            "ownership_mode": plan.packages.ownership_mode,
            "artifact_owners": plan.packages.artifact_owners.iter().map(|owner| serde_json::json!({
                "relative_path": owner.relative_path,
                "package_id": owner.package_id,
                "owner": owner.owner,
            })).collect::<Vec<_>>(),
            "ownership_complete": plan.packages.ownership_complete,
        },
        "allowed_artifact_classes": plan.allowed_artifact_classes,
        "dropped_artifact_classes": plan.dropped_artifact_classes,
    })
}

/// Path-independent content identity for a rust artifact plan (issue #1539).
///
/// Two sibling worktrees checked out from the same commit — same lockfile,
/// same manifests, same toolchain, same target triple/profile/features/env/
/// rustflags — produce the SAME identity here even though their
/// `workspace_root` / `target_dir` (both absolute, both worktree-specific)
/// differ. This is intentionally a distinct, purpose-labeled function from
/// [`compute_plan_inputs_hash`] even though the two currently compute the
/// same digest: the inputs-hash contract is owned by the warm-restore
/// sentinel (same-tree, same-job proof) and must stay exactly as sensitive
/// as it is today; this identity is owned by cross-worktree comparison and
/// is free to evolve independently (e.g. if the sentinel later needs a
/// field that a cross-worktree identity must NOT be sensitive to, or
/// vice versa).
///
/// This does not, by itself, restore or share anything across worktrees —
/// it only proves when it is *safe* to consider two plans equivalent.
/// `target/`-tree contents (dep-info files, `build.rs` `OUT_DIR` bakes)
/// remain fundamentally worktree-specific and are restored per-tree via
/// the path-sensitive [`compute_plan_inputs_hash`] / warm-restore sentinel
/// path, never via this identity. The rustc-level compile cache is already
/// shared cross-worktree today through `ZCCACHE_PATH_REMAP=auto` — this
/// identity exists so a future consumer can safely recognize when that
/// sharing opportunity exists at the plan layer too, without ever treating
/// unproven or path-sensitive state as reusable.
pub(crate) fn compute_plan_content_identity(plan: &RustArtifactPlan) -> String {
    stable_hash_json(&plan_content_identity_payload(plan))
}

#[path = "rust_plan_warm_restore.rs"]
mod warm_restore;
pub(crate) use warm_restore::{
    current_unix_seconds, evaluate_warm_restore_skip, should_skip_rust_plan_save,
    should_skip_warm_restore, warm_restore_sentinel_path, warm_restore_skip_enabled,
    warm_restore_target_marker_path, write_warm_restore_sentinel, WarmRestoreSentinel,
    WarmRestoreSkipInputs, WarmRestoreTargetMarker,
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
pub(crate) fn prune_orphan_rmetas_after_failed_build(plan: &RustArtifactPlanContext) -> usize {
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
    total
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
    crate::core::flag_value(&raw.to_string_lossy())
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
    session: &BuildCacheSession,
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
    crate::binaries::apply_resolved_toolchain_homes(&mut command, cargo);
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

/// Run the two authoritative toolchain probe subprocesses (`rustc -Vv`,
/// `cargo --version`) and return their raw stdout. Parsing happens in
/// [`derive_toolchain_identity`] so the raw outputs can be memoized
/// (soldr#1540) while env-dependent fields stay live.
fn toolchain_probe(
    cargo: &std::path::Path,
    rustc: &std::path::Path,
) -> Result<ToolchainProbe, SoldrError> {
    Ok(ToolchainProbe {
        rustc_verbose: tool_output(rustc, &["-Vv"])?,
        cargo_version: tool_output(cargo, &["--version"])?,
    })
}

/// Derive the plan toolchain identity from raw probe outputs. The channel
/// prefers the live `RUSTUP_TOOLCHAIN` env var, so it is evaluated at plan
/// build time rather than persisted in the prep memo.
pub(crate) fn derive_toolchain_identity(
    probe: &ToolchainProbe,
    channel_override: Option<&str>,
) -> RustToolchainIdentity {
    let host = probe
        .rustc_verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap_or("unknown")
        .to_string();
    let channel = channel_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("RUSTUP_TOOLCHAIN")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            probe
                .rustc_verbose
                .lines()
                .find_map(|line| line.strip_prefix("release: "))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());

    RustToolchainIdentity {
        rustc: probe.rustc_verbose.trim().to_string(),
        cargo: probe.cargo_version.trim().to_string(),
        channel,
        host,
    }
}

fn tool_output(tool: &std::path::Path, args: &[&str]) -> Result<String, SoldrError> {
    let mut command = std::process::Command::new(tool);
    command.args(args);
    crate::binaries::apply_resolved_toolchain_homes(&mut command, tool);
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_rust_artifact_plan(
    metadata: &CargoMetadata,
    toolchain: &RustToolchainIdentity,
    args: &[String],
    mode: &str,
    cache_profile: Option<&'static str>,
    session: &BuildCacheSession,
    cargo_profile_debug_default: Option<&CargoProfileDebugDefault>,
    file_hashes: &WorkspaceFileHashes,
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
        Some("thin-v3") => 3,
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
            lockfile_hash: file_hashes.lockfile_hash.clone(),
            cargo_config_hash: file_hashes.cargo_config_hash.clone(),
            manifest_hashes: file_hashes.manifest_hashes.clone(),
        },
        packages: RustPlanPackages {
            selected_package_ids,
            workspace_package_ids,
            excluded_path_package_ids,
            // Cargo's JSON stream does not yet attach package ownership to
            // every fingerprint/build-script path. Select the explicit
            // zccache-all fallback until a complete cook closure is proven.
            ownership_policy: (cache_profile == Some("thin-v3"))
                .then_some("thin-v3-lifetime-partition-v1"),
            ownership_mode: (cache_profile == Some("thin-v3")).then_some("zccache-all-v1"),
            artifact_owners: Vec::new(),
            ownership_complete: false,
        },
        allowed_artifact_classes: allowed,
        dropped_artifact_classes: dropped,
        cargo_artifact_paths: Vec::new(),
        cargo_artifacts_complete: false,
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
            // Cargo JSON hydration makes these verified primary outputs
            // available to zccache; incomplete streams still fall back to
            // the drop list below and retain the old thin-v2 behavior.
            "rlib",
            "rmeta",
            "proc_macro",
            "shared_lib",
            "build_script_build",
            "cargo_fingerprint_outputs",
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
            // soldr#1579: the compiled build-script binary itself was
            // missing from this allowlist. Units whose `build.rs` output
            // feeds their own compilation saw cargo's fingerprint check
            // treat the (dropped) `build_script_build` artifact as stale,
            // cascading a `StaleDepFingerprint` rebuild through everything
            // downstream of that build script even though the rest of the
            // thin-v1 slice (rlib/rmeta/dep_info) was retained.
            "build_script_build",
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

include!("rust_plan_execute.rs");
include!("rust_plan_bundle.rs");
#[cfg(test)]
#[path = "rust_plan_tests/mod.rs"]
mod tests;
