//! Warm-restore-skip sentinel for rust-plan save/restore.
//! Extracted from `rust_plan.rs` to keep that file under the soldr-wide
//! 1K LOC budget enforced by `loc_guard.py`. Re-exported from the parent
//! module so callers keep using the same path.
//!
//! The sentinel is written next to the thin-slice bundle root after a
//! successful `rust-plan save`. The next `soldr cargo ...` invocation
//! reads it via [`should_skip_warm_restore`] / [`evaluate_warm_restore_skip`]
//! to decide whether a matching `restore` would be a no-op-but-touches-mtimes
//! operation against an already-warm `target/` tree.

use crate::core::SoldrError;
use crate::{
    SKIP_WARM_RESTORE_ENV_VAR, WARM_RESTORE_MAX_AGE_SECONDS, WARM_RESTORE_SENTINEL_FILENAME,
};
use serde::{Deserialize, Serialize};

use super::{compute_plan_inputs_hash, RustArtifactPlanContext};

/// Sentinel written by `soldr cargo ...` after a successful `rust-plan save`.
/// Read on the next invocation by [`should_skip_warm_restore`] to decide
/// whether the matching `restore` would be a no-op-but-touches-mtimes
/// operation against an already-warm `target/` tree.
///
/// All fields are required for a sentinel match; missing fields cause the
/// short-circuit to bail out and fall through to the normal restore.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WarmRestoreSentinel {
    /// Format version; bump if any field semantics change so older sentinels
    /// are treated as stale.
    pub(crate) schema_version: u32,
    /// Hash from [`compute_plan_inputs_hash`]. Must match the current plan.
    pub(crate) plan_inputs_hash: String,
    /// Absolute target dir the previous save wrote into. Must match the
    /// current plan's target dir, since restore is per-target-dir.
    pub(crate) target_dir: String,
    /// `GITHUB_RUN_ID` at save time. Empty string outside Actions; matched
    /// strictly so the short-circuit does not leak between runs.
    pub(crate) github_run_id: String,
    /// `GITHUB_JOB` at save time. Same matching rule as run id.
    pub(crate) github_job: String,
    /// `GITHUB_RUN_ATTEMPT` at save time. Re-runs of the same job get a new
    /// attempt id, so a prior attempt's sentinel is correctly treated as
    /// stale.
    pub(crate) github_run_attempt: String,
    /// zccache session id from the saving invocation. Recorded for log
    /// correlation; not used in the match decision.
    pub(crate) session_id: String,
    /// Wall-clock seconds since the unix epoch at save time. Compared
    /// against [`WARM_RESTORE_MAX_AGE_SECONDS`] to bound how stale the
    /// sentinel may be.
    pub(crate) saved_at_unix_seconds: u64,
}

/// Returns the path of the warm-restore sentinel for this plan's bundle dir.
pub(crate) fn warm_restore_sentinel_path(plan: &RustArtifactPlanContext) -> std::path::PathBuf {
    plan.cache_dir.join(WARM_RESTORE_SENTINEL_FILENAME)
}

/// Returns whether the warm-restore short-circuit is enabled for this
/// invocation. The flag is default-on after #229 validation completed:
///
/// - Unset → `true` (default-on).
/// - Set to a falsy value (`0` / `false` / `no` / `off` / empty,
///   case-insensitive after trimming) → `false` (explicit opt-out).
/// - Set to any other value, including the historical truthy values
///   (`1` / `true` / `yes` / `on`) → `true`. Unrecognised values are
///   tolerated as "enabled" rather than silently disabling the feature.
pub(crate) fn warm_restore_skip_enabled() -> bool {
    match std::env::var(SKIP_WARM_RESTORE_ENV_VAR) {
        Ok(value) => {
            let trimmed = value.trim().to_ascii_lowercase();
            !matches!(trimmed.as_str(), "0" | "false" | "no" | "off" | "")
        }
        Err(_) => true,
    }
}

/// Read `name` from the environment, returning an empty string when absent
/// or the value cannot be UTF-8 decoded. Used to canonicalise the GitHub
/// Actions identifiers stored in the warm-restore sentinel so a
/// missing-vs-empty distinction does not produce false negatives.
fn env_string_or_empty(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// Returns the current wall-clock as seconds since the unix epoch, or `0`
/// if the system clock is before the epoch (which would only happen on a
/// badly-misconfigured host).
pub(crate) fn current_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Decide whether the current invocation can skip `rust-plan restore`
/// because a previous invocation in the same CI job already populated
/// `target/` with the exact contents restore would produce.
///
/// Returns `Some(reason)` when the restore should be skipped (caller
/// should log `reason` for operator visibility). Returns `None` when the
/// caller should proceed with the normal restore — either because the
/// short-circuit is disabled, the sentinel is missing/stale, or any of
/// the match fields disagree.
///
/// Pure function over its inputs so it can be unit-tested without touching
/// the filesystem; the IO-bound caller passes the loaded sentinel and
/// current env snapshot in.
/// Bundle of "current invocation" inputs that
/// [`evaluate_warm_restore_skip`] compares against a loaded sentinel.
///
/// Internal-only — exists solely to keep the function's argument count
/// under clippy's `too_many_arguments` threshold without changing
/// behavior. Field names mirror the previous parameter names so call
/// sites stay legible.
pub(crate) struct WarmRestoreSkipInputs<'a> {
    pub(crate) plan_inputs_hash: &'a str,
    pub(crate) plan_target_dir: &'a str,
    pub(crate) github_run_id: &'a str,
    pub(crate) github_job: &'a str,
    pub(crate) github_run_attempt: &'a str,
    pub(crate) now_unix_seconds: u64,
    pub(crate) max_age_seconds: u64,
}

