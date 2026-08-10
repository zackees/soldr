//! Standalone cleanup support for v1 broker CacheManifest files.
//!
//! Phase 2 of #228 (#231). This module intentionally operates only on
//! the central manifest registry; it does not require the broker or any
//! originating daemon to be running.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::broker::protocol::{CacheManifest, CacheRoot, CacheRootKind, StorageDisposition};

/// Inspection helpers for broker cleanup instance metadata.
pub mod instances;
/// Read-only cleanup listing operations.
pub mod list;
/// Pruning operations for removable cache roots.
pub mod prune;
/// Uninstall cleanup operations for service-owned roots.
pub mod uninstall;
/// Exhaustive daemon-artifact reconciliation for `cleanup verify` (#391).
pub mod verify_artifacts;
/// Basic cleanup verification helpers.
pub mod verify_basic;

/// A filesystem action planned or executed by cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupAction {
    /// Manifest service name.
    pub service_name: String,
    /// Manifest service version.
    pub service_version: String,
    /// Root path affected by the action.
    pub path: PathBuf,
    /// Why the path was selected.
    pub reason: String,
    /// Whether the path was deleted.
    pub deleted: bool,
    /// Whether the path was skipped.
    pub skipped: bool,
    /// Skip reason when `skipped` is true.
    pub skip_reason: Option<String>,
}

/// Shared cleanup error type.
#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
    /// Manifest-layer error.
    #[error(transparent)]
    Manifest(#[from] crate::broker::manifest::ManifestError),
    /// Filesystem operation failed.
    #[error("cleanup I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// User supplied an invalid argument.
    #[error("{0}")]
    User(String),
}

/// Current wall-clock time as Unix milliseconds.
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse simple duration strings such as `30d`, `12h`, `10m`, `45s`.
pub fn parse_duration_secs(input: &str) -> Result<u64, CleanupError> {
    if input.is_empty() {
        return Err(CleanupError::User("duration must not be empty".into()));
    }
    let (digits, suffix) = input.split_at(input.len() - 1);
    let value: u64 = digits
        .parse()
        .map_err(|_| CleanupError::User(format!("invalid duration: {input}")))?;
    match suffix {
        "d" => Ok(value * 24 * 60 * 60),
        "h" => Ok(value * 60 * 60),
        "m" => Ok(value * 60),
        "s" => Ok(value),
        _ => Err(CleanupError::User(format!(
            "duration must end with d, h, m, or s: {input}"
        ))),
    }
}

pub(crate) fn root_disposition(root: &CacheRoot) -> i32 {
    root.disposition
}

pub(crate) fn root_kind(root: &CacheRoot) -> i32 {
    root.kind
}

pub(crate) fn root_is_config(root: &CacheRoot) -> bool {
    root_kind(root) == CacheRootKind::CacheConfig as i32
}

pub(crate) fn root_is_prunable(root: &CacheRoot) -> bool {
    !matches!(
        root_disposition(root),
        x if x == StorageDisposition::NeverPrune as i32
            || x == StorageDisposition::PreserveAcrossUninstall as i32
    )
}

pub(crate) fn delete_path(path: &Path) -> Result<(), CleanupError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(CleanupError::Io(err)),
    }
    Ok(())
}

pub(crate) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn manifest_json(manifest: &CacheManifest) -> String {
    let roots = manifest
        .roots
        .iter()
        .map(|root| {
            format!(
                "{{\"path\":\"{}\",\"kind\":{},\"disposition\":{},\"estimated_size_bytes\":{}}}",
                json_escape(&root.path),
                root.kind,
                root.disposition,
                root.estimated_size_bytes
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"service_name\":\"{}\",\"service_version\":\"{}\",\"broker_instance\":\"{}\",\"last_active_unix_ms\":{},\"roots\":[{}]}}",
        json_escape(&manifest.service_name),
        json_escape(&manifest.service_version),
        json_escape(&manifest.broker_instance),
        manifest.last_active_unix_ms,
        roots
    )
}

/// Serialize cleanup actions to the CLI JSON response envelope.
pub fn actions_json(schema_version: u32, actions: &[CleanupAction]) -> String {
    let actions = actions
        .iter()
        .map(|action| {
            format!(
                "{{\"service_name\":\"{}\",\"service_version\":\"{}\",\"path\":\"{}\",\"reason\":\"{}\",\"deleted\":{},\"skipped\":{},\"skip_reason\":{}}}",
                json_escape(&action.service_name),
                json_escape(&action.service_version),
                json_escape(&action.path.to_string_lossy()),
                json_escape(&action.reason),
                action.deleted,
                action.skipped,
                match &action.skip_reason {
                    Some(reason) => format!("\"{}\"", json_escape(reason)),
                    None => "null".to_string(),
                }
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"schema_version\":{schema_version},\"actions\":[{actions}]}}")
}
