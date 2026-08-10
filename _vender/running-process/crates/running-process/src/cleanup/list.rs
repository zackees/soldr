use std::path::Path;

use crate::broker::manifest;
use crate::broker::protocol::CacheManifest;

/// Return parseable, current-host manifests from a registry.
pub fn list(registry_dir: &Path) -> Vec<CacheManifest> {
    manifest::enumerate_central(registry_dir)
}

/// Render `running-process-cleanup list --json`.
pub fn render_json(manifests: &[CacheManifest]) -> String {
    let body = manifests
        .iter()
        .map(crate::cleanup::manifest_json)
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"schema_version\":1,\"manifests\":[{body}]}}")
}
