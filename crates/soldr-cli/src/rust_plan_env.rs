//! Env-var driven cache mode / profile / backend / tar-thread resolvers
//! used by `rust_plan` to shape the artifact-plan request. Extracted to
//! keep `rust_plan.rs` under the soldr-wide 1K LOC budget.
//!
//! All functions are pure over the env vars they read; nothing here
//! touches disk or spawns processes. The result is then plumbed into
//! `RustArtifactPlan` by `maybe_prepare_rust_artifact_plan`.

use crate::core::SoldrError;
use crate::{
    TARGET_CACHE_BACKEND_ENV_VAR, TARGET_CACHE_MODE_ENV_VAR, TARGET_CACHE_PROFILE_ENV_VAR,
    TARGET_CACHE_TAR_THREADS_ENV_VAR,
};

pub(super) fn rust_artifact_cache_mode_from_env() -> Result<Option<String>, SoldrError> {
    let raw = std::env::var(TARGET_CACHE_MODE_ENV_VAR).unwrap_or_default();
    let mode = raw.trim().to_ascii_lowercase();
    match mode.as_str() {
        "" | "off" | "false" | "0" | "no" => Ok(None),
        "hot" | "thin" => Ok(Some("thin".to_string())),
        "full" => Ok(Some("full".to_string())),
        _ => Err(SoldrError::Other(format!(
            "invalid {TARGET_CACHE_MODE_ENV_VAR} value {raw:?}; expected thin, full, or off"
        ))),
    }
}

pub(crate) fn rust_artifact_cache_profile_from_env() -> Result<&'static str, SoldrError> {
    let raw = std::env::var(TARGET_CACHE_PROFILE_ENV_VAR).unwrap_or_default();
    let profile = raw.trim().to_ascii_lowercase();
    match profile.as_str() {
        // Default is the fingerprint-aware prune (issue #461). Requires
        // managed zccache >= 1.9.1 to honor `dropped_artifact_classes`
        // and the `cargo_fingerprint_meta` / `cargo_fingerprint_outputs`
        // split. `thin-v1` is preserved as an explicit opt-out for users
        // pinned to older zccache.
        "" | "thin-v2" => Ok("thin-v2"),
        "thin-v1" => Ok("thin-v1"),
        _ => Err(SoldrError::Other(format!(
            "invalid {TARGET_CACHE_PROFILE_ENV_VAR} value {raw:?}; expected thin-v1 or thin-v2"
        ))),
    }
}

pub(super) fn rust_artifact_cache_backend_from_env() -> Result<String, SoldrError> {
    let raw = std::env::var(TARGET_CACHE_BACKEND_ENV_VAR).unwrap_or_else(|_| "auto".to_string());
    let backend = raw.trim().to_ascii_lowercase();
    match backend.as_str() {
        "" | "auto" => Ok("auto".to_string()),
        "local" => Ok("local".to_string()),
        "gha" => Ok("gha".to_string()),
        _ => Err(SoldrError::Other(format!(
            "invalid {TARGET_CACHE_BACKEND_ENV_VAR} value {raw:?}; expected auto, local, or gha"
        ))),
    }
}

pub(super) fn rust_artifact_cache_tar_threads_from_env() -> Result<Option<String>, SoldrError> {
    parse_rust_artifact_cache_tar_threads(
        &std::env::var(TARGET_CACHE_TAR_THREADS_ENV_VAR).unwrap_or_default(),
    )
}

pub(crate) fn parse_rust_artifact_cache_tar_threads(
    raw: &str,
) -> Result<Option<String>, SoldrError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.eq_ignore_ascii_case("auto") {
        return Ok(Some("auto".to_string()));
    }
    match trimmed.parse::<u32>() {
        Ok(n) if n >= 1 => Ok(Some(n.to_string())),
        _ => Err(SoldrError::Other(format!(
            "invalid {TARGET_CACHE_TAR_THREADS_ENV_VAR} value {raw:?}; expected `auto` or a positive integer (use `1` to disable parallelism)"
        ))),
    }
}