pub(crate) fn evaluate_warm_restore_skip(
    sentinel: Option<&WarmRestoreSentinel>,
    inputs: &WarmRestoreSkipInputs<'_>,
) -> Option<String> {
    let sentinel = sentinel?;
    if sentinel.schema_version != 1 {
        return None;
    }
    if sentinel.plan_inputs_hash != inputs.plan_inputs_hash {
        return None;
    }
    if sentinel.target_dir != inputs.plan_target_dir {
        return None;
    }
    // CI scoping: only short-circuit when both invocations are inside the
    // same GitHub Actions run + job + attempt. Locally these are all empty
    // strings on both sides, which still matches and lets the local repro
    // benefit from the same path. The intent of the issue, however, is the
    // CI case — hence the explicit attempt scoping.
    if sentinel.github_run_id != inputs.github_run_id {
        return None;
    }
    if sentinel.github_job != inputs.github_job {
        return None;
    }
    if sentinel.github_run_attempt != inputs.github_run_attempt {
        return None;
    }
    let age = inputs
        .now_unix_seconds
        .saturating_sub(sentinel.saved_at_unix_seconds);
    if age > inputs.max_age_seconds {
        return None;
    }
    Some(format!(
        "soldr: skipping rust-plan restore; target dir {} was warmed by this job {} seconds ago (session {})",
        sentinel.target_dir, age, sentinel.session_id,
    ))
}

/// Filesystem-backed wrapper around [`evaluate_warm_restore_skip`]. Reads
/// the sentinel for this plan's bundle dir, gathers the current env, and
/// returns the skip reason when the short-circuit should fire.
///
/// Errors from sentinel parsing are deliberately swallowed (return `None`)
/// — a corrupt sentinel must never break the build, only forfeit the
/// optimisation.
pub(crate) fn should_skip_warm_restore(plan: &RustArtifactPlanContext) -> Option<String> {
    if !warm_restore_skip_enabled() {
        return None;
    }
    let sentinel_path = warm_restore_sentinel_path(plan);
    let raw = std::fs::read_to_string(&sentinel_path).ok()?;
    let sentinel: WarmRestoreSentinel = serde_json::from_str(&raw).ok()?;
    let github_run_id = env_string_or_empty("GITHUB_RUN_ID");
    let github_job = env_string_or_empty("GITHUB_JOB");
    let github_run_attempt = env_string_or_empty("GITHUB_RUN_ATTEMPT");
    let inputs = WarmRestoreSkipInputs {
        plan_inputs_hash: &plan.plan_inputs_hash,
        plan_target_dir: &plan.target_dir,
        github_run_id: &github_run_id,
        github_job: &github_job,
        github_run_attempt: &github_run_attempt,
        now_unix_seconds: current_unix_seconds(),
        max_age_seconds: WARM_RESTORE_MAX_AGE_SECONDS,
    };
    evaluate_warm_restore_skip(Some(&sentinel), &inputs)
}

/// Persist the warm-restore sentinel after a successful `rust-plan save`.
/// Errors are downgraded to a warning so a sentinel write failure can never
/// break the build that just succeeded; the worst case is that the next
/// invocation does the normal restore (current behavior).
pub(crate) fn write_warm_restore_sentinel(plan: &RustArtifactPlanContext) {
    if !warm_restore_skip_enabled() {
        return;
    }
    let sentinel = WarmRestoreSentinel {
        schema_version: 1,
        plan_inputs_hash: plan.plan_inputs_hash.clone(),
        target_dir: plan.target_dir.clone(),
        github_run_id: env_string_or_empty("GITHUB_RUN_ID"),
        github_job: env_string_or_empty("GITHUB_JOB"),
        github_run_attempt: env_string_or_empty("GITHUB_RUN_ATTEMPT"),
        session_id: plan.session_id.clone(),
        saved_at_unix_seconds: current_unix_seconds(),
    };
    let sentinel_path = warm_restore_sentinel_path(plan);
    let json = match serde_json::to_string_pretty(&sentinel) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("soldr warning: failed to serialize warm-restore sentinel: {e}");
            return;
        }
    };
    if let Some(parent) = sentinel_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!(
                "soldr warning: failed to create warm-restore sentinel dir {}: {e}",
                parent.display()
            );
            return;
        }
    }
    if let Err(e) = std::fs::write(&sentinel_path, json) {
        eprintln!(
            "soldr warning: failed to write warm-restore sentinel at {}: {e}",
            sentinel_path.display()
        );
    }
}
