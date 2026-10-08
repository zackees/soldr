//! Keep compiler snapshots portable without moving live daemon directories.

use std::path::{Path, PathBuf};

use crate::cache_lib::save::CacheProjection;
use soldr_daemon::zccache_embedded_snapshot as backend;

pub(super) struct PreparedSnapshot {
    _temporary: tempfile::TempDir,
    source: PathBuf,
    archive_prefix: PathBuf,
    exclude_prefix: PathBuf,
}

impl PreparedSnapshot {
    pub(super) fn projection(&self) -> CacheProjection<'_> {
        CacheProjection {
            source: &self.source,
            archive_prefix: &self.archive_prefix,
            exclude_prefix: &self.exclude_prefix,
        }
    }
}

/// Recognize only the Soldr cache layouts the archive transport supports.
/// Generic archives retain their existing behavior and never stop an unrelated
/// ambient daemon. All three transport phases use this same selection.
pub(super) fn selected_paths(cache_dir: &Path) -> Result<Option<crate::core::SoldrPaths>, String> {
    let archive = super::path_for_containment(cache_dir)?;
    let cache = if archive.file_name().is_some_and(|name| name == "cache") {
        archive.clone()
    } else {
        archive.join("cache")
    };
    if cache.join(backend::PRIVATE_PREFIX).is_dir() || cache.join(backend::ARCHIVE_PREFIX).is_dir()
    {
        let root = cache
            .parent()
            .ok_or("Soldr cache has no parent directory")?;
        return Ok(Some(crate::core::SoldrPaths::with_root(root.to_path_buf())));
    }
    let ambient = crate::core::SoldrPaths::new().map_err(|error| error.to_string())?;
    if super::archive_contains_embedded_cache(
        &archive,
        &crate::zccache_embedded::embedded_cache_root(&ambient),
    )? {
        return Ok(Some(ambient));
    }
    Ok(None)
}

fn archive_cache_prefix(
    cache_dir: &Path,
    paths: &crate::core::SoldrPaths,
) -> Result<PathBuf, String> {
    let cache = super::path_for_containment(cache_dir)?;
    let owned = super::path_for_containment(&paths.cache)?;
    owned
        .strip_prefix(&cache)
        .map(Path::to_path_buf)
        .map_err(|_| {
            "compiler snapshot transport requires the Soldr cache directory or its parent".into()
        })
}

pub(super) fn prepare(cache_dir: Option<&Path>) -> Result<Option<PreparedSnapshot>, String> {
    let Some(cache_dir) = cache_dir else {
        return Ok(None);
    };
    let Some(paths) = selected_paths(cache_dir)? else {
        return Ok(None);
    };
    let prefix = archive_cache_prefix(cache_dir, &paths)?;
    let temporary = tempfile::Builder::new()
        .prefix("soldr-compiler-snapshot-")
        .tempdir()
        .map_err(|error| error.to_string())?;
    let source = temporary.path().join("snapshot");
    let receipt = backend::export(&paths, &source).map_err(|error| error.to_string())?;
    let Some(receipt) = receipt else {
        return Ok(None);
    };
    eprintln!(
        "soldr save: compiler snapshot entries={} outputs={}",
        receipt.entries, receipt.outputs
    );
    Ok(Some(PreparedSnapshot {
        _temporary: temporary,
        source,
        archive_prefix: prefix.join(backend::ARCHIVE_PREFIX),
        exclude_prefix: prefix.join(backend::PRIVATE_PREFIX),
    }))
}

pub(super) fn restore(cache_dir: Option<&Path>) -> Result<(), String> {
    let Some(cache_dir) = cache_dir else {
        return Ok(());
    };
    let Some(paths) = selected_paths(cache_dir)? else {
        return Ok(());
    };
    let source = cache_dir
        .join(archive_cache_prefix(cache_dir, &paths)?)
        .join(backend::ARCHIVE_PREFIX);
    if !source.exists() {
        return Ok(()); // Historical archives have no portable backend snapshot.
    }
    let receipt = backend::import(&paths, &source).map_err(|error| error.to_string())?;
    eprintln!(
        "soldr load: compiler snapshot entries={} outputs={}",
        receipt.entries, receipt.outputs
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_cache_and_parent_select_the_same_owned_root() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let root = temporary.path().join("selected");
        let cache = root.join("cache");
        std::fs::create_dir_all(cache.join(backend::PRIVATE_PREFIX)).expect("private store");
        let expected = std::fs::canonicalize(&root).expect("canonical root");
        for archive in [&cache, &root] {
            let paths = selected_paths(archive)
                .expect("resolve")
                .expect("owned root");
            assert_eq!(paths.root, expected);
        }
    }

    #[test]
    fn restored_portable_snapshot_selects_an_empty_destination_store() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let cache = temporary.path().join("cache");
        std::fs::create_dir_all(cache.join(backend::ARCHIVE_PREFIX)).expect("portable store");
        let paths = selected_paths(&cache)
            .expect("resolve")
            .expect("owned root");
        assert_eq!(
            paths.cache,
            std::fs::canonicalize(cache).expect("canonical cache")
        );
    }

    #[test]
    fn unrelated_archive_is_not_an_embedded_store() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir_all(temporary.path().join("cache/arbitrary")).expect("generic cache");
        assert!(selected_paths(temporary.path()).expect("resolve").is_none());
    }
}
