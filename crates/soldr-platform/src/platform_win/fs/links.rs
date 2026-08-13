//! Windows links: reparse classification, symlink create/remove, and the
//! deferred archive-symlink materialization Windows requires.

use std::io::Read;
use std::path::{Path, PathBuf};

/// True for symlinks and any NTFS reparse point (junctions included).
/// Destructive collectors must not follow any of them.
pub fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Hard-link count for an open file.
pub fn hard_link_count(file: &std::fs::File) -> std::io::Result<u64> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `file` owns a valid handle, and `info` points to enough
    // writable storage for BY_HANDLE_FILE_INFORMATION.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, info.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful call initialized the full structure.
    Ok(u64::from(unsafe { info.assume_init() }.nNumberOfLinks))
}

/// Create a symlink at `dest` pointing at `target`.
///
/// The target string is converted to backslash separators: NTFS reparse
/// points are unreadable when created with a literal POSIX target.
pub fn create(target: &str, dest: &Path, is_dir: bool) -> std::io::Result<()> {
    let native = target.replace('/', "\\");
    if is_dir {
        std::os::windows::fs::symlink_dir(native, dest)
    } else {
        std::os::windows::fs::symlink_file(native, dest)
    }
}

/// Remove a symlink itself (never its target). A directory symlink must
/// be removed with `remove_dir`; try file-removal first and fall back so
/// both flavors are covered.
pub fn remove(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::remove_dir(path),
    }
}

// ---------------------------------------------------------------------------
// Deferred archive-symlink materialization.
//
// `tar::Archive::unpack` creates symlinks verbatim, which on Windows
// produces unreadable reparse points (the targets use `/` separators) and
// often fails outright. Instead, walk the entries, defer every symlink,
// and replay them once the rest of the tree is on disk — creating the
// correct NTFS link flavor per target and falling back to copies when
// link creation is denied (no SeCreateSymbolicLinkPrivilege).
// ---------------------------------------------------------------------------

/// A symlink entry captured during the extraction pass, replayed once
/// the rest of the tree is on disk.
struct DeferredLink {
    /// Entry path relative to the extraction root.
    rel: PathBuf,
    /// Link target exactly as stored in the archive.
    target: PathBuf,
}

/// Unpack `archive` into `dest`, materializing symlinks through the
/// deferred replay. `filter` (when present) receives each archive-relative
/// path and skips the entry when it returns `true`.
pub fn unpack_archive_entries<R, F>(
    archive: &mut tar::Archive<R>,
    dest: &Path,
    mut filter: Option<F>,
) -> std::io::Result<()>
where
    R: Read,
    F: FnMut(&Path) -> std::io::Result<bool>,
{
    fn archive_err(e: impl std::fmt::Display) -> std::io::Error {
        std::io::Error::other(e.to_string())
    }

    let mut deferred: Vec<DeferredLink> = Vec::new();
    for entry in archive.entries().map_err(archive_err)? {
        let mut entry = entry.map_err(archive_err)?;
        let rel = entry.path().map_err(archive_err)?.into_owned();
        if filter
            .as_mut()
            .map_or(Ok(false), |filter| filter(&rel))
            .map_err(archive_err)?
        {
            std::io::copy(&mut entry, &mut std::io::sink()).map_err(archive_err)?;
            continue;
        }
        if entry.header().entry_type().is_symlink() {
            let target = entry
                .link_name()
                .map_err(archive_err)?
                .ok_or_else(|| archive_err(format!("symlink entry {} has no target", rel.display())))?
                .into_owned();
            deferred.push(DeferredLink { rel, target });
        } else {
            entry.unpack_in(dest).map_err(archive_err)?;
        }
    }
    replay_symlinks(dest, deferred)
}

