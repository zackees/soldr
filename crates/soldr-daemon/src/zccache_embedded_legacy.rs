//! Legacy embedded zccache cache-root migration and sweep (extracted
//! from zccache_embedded.rs to keep that file under the 1000-LOC
//! budget).

use super::*;

/// Re-home the pre-#1651 backend directory into the stable namespace.
///
/// The legacy identity hashed `(SoldrPaths::root, current_exe path)`. For a
/// normal in-place upgrade we can derive that exact path and prefer it even
/// when stale sibling identities exist. A cache restored to a different root
/// cannot recover the old root string, so it selects the uniquely most-recent
/// legacy backend instead. `soldr save` flushes the active backend immediately
/// before archiving and preserves nanosecond mtimes, making that ordering
/// durable across load. A tied newest mtime is rejected rather than silently
/// starting with an arbitrary cold cache.
pub(crate) fn migrate_legacy_cache_root(
    paths: &SoldrPaths,
    daemon_identity: &DaemonProcess,
    stable_root: &std::path::Path,
) -> Result<(), EmbeddedServiceError> {
    if stable_root.exists() {
        return Ok(());
    }

    let parent = stable_root
        .parent()
        .expect("private zccache cache root always has a parent");
    if !parent.exists() {
        return Ok(());
    }
    crate::cache_lib::path_safety::validate_owned_directory(&paths.root, parent)?;

    let exact_legacy = private_zccache_cache_root(
        paths,
        &derive_legacy_identity(paths, &daemon_identity.exe_path),
    );
    if std::fs::symlink_metadata(&exact_legacy).is_ok_and(|metadata| {
        metadata.is_dir() && !crate::cache_lib::path_safety::is_link_or_reparse(&metadata)
    }) {
        std::fs::rename(&exact_legacy, stable_root)?;
        tracing::info!(
            from = %exact_legacy.display(),
            to = %stable_root.display(),
            "migrated exact legacy embedded zccache backend"
        );
        return Ok(());
    }

    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir()
            && !crate::cache_lib::path_safety::is_link_or_reparse(&metadata)
            && is_legacy_identity_name(&entry.file_name())
        {
            candidates.push((latest_tree_mtime(&entry.path())?, entry.path()));
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let selected = select_legacy_candidate(parent, candidates)?;
    std::fs::rename(&selected, stable_root)?;
    tracing::warn!(
        from = %selected.display(),
        to = %stable_root.display(),
        "migrated most recently flushed legacy embedded zccache backend from a relocated cache"
    );
    Ok(())
}

pub(super) fn select_legacy_candidate(
    parent: &std::path::Path,
    mut candidates: Vec<(SystemTime, PathBuf)>,
) -> Result<PathBuf, EmbeddedServiceError> {
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    if candidates.len() > 1 && candidates[0].0 == candidates[1].0 {
        let newest = candidates[0].0;
        return Err(EmbeddedServiceError::AmbiguousLegacyCache {
            root: parent.to_path_buf(),
            candidates: candidates
                .into_iter()
                .take_while(|(mtime, _)| *mtime == newest)
                .map(|(_, path)| path)
                .collect(),
        });
    }
    Ok(candidates
        .into_iter()
        .next()
        .expect("caller rejects an empty legacy candidate list")
        .1)
}

pub(super) fn derive_legacy_identity(
    paths: &SoldrPaths,
    exe_path: &std::path::Path,
) -> HostIdentity {
    let mut hasher = StreamHasher::new();
    hasher.update(paths.root.as_os_str().to_string_lossy().as_bytes());
    hasher.update(exe_path.as_os_str().to_string_lossy().as_bytes());
    let id = hex::encode(&hasher.finalize().as_bytes()[..16]);
    HostIdentity {
        product: "soldr".to_string(),
        instance_id: id.clone(),
        workspace_id: id,
    }
}

fn is_legacy_identity_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn latest_tree_mtime(root: &std::path::Path) -> Result<SystemTime, std::io::Error> {
    let root_metadata = std::fs::symlink_metadata(root)?;
    if crate::cache_lib::path_safety::is_link_or_reparse(&root_metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("linked cache tree retained: {}", root.display()),
        ));
    }
    let mut newest = root_metadata.modified()?;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("linked cache entry retained: {}", entry.path().display()),
                ));
            }
            let modified = metadata.modified()?;
            newest = newest.max(modified);
            if metadata.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(newest)
}

