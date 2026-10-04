//! Retired embedded-store usage in daemon maintenance status.

use super::{MaintenanceContext, MaintenanceStatus};

/// Read-only; no maintenance lease is needed.
pub(super) fn measure_retired(context: &MaintenanceContext, status: &mut MaintenanceStatus) {
    let embedded_root = crate::zccache_embedded::embedded_cache_root(&context.paths);
    let current = embedded_root.join(zccache::core::config::versioned_subdir());
    let mut usage = crate::zccache_embedded::RetiredStoresUsage::default();
    let daemon_state = context.paths.cache.join("zccache/daemon-state");
    if let Ok(entries) = std::fs::read_dir(daemon_state) {
        for entry in entries.flatten() {
            let path = entry.path();
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("embedded-v1"))
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
                && crate::cache_lib::path_safety::validate_owned_directory(
                    &context.paths.root,
                    &path,
                )
                .is_ok()
            {
                let retired = crate::zccache_embedded::measure_retired_stores(&path, &current);
                usage.stores = usage.stores.saturating_add(retired.stores);
                usage.bytes = usage.bytes.saturating_add(retired.bytes);
            }
        }
    }
    status.retired_store_bytes = Some(usage.bytes);
    status.retired_store_count = Some(usage.stores);
}
