/// Replace an archive subtree with an immutable external directory.
/// The archive owner maps paths; the producer still owns the subtree's format.
#[derive(Debug)]
pub struct CacheProjection<'a> {
    pub source: &'a Path,
    pub archive_prefix: &'a Path,
    pub exclude_prefix: &'a Path,
}

impl CacheProjection<'_> {
    fn validate(&self, cache_dir: &Path) -> Result<()> {
        for prefix in [self.archive_prefix, self.exclude_prefix] {
            if prefix.as_os_str().is_empty()
                || prefix.components().any(|component| {
                    !matches!(component, std::path::Component::Normal(_))
                })
            {
                return Err(SaveLoadError::BadArchivePath(prefix.display().to_string()));
            }
        }
        let source = std::fs::canonicalize(self.source).map_err(|error| io(self.source, error))?;
        let cache = std::fs::canonicalize(cache_dir).map_err(|error| io(cache_dir, error))?;
        if source.starts_with(&cache) || cache.starts_with(&source) {
            return Err(SaveLoadError::BadArchivePath(
                "projected source must be outside the cache directory".into(),
            ));
        }
        Ok(())
    }

    fn relative_path(&self, cache_dir: &Path, source: &Path) -> Result<PathBuf> {
        if let Ok(relative) = source.strip_prefix(self.source) {
            return Ok(self.archive_prefix.join(relative));
        }
        source
            .strip_prefix(cache_dir)
            .map(Path::to_path_buf)
            .map_err(|_| SaveLoadError::BadArchivePath(source.display().to_string()))
    }

    fn excludes(&self, cache_dir: &Path, source: &Path) -> bool {
        source.strip_prefix(cache_dir).is_ok_and(|relative| {
            relative.starts_with(self.exclude_prefix) || relative.starts_with(self.archive_prefix)
        })
    }

    fn protects_manifest_path(&self, path: &str) -> bool {
        manifest_rel_to_path(path).is_ok_and(|relative| relative.starts_with(self.exclude_prefix))
    }
}

fn walk_cache_with_projection(
    cache_dir: &Path,
    threads: Option<usize>,
    profile: SaveProfile,
    projection: Option<&CacheProjection<'_>>,
) -> Result<CacheWalk> {
    let mut walk = walk_cache_files_for_profile(cache_dir, threads, profile)?;
    let Some(projection) = projection else {
        return Ok(walk);
    };
    let mut retained = Vec::new();
    for source in walk.included_paths {
        if projection.excludes(cache_dir, &source) {
            walk.excluded_files += 1;
            walk.excluded_bytes = walk.excluded_bytes.saturating_add(excluded_file_len(&source)?);
        } else {
            retained.push(source);
        }
    }
    walk.included_paths = retained;
    walk.symlinks.retain(|entry| {
        let excluded = projection.excludes(cache_dir, &cache_dir.join(&entry.path));
        walk.excluded_files += u64::from(excluded);
        !excluded
    });
    // The producer's directory is opaque: profile rules for Cargo trees must
    // not prune staged output names or invalidate the producer's own index.
    let (projected, symlinks) = walk_cache_files(projection.source, threads)?;
    if !symlinks.is_empty() {
        return Err(SaveLoadError::BadArchivePath(
            "projected immutable directory must not contain symlinks".into(),
        ));
    }
    walk.included_paths.extend(projected);
    Ok(walk)
}
