use std::path::Path;

use crate::broker::manifest;
use crate::cleanup::{delete_path, root_is_config, root_is_prunable, CleanupAction, CleanupError};

/// Uninstall all manifest-declared roots for one service.
pub fn run(
    registry_dir: &Path,
    service: &str,
    keep_config: bool,
    confirm: bool,
) -> Result<Vec<CleanupAction>, CleanupError> {
    let manifests = manifest::enumerate_central(registry_dir);
    let mut actions = Vec::new();

    for manifest in manifests {
        if manifest.service_name != service {
            continue;
        }
        for root in &manifest.roots {
            let path = std::path::PathBuf::from(&root.path);
            if keep_config && root_is_config(root) {
                actions.push(CleanupAction {
                    service_name: manifest.service_name.clone(),
                    service_version: manifest.service_version.clone(),
                    path,
                    reason: "uninstall".to_string(),
                    deleted: false,
                    skipped: true,
                    skip_reason: Some("CACHE_CONFIG preserved by --keep-config".to_string()),
                });
                continue;
            }
            if !root_is_prunable(root) {
                actions.push(CleanupAction {
                    service_name: manifest.service_name.clone(),
                    service_version: manifest.service_version.clone(),
                    path,
                    reason: "uninstall".to_string(),
                    deleted: false,
                    skipped: true,
                    skip_reason: Some("root disposition is not prunable".to_string()),
                });
                continue;
            }
            if confirm {
                delete_path(&path)?;
            }
            actions.push(CleanupAction {
                service_name: manifest.service_name.clone(),
                service_version: manifest.service_version.clone(),
                path,
                reason: if confirm {
                    "uninstall-confirmed".to_string()
                } else {
                    "uninstall-dry-run".to_string()
                },
                deleted: confirm,
                skipped: false,
                skip_reason: None,
            });
        }
    }

    Ok(actions)
}
