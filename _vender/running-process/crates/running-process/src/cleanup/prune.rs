use std::collections::HashMap;
use std::path::Path;

use crate::broker::manifest;
use crate::broker::protocol::CacheManifest;
use crate::cleanup::{delete_path, now_unix_ms, root_is_prunable, CleanupAction, CleanupError};

/// Prune selection options.
#[derive(Debug, Clone)]
pub struct PruneOptions {
    /// Only prune manifests inactive for at least this many seconds.
    pub dormant_after_secs: Option<u64>,
    /// Keep manifests with a current daemon.
    pub keep_current: bool,
    /// Keep the N most recently active manifests per service.
    pub keep_last: Option<usize>,
    /// Restrict to one service.
    pub service: Option<String>,
    /// Restrict to one version.
    pub version: Option<String>,
    /// Delete selected paths. False means dry-run.
    pub confirm: bool,
}

/// Select and optionally delete prunable roots.
pub fn run(
    registry_dir: &Path,
    options: &PruneOptions,
) -> Result<Vec<CleanupAction>, CleanupError> {
    let manifests = manifest::enumerate_central(registry_dir);
    let keep_last = keep_last_keys(&manifests, options.keep_last);
    let now_ms = now_unix_ms();
    let mut actions = Vec::new();

    for manifest in manifests {
        if let Some(service) = &options.service {
            if &manifest.service_name != service {
                continue;
            }
        }
        if let Some(version) = &options.version {
            if &manifest.service_version != version {
                continue;
            }
        }
        let key = manifest_key(&manifest);
        if keep_last.contains_key(&key) {
            continue;
        }
        if options.keep_current && manifest.current_daemon.is_some() {
            continue;
        }
        if let Some(dormant_after_secs) = options.dormant_after_secs {
            let dormant_ms = dormant_after_secs.saturating_mul(1000);
            if now_ms.saturating_sub(manifest.last_active_unix_ms) < dormant_ms {
                continue;
            }
        }

        for root in &manifest.roots {
            let path = std::path::PathBuf::from(&root.path);
            if !root_is_prunable(root) {
                actions.push(CleanupAction {
                    service_name: manifest.service_name.clone(),
                    service_version: manifest.service_version.clone(),
                    path,
                    reason: "prune".to_string(),
                    deleted: false,
                    skipped: true,
                    skip_reason: Some("root disposition is not prunable".to_string()),
                });
                continue;
            }
            if options.confirm {
                delete_path(&path)?;
            }
            actions.push(CleanupAction {
                service_name: manifest.service_name.clone(),
                service_version: manifest.service_version.clone(),
                path,
                reason: if options.confirm {
                    "prune-confirmed".to_string()
                } else {
                    "prune-dry-run".to_string()
                },
                deleted: options.confirm,
                skipped: false,
                skip_reason: None,
            });
        }
    }

    Ok(actions)
}

fn keep_last_keys(manifests: &[CacheManifest], keep_last: Option<usize>) -> HashMap<String, ()> {
    let Some(limit) = keep_last else {
        return HashMap::new();
    };
    let mut by_service: HashMap<&str, Vec<&CacheManifest>> = HashMap::new();
    for manifest in manifests {
        by_service
            .entry(&manifest.service_name)
            .or_default()
            .push(manifest);
    }
    let mut out = HashMap::new();
    for manifests in by_service.values_mut() {
        manifests.sort_by_key(|m| std::cmp::Reverse(m.last_active_unix_ms));
        for manifest in manifests.iter().take(limit) {
            out.insert(manifest_key(manifest), ());
        }
    }
    out
}

fn manifest_key(manifest: &CacheManifest) -> String {
    format!("{}@{}", manifest.service_name, manifest.service_version)
}
