//! macOS links: symlink classification, creation/removal, and archive
//! extraction (tar handles symlinks natively here).

use std::io::Read;
use std::path::Path;

/// True for symlinks. Destructive collectors must not follow them.
pub fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// Hard-link count for an open file.
pub fn hard_link_count(file: &std::fs::File) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.nlink())
}

/// Create a symlink at `dest` pointing at `target`.
pub fn create(target: &str, dest: &Path, is_dir: bool) -> std::io::Result<()> {
    let _ = is_dir;
    std::os::unix::fs::symlink(target, dest)
}

/// Remove a symlink itself (never its target).
pub fn remove(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

/// Unpack `archive` into `dest`. Tar creates symlinks natively on macOS;
/// the `filter` (when present) skips matching entries.
pub fn unpack_archive_entries<R, F>(
    archive: &mut tar::Archive<R>,
    dest: &Path,
    mut filter: Option<F>,
) -> std::io::Result<()>
where
    R: Read,
    F: FnMut(&Path) -> std::io::Result<bool>,
{
    match filter.as_mut() {
        None => archive.unpack(dest),
        Some(filter) => {
            for entry in archive.entries()? {
                let mut entry = entry?;
                let path = entry.path()?.into_owned();
                if filter(&path)? {
                    std::io::copy(&mut entry, &mut std::io::sink())?;
                    continue;
                }
                entry.unpack_in(dest)?;
            }
            Ok(())
        }
    }
}
