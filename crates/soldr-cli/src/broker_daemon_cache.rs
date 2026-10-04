//! Broker cleanup of private zccache stores after daemon route retirement.
//!
//! Service definitions persist the route's Soldr root. They are the durable
//! retirement registration: a restarted broker can find idle routes again.
//! The embedded writer lock remains the final authority before deletion.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use prost::Message as _;
use running_process::broker::protocol_v2::{ServiceDefinition, SERVICE_DEF_V2_EXTENSION};

use crate::core::SoldrPaths;
use crate::daemon::service_definition::SOLDR_ROOT_SERVICE_LABEL;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RouteCacheSweepReport {
    pub(crate) stores_removed: usize,
    pub(crate) bytes_reclaimed: u64,
    pub(crate) stores_live: usize,
    pub(crate) failed: usize,
    pub(crate) pending: BTreeSet<String>,
}

pub(crate) fn sweep_retired_route_caches(
    services_root: &Path,
    live: &BTreeSet<String>,
    now: SystemTime,
) -> RouteCacheSweepReport {
    let mut report = RouteCacheSweepReport::default();
    let Ok(entries) = std::fs::read_dir(services_root) else {
        return report;
    };
    for entry in entries.flatten() {
        let Some(service) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(SERVICE_DEF_V2_EXTENSION))
            .and_then(|name| name.strip_suffix('.'))
            .map(str::to_owned)
        else {
            continue;
        };
        if !valid_route_name(&service) || live.contains(&service) {
            continue;
        }
        // A front door renews this file before asking the broker to launch.
        // Let that request finish its daemon startup even if it landed after
        // the broker took the live-set snapshot for this pass.
        let recently_registered = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .is_none_or(|modified| {
                now.duration_since(modified).unwrap_or_default()
                    < crate::broker_reaper::DEFAULT_GRACE
            });
        if recently_registered {
            continue;
        }
        let Some(root) = registered_root(&entry.path(), &service) else {
            continue;
        };
        let paths = SoldrPaths::with_root(root);
        let private_root = paths
            .cache
            .join("zccache/daemon-state")
            .join(format!("embedded-v1-{service}"));
        if !private_root.exists() {
            continue;
        }
        if crate::cache_lib::path_safety::validate_owned_directory(&paths.root, &private_root)
            .is_err()
        {
            report.failed += 1;
            report.pending.insert(service);
            continue;
        }
        let Ok(stores) = std::fs::read_dir(&private_root) else {
            report.failed += 1;
            report.pending.insert(service);
            continue;
        };
        for store in stores.flatten() {
            if !store.file_type().is_ok_and(|kind| kind.is_dir())
                || !store
                    .file_name()
                    .to_str()
                    .is_some_and(zccache::core::config::is_version_dir_name)
            {
                continue;
            }
            let swept = zccache::core::config::sweep_retired_version_store_with_mode(
                &store.path(),
                Duration::ZERO,
                now,
                zccache::core::config::RetiredSweepMode::Pressure,
            );
            report.stores_removed += swept.stores_removed;
            report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(swept.bytes_reclaimed);
            report.stores_live += swept.stores_live;
            report.failed += swept.failed;
        }
        if std::fs::read_dir(&private_root).is_ok_and(|mut stores| {
            stores.any(|store| {
                store.is_ok_and(|store| {
                    store.file_type().is_ok_and(|kind| kind.is_dir())
                        && store
                            .file_name()
                            .to_str()
                            .is_some_and(zccache::core::config::is_version_dir_name)
                })
            })
        }) {
            report.pending.insert(service);
        }
    }
    report
}

fn valid_route_name(service: &str) -> bool {
    service
        .strip_prefix(crate::broker_daemon_disk::DAEMON_SERVICE_PREFIX)
        .is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn registered_root(path: &Path, service: &str) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let definition = ServiceDefinition::decode(bytes.as_slice()).ok()?;
    if definition.service_name != service
        || definition.labels.get("package").map(String::as_str) != Some("soldr")
    {
        return None;
    }
    let root = definition.labels.get(SOLDR_ROOT_SERVICE_LABEL)?;
    let root = PathBuf::from(root);
    root.is_absolute().then_some(root)
}

#[cfg(test)]
#[path = "broker_daemon_cache_tests.rs"]
mod tests;