/// Replay deferred symlinks. Multiple passes handle chains (a link
/// whose target is another link): file-target links are created
/// first, then directory-target links deepest-first, so if the copy
/// fallback engages, a copied directory already contains every link
/// materialized inside it.
fn replay_symlinks(dest: &Path, mut pending: Vec<DeferredLink>) -> std::io::Result<()> {
    let mut linked = 0usize;
    let mut copied = 0usize;
    let total = pending.len();
    while !pending.is_empty() {
        let mut ready_files: Vec<(DeferredLink, PathBuf)> = Vec::new();
        let mut ready_dirs: Vec<(DeferredLink, PathBuf)> = Vec::new();
        let mut unresolved: Vec<DeferredLink> = Vec::new();
        for link in pending {
            let link_path = dest.join(&link.rel);
            match resolve_link_target(dest, &link_path, &link.target) {
                Some(target_path) => match std::fs::metadata(&target_path) {
                    Ok(meta) if meta.is_dir() => ready_dirs.push((link, target_path)),
                    Ok(_) => ready_files.push((link, target_path)),
                    Err(_) => unresolved.push(link),
                },
                None => unresolved.push(link),
            }
        }
        if !ready_files.is_empty() {
            for (link, target_path) in ready_files {
                materialize(dest, &link, &target_path, false, &mut linked, &mut copied)?;
            }
            pending = ready_dirs.into_iter().map(|(link, _)| link).collect();
            pending.extend(unresolved);
            continue;
        }
        if !ready_dirs.is_empty() {
            // Deepest links first: a shallower directory copy then
            // already contains any deeper link inside it.
            ready_dirs.sort_by_key(|(link, _)| std::cmp::Reverse(link.rel.components().count()));
            for (link, target_path) in ready_dirs {
                materialize(dest, &link, &target_path, true, &mut linked, &mut copied)?;
            }
            pending = unresolved;
            continue;
        }
        // No target resolves inside dest: dangling or external links.
        // Match `tar::Archive::unpack` (which creates such links
        // verbatim) on a best-effort basis; a failure to create a
        // dangling link is not fatal to the extraction.
        let mut dangling = 0usize;
        for link in &unresolved {
            let link_path = dest.join(&link.rel);
            let Some(_) = resolve_link_target(dest, &link_path, &link.target) else {
                // Never reproduce an absolute or escaping target in the
                // extracted Windows tree. In particular, this prevents a
                // later consumer from following an archive-controlled
                // reparse point outside the SDK root.
                eprintln!(
                    "soldr: warning: skipped escaping tar symlink {} -> {}",
                    link.rel.display(),
                    link.target.display(),
                );
                continue;
            };
            let target = windows_link_target(&link.target);
            if std::os::windows::fs::symlink_file(&target, &link_path).is_ok() {
                dangling += 1;
            } else {
                eprintln!(
                    "soldr: warning: skipped unresolvable tar symlink {} -> {} (Windows target {})",
                    link.rel.display(),
                    link.target.display(),
                    target.display(),
                );
            }
        }
        linked += dangling;
        break;
    }
    if total > 0 {
        eprintln!(
            "soldr: tar extract: materialized {total} symlink(s) on Windows \
             ({linked} as NTFS symlinks, {copied} as copies)"
        );
    }
    Ok(())
}

fn materialize(
    dest: &Path,
    link: &DeferredLink,
    target_path: &Path,
    is_dir: bool,
    linked: &mut usize,
    copied: &mut usize,
) -> std::io::Result<()> {
    let link_path = dest.join(&link.rel);
    if let Some(parent) = link_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = std::fs::symlink_metadata(&link_path) {
        let _ = if meta.is_dir() {
            std::fs::remove_dir_all(&link_path)
        } else {
            std::fs::remove_file(&link_path)
        };
    }
    // Keep archive links relative so the tree is relocatable, but Windows
    // needs backslash separators in the reparse-point target. A literal
    // POSIX `pthread/pthread.h` target can be created on NTFS yet be
    // unreadable (`ERROR_INVALID_PARAMETER`).
    let windows_target = windows_link_target(&link.target);
    let created = if is_dir {
        std::os::windows::fs::symlink_dir(&windows_target, &link_path)
    } else {
        std::os::windows::fs::symlink_file(&windows_target, &link_path)
    };
    match created.and_then(|()| verify_materialization(&link_path, is_dir)) {
        Ok(()) => {
            *linked += 1;
            Ok(())
        }
        Err(err) => {
            // A successful API call is insufficient: Windows can leave a
            // broken reparse point behind. Remove it before falling back
            // to a safe copy of the lexically-contained target.
            remove_materialization(&link_path, is_dir);
            eprintln!(
                "soldr: tar symlink fallback member={} stored_target={} resolved_target={} \
                 windows_target={} kind={}; materializing a copy after: {err}",
                link.rel.display(),
                link.target.display(),
                target_path.display(),
                windows_target.display(),
                if is_dir { "directory" } else { "file" },
            );
            if is_dir {
                copy_dir_recursive(target_path, &link_path)?;
            } else {
                std::fs::copy(target_path, &link_path)?;
            }
            *copied += 1;
            Ok(())
        }
    }
}

// `resolve_link_target` lives in the neutral facade
// (`crate::platform::fs::links`) so the containment logic is compiled and
// tested on every host; import it here for the replay path.
use crate::platform::fs::links::resolve_link_target;

fn windows_link_target(target: &Path) -> PathBuf {
    PathBuf::from(target.to_string_lossy().replace('/', "\\"))
}

fn verify_materialization(link_path: &Path, is_dir: bool) -> std::io::Result<()> {
    let metadata = std::fs::metadata(link_path)?;
    if is_dir {
        if !metadata.is_dir() {
            return Err(std::io::Error::other(
                "directory link resolved to a non-directory",
            ));
        }
        std::fs::read_dir(link_path)?;
    } else {
        if metadata.is_dir() {
            return Err(std::io::Error::other("file link resolved to a directory"));
        }
        std::fs::File::open(link_path)?;
    }
    Ok(())
}

fn remove_materialization(path: &Path, is_dir: bool) {
    // `symlink_metadata` reports a directory symlink as a symlink rather
    // than a directory. Use the target kind determined before creation so
    // Windows receives the matching remove operation.
    let _ = if is_dir {
        std::fs::remove_dir(path).or_else(|_| std::fs::remove_file(path))
    } else {
        std::fs::remove_file(path)
    };
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for child in std::fs::read_dir(src)? {
        let child = child?;
        let child_src = child.path();
        let child_dst = dst.join(child.file_name());
        // Follow links: by replay ordering, links inside a copied
        // directory were already materialized.
        let meta = std::fs::metadata(&child_src)?;
        if meta.is_dir() {
            copy_dir_recursive(&child_src, &child_dst)?;
        } else {
            std::fs::copy(&child_src, &child_dst)?;
        }
    }
    Ok(())
}