/// Reclaim stale soldr-owned embedded generations beneath exactly one selected
/// product root.  The active stable identity and current version are always
/// protected; links and every sibling product root are ignored.
pub fn sweep_legacy_cache_roots(
    paths: &SoldrPaths,
    now: SystemTime,
    max_age: std::time::Duration,
) -> LegacyCacheSweepReport {
    let zccache_root = paths.cache.join("zccache");
    let daemon_state = zccache_root.join("daemon-state");
    let embedded_root = daemon_state.join("embedded-v1");
    let current_version = zccache::core::config::versioned_subdir();
    let mut report = LegacyCacheSweepReport::default();
    if !zccache_root.exists() {
        return report;
    }
    for root in [&zccache_root, &daemon_state, &embedded_root] {
        if root.exists()
            && crate::cache_lib::path_safety::validate_owned_directory(&paths.root, root).is_err()
        {
            report.failed += 1;
            return report;
        }
    }
    let mut candidates = Vec::new();
    if daemon_state.exists() {
        match std::fs::read_dir(&daemon_state) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) if is_legacy_identity_name(&entry.file_name()) => {
                            candidates.push(entry.path());
                        }
                        Ok(_) => {}
                        Err(_) => report.failed += 1,
                    }
                }
            }
            Err(_) => report.failed += 1,
        }
    }
    for (root, protect_current) in [(&zccache_root, false), (&embedded_root, true)] {
        if !root.exists() {
            continue;
        }
        match std::fs::read_dir(root) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => {
                            let name = entry.file_name();
                            if name.to_str().is_some_and(|name| {
                                zccache::core::config::is_version_dir_name(name)
                                    && (!protect_current || name != current_version)
                            }) {
                                candidates.push(entry.path());
                            }
                        }
                        Err(_) => report.failed += 1,
                    }
                }
            }
            Err(_) => report.failed += 1,
        }
    }

    for path in candidates {
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            report.failed += 1;
            continue;
        };
        if !metadata.is_dir() || crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
            report.failed += 1;
            continue;
        }
        let Ok(modified) = latest_tree_mtime(&path) else {
            report.failed += 1;
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() < max_age {
            continue;
        }
        let bytes = crate::cache_lib::target_registry::directory_size(&path);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                report.removed += 1;
                report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(bytes);
            }
            Err(_) => report.failed += 1,
        }
    }
    report
}

#[cfg(test)]
mod legacy_gc_tests {
    use super::*;

    #[test]
    fn legacy_sweep_protects_current_and_sibling_roots() {
        let temp = tempfile::tempdir().unwrap();
        let owned = SoldrPaths::with_root(temp.path().join(".soldr"));
        let sibling = SoldrPaths::with_root(temp.path().join(".soldr-dev"));
        let legacy = owned
            .cache
            .join("zccache/daemon-state/0123456789abcdef0123456789abcdef");
        let embedded = owned.cache.join("zccache/daemon-state/embedded-v1");
        let current = embedded.join(zccache::core::config::versioned_subdir());
        let nested_old_version = embedded.join("v0.0.1");
        let top_old_version = owned.cache.join("zccache/v0.0.2");
        // Top-level versions belong to the removed standalone/legacy layout,
        // even when their version text happens to equal the embedded build.
        let top_current_version = owned
            .cache
            .join("zccache")
            .join(zccache::core::config::versioned_subdir());
        let malformed = owned.cache.join("zccache/vprivate");
        let sibling_sentinel = sibling
            .cache
            .join("zccache/daemon-state/0123456789abcdef0123456789abcdef/sentinel");
        for path in [
            legacy.join("artifact"),
            current.join("artifact"),
            nested_old_version.join("artifact"),
            top_old_version.join("artifact"),
            top_current_version.join("artifact"),
            malformed.join("artifact"),
            sibling_sentinel.clone(),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"payload").unwrap();
        }

        let report = sweep_legacy_cache_roots(&owned, SystemTime::now(), std::time::Duration::ZERO);
        assert_eq!(report.removed, 4);
        assert!(!legacy.exists());
        assert!(!nested_old_version.exists());
        assert!(!top_old_version.exists());
        assert!(!top_current_version.exists());
        assert!(current.join("artifact").is_file());
        assert!(malformed.join("artifact").is_file());
        assert!(sibling_sentinel.is_file());
    }
}
