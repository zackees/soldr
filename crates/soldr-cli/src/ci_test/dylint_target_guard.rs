//! What the Dylint shared-tree guard counts as a compiler artifact.
//!
//! Split out of `execute.rs` for the 1,000-line production ceiling.

use crate::core::SoldrError;

/// How many offending paths the shared-tree guard names before truncating.
pub(crate) const MATERIAL_ARTIFACT_LIST_LIMIT: usize = 40;

/// Every file under `path` that is not cargo bookkeeping, up to `limit`,
/// with its size, in path order. Bookkeeping (`.rustc_info.json`,
/// `CACHEDIR.TAG`, `.cargo-lock`) is what a cargo that merely *looked* at a
/// target dir leaves behind; anything else means a compiler wrote here.
pub(crate) fn material_artifacts(
    path: &std::path::Path,
    limit: usize,
) -> Result<Vec<(std::path::PathBuf, u64)>, SoldrError> {
    let mut found = Vec::new();
    collect_material_artifacts(path, limit, &mut found)?;
    Ok(found)
}

fn collect_material_artifacts(
    path: &std::path::Path,
    limit: usize,
    found: &mut Vec<(std::path::PathBuf, u64)>,
) -> Result<(), SoldrError> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut entries: Vec<_> = entries.collect::<Result<_, _>>()?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        if found.len() >= limit {
            return Ok(());
        }
        let child = entry.path();
        if child.is_dir() {
            collect_material_artifacts(&child, limit, found)?;
            continue;
        }
        let name = child.file_name().and_then(|value| value.to_str());
        if !matches!(
            name,
            Some(".rustc_info.json" | "CACHEDIR.TAG" | ".cargo-lock")
        ) {
            let bytes = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            found.push((child, bytes));
        }
    }
    Ok(())
}
