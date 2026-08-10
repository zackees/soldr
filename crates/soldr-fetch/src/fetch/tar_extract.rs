//! Tar extraction with Windows-correct symlink materialization (soldr#2300).
//!
//! The `tar` crate creates every symlink entry on Windows with
//! `std::os::windows::fs::symlink_file`, even when the link target is a
//! directory (`tar-0.4.46/src/entry.rs`). NTFS symlinks are flavored: a
//! *file*-flavor symlink pointing at a directory is non-traversable — a
//! link-following stat fails, `Path::is_dir()` returns `false`, and any
//! native consumer (the linker, sysroot validation) sees a broken path.
//! The conda GNU/Linux sysroot ships `usr/lib -> lib64`, so on a Windows
//! host every extraction produced a sysroot whose `usr/lib` could not be
//! entered, blocking Windows-host → Linux cross-compilation at `prepare`.
//!
//! [`unpack_tar`] is a drop-in replacement for `tar::Archive::unpack`:
//!
//! - On non-Windows it *is* `tar::Archive::unpack` — byte-identical
//!   behavior, asserted by the unit tests below.
//! - On Windows it unpacks regular entries in archive order, defers every
//!   symlink entry, and replays them once their targets exist so each link
//!   can be created with the correct NTFS flavor (`symlink_dir` for
//!   directory targets, `symlink_file` for file targets). When symlink
//!   creation is denied (no `SeCreateSymbolicLinkPrivilege` and Developer
//!   Mode off), the link is materialized as a copy of its resolved target
//!   instead — always traversable, at the cost of disk space.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::core::SoldrError;

/// Unpack `archive` into `dest`.
///
/// See the module docs: identical to `tar::Archive::unpack` on
/// non-Windows; on Windows, symlink entries are deferred and replayed
/// with the correct NTFS link flavor (falling back to copies).
pub fn unpack_tar<R: Read>(archive: &mut tar::Archive<R>, dest: &Path) -> Result<(), SoldrError> {
    #[cfg(not(windows))]
    {
        archive
            .unpack(dest)
            .map_err(|e| SoldrError::Archive(format!("tar unpack: {e}")))
    }
    #[cfg(windows)]
    {
        windows_impl::unpack_deferring_symlinks(archive, dest, |_| Ok(false))
    }
}

/// Unpack `archive` into `dest`, skipping entries selected by `filter`.
///
/// This is the filtered counterpart to [`unpack_tar`]. It exists for archive
/// formats, such as Apple SDK bundles, that contain known host-invalid optional
/// entries but still need the shared Windows symlink materialization path.
/// `filter` receives each archive-relative path and returns whether to skip it.
pub fn unpack_tar_filtered<R: Read, F>(
    archive: &mut tar::Archive<R>,
    dest: &Path,
    filter: F,
) -> Result<(), SoldrError>
where
    F: FnMut(&Path) -> Result<bool, SoldrError>,
{
    #[cfg(not(windows))]
    {
        let mut filter = filter;
        for entry in archive
            .entries()
            .map_err(|e| SoldrError::Archive(format!("tar unpack: {e}")))?
        {
            let mut entry = entry.map_err(|e| SoldrError::Archive(format!("tar unpack: {e}")))?;
            let path = entry
                .path()
                .map_err(|e| SoldrError::Archive(format!("tar unpack: {e}")))?
                .into_owned();
            if filter(&path)? {
                std::io::copy(&mut entry, &mut std::io::sink())
                    .map_err(|e| SoldrError::Archive(format!("tar unpack: {e}")))?;
                continue;
            }
            entry
                .unpack_in(dest)
                .map_err(|e| SoldrError::Archive(format!("tar unpack: {e}")))?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        windows_impl::unpack_deferring_symlinks(archive, dest, filter)
    }
}

/// Lexically resolve a symlink `target` (as stored in the tar entry,
/// `/`-separated and usually relative) against the link's parent
/// directory, refusing absolute targets and any result that escapes
/// `dest`. Returns the absolute on-disk path the link points at.
///
/// This is only used to decide the NTFS link flavor and as the source of
/// the copy fallback — it deliberately never resolves outside the
/// extraction root, so a hostile archive cannot make the copy fallback
/// read arbitrary host files.
///
/// Only the Windows replay path calls this in production; on other
/// platforms it is exercised by the portable unit tests, which keep the
/// containment logic compiled and verified everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
fn resolve_link_target(dest: &Path, link_path: &Path, target: &Path) -> Option<PathBuf> {
    let link_rel = link_path.strip_prefix(dest).ok()?;
    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    for component in link_rel.components() {
        match component {
            Component::Normal(part) => stack.push(part.to_os_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    // Drop the link's own file name; targets are relative to its parent.
    stack.pop()?;
    for component in target.components() {
        match component {
            Component::Normal(part) => stack.push(part.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                stack.pop()?;
            }
            // Absolute targets (RootDir / Prefix) never resolve inside dest.
            _ => return None,
        }
    }
    let mut resolved = dest.to_path_buf();
    for part in stack {
        resolved.push(part);
    }
    Some(resolved)
}

#[cfg(windows)]
mod windows_impl {
    use super::*;

    fn archive_err(e: impl std::fmt::Display) -> SoldrError {
        SoldrError::Archive(format!("tar unpack: {e}"))
    }

    /// A symlink entry captured during the extraction pass, replayed once
    /// the rest of the tree is on disk.
    struct DeferredLink {
        /// Entry path relative to the extraction root.
        rel: PathBuf,
        /// Link target exactly as stored in the archive.
        target: PathBuf,
    }

    pub(super) fn unpack_deferring_symlinks<R: Read, F>(
        archive: &mut tar::Archive<R>,
        dest: &Path,
        mut filter: F,
    ) -> Result<(), SoldrError>
    where
        F: FnMut(&Path) -> Result<bool, SoldrError>,
    {
        let mut deferred: Vec<DeferredLink> = Vec::new();
        for entry in archive.entries().map_err(archive_err)? {
            let mut entry = entry.map_err(archive_err)?;
            let rel = entry.path().map_err(archive_err)?.into_owned();
            if filter(&rel)? {
                std::io::copy(&mut entry, &mut std::io::sink()).map_err(archive_err)?;
                continue;
            }
            if entry.header().entry_type().is_symlink() {
                let target = entry
                    .link_name()
                    .map_err(archive_err)?
                    .ok_or_else(|| {
                        archive_err(format!("symlink entry {} has no target", rel.display()))
                    })?
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
    fn replay_symlinks(dest: &Path, mut pending: Vec<DeferredLink>) -> Result<(), SoldrError> {
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
                ready_dirs
                    .sort_by_key(|(link, _)| std::cmp::Reverse(link.rel.components().count()));
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
    ) -> Result<(), SoldrError> {
        let link_path = dest.join(&link.rel);
        if let Some(parent) = link_path.parent() {
            std::fs::create_dir_all(parent).map_err(archive_err)?;
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
                    std::fs::copy(target_path, &link_path).map_err(archive_err)?;
                }
                *copied += 1;
                Ok(())
            }
        }
    }

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

    fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), SoldrError> {
        std::fs::create_dir_all(dst).map_err(archive_err)?;
        for child in std::fs::read_dir(src).map_err(archive_err)? {
            let child = child.map_err(archive_err)?;
            let child_src = child.path();
            let child_dst = dst.join(child.file_name());
            // Follow links: by replay ordering, links inside a copied
            // directory were already materialized.
            let meta = std::fs::metadata(&child_src).map_err(archive_err)?;
            if meta.is_dir() {
                copy_dir_recursive(&child_src, &child_dst)?;
            } else {
                std::fs::copy(&child_src, &child_dst).map_err(archive_err)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A synthetic bundle shaped like the conda sysroot that triggered
    /// soldr#2300: a real `lib64/` directory, a file symlink chain inside
    /// it, and directory symlinks (`usr/lib -> lib64`, appearing in the
    /// archive BEFORE the directory it points at, to exercise deferral).
    fn synthetic_sysroot_tar() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());

        let mut dir = tar::Header::new_gnu();
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_size(0);
        dir.set_mode(0o755);
        dir.set_cksum();
        builder
            .append_data(&mut dir.clone(), "usr/", &b""[..])
            .unwrap();

        // Dir symlink stored before its target exists (tar ordering).
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        builder
            .append_link(&mut link.clone(), "usr/lib", "lib64")
            .unwrap();

        builder
            .append_data(&mut dir.clone(), "usr/lib64/", &b""[..])
            .unwrap();

        let mut file = tar::Header::new_gnu();
        file.set_entry_type(tar::EntryType::Regular);
        file.set_mode(0o644);
        file.set_size(5);
        builder
            .append_data(&mut file.clone(), "usr/lib64/libc-2.17.so", &b"hello"[..])
            .unwrap();

        // File symlink chain: libc.so -> libc.so.6 -> libc-2.17.so.
        builder
            .append_link(&mut link.clone(), "usr/lib64/libc.so.6", "libc-2.17.so")
            .unwrap();
        builder
            .append_link(&mut link.clone(), "usr/lib64/libc.so", "libc.so.6")
            .unwrap();

        builder.into_inner().unwrap()
    }

    /// Minimal Apple SDK shape with the two POSIX links that fail when their
    /// archive `/` separators reach a Windows reparse point unchanged.
    fn synthetic_apple_sdk_tar() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut dir = tar::Header::new_gnu();
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_size(0);
        dir.set_mode(0o755);
        dir.set_cksum();
        for path in [
            "package/",
            "package/sdk/",
            "package/sdk/usr/",
            "package/sdk/usr/include/",
            "package/sdk/System/",
            "package/sdk/System/Library/",
            "package/sdk/System/Library/Frameworks/",
            "package/sdk/System/Library/Frameworks/CoreFoundation.framework/",
            "package/sdk/System/Library/Frameworks/CoreFoundation.framework/Versions/",
        ] {
            builder
                .append_data(&mut dir.clone(), path, std::io::empty())
                .unwrap();
        }

        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        builder
            .append_link(
                &mut link.clone(),
                "package/sdk/usr/include/pthread.h",
                "pthread/pthread.h",
            )
            .unwrap();
        builder
            .append_link(
                &mut link.clone(),
                "package/sdk/System/Library/Frameworks/CoreFoundation.framework/Headers",
                "Versions/Current/Headers",
            )
            .unwrap();
        builder
            .append_link(
                &mut link.clone(),
                "package/sdk/System/Library/Frameworks/CoreFoundation.framework/Versions/Current",
                "A",
            )
            .unwrap();

        for path in [
            "package/sdk/usr/include/pthread/",
            "package/sdk/System/Library/Frameworks/CoreFoundation.framework/Versions/A/",
            "package/sdk/System/Library/Frameworks/CoreFoundation.framework/Versions/A/Headers/",
        ] {
            builder
                .append_data(&mut dir.clone(), path, std::io::empty())
                .unwrap();
        }
        let mut file = tar::Header::new_gnu();
        file.set_entry_type(tar::EntryType::Regular);
        file.set_mode(0o644);
        file.set_size(7);
        file.set_cksum();
        builder
            .append_data(
                &mut file.clone(),
                "package/sdk/usr/include/pthread/pthread.h",
                &b"pthread"[..],
            )
            .unwrap();
        builder
            .append_data(
                &mut file,
                "package/sdk/System/Library/Frameworks/CoreFoundation.framework/Versions/A/Headers/CoreFoundation.h",
                &b"header!"[..],
            )
            .unwrap();
        builder.into_inner().unwrap()
    }

    crate::timed_test!(unpack_tar_makes_dir_symlinks_traversable, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let dest = tmp.path();
        let tarball = synthetic_sysroot_tar();
        let mut archive = tar::Archive::new(Cursor::new(tarball));
        unpack_tar(&mut archive, dest).expect("unpack");

        // The whole point of soldr#2300: the dir link must be enterable
        // with link-following stats on every platform.
        let usr_lib = dest.join("usr/lib");
        assert!(usr_lib.is_dir(), "usr/lib must be a traversable directory");
        assert_eq!(
            std::fs::read_to_string(usr_lib.join("libc-2.17.so")).expect("read through dir link"),
            "hello"
        );
        // File symlink chain resolves to real content.
        assert_eq!(
            std::fs::read_to_string(dest.join("usr/lib64/libc.so")).expect("read file link chain"),
            "hello"
        );

        // Linux behavior stays byte-identical to `tar::Archive::unpack`:
        // real symlinks with the stored targets, not copies.
        #[cfg(unix)]
        {
            let meta = std::fs::symlink_metadata(&usr_lib).expect("lstat");
            assert!(meta.file_type().is_symlink(), "usr/lib must stay a symlink");
            assert_eq!(
                std::fs::read_link(&usr_lib).unwrap(),
                PathBuf::from("lib64")
            );
            assert_eq!(
                std::fs::read_link(dest.join("usr/lib64/libc.so")).unwrap(),
                PathBuf::from("libc.so.6")
            );
        }
        // On Windows the entry is either a correctly-flavored NTFS
        // symlink or a copied directory — both must satisfy is_dir()
        // (asserted above) and support enumeration.
        #[cfg(windows)]
        {
            let names: Vec<_> = std::fs::read_dir(&usr_lib)
                .expect("read_dir through link")
                .map(|e| e.unwrap().file_name())
                .collect();
            assert!(
                names.iter().any(|n| n == "libc-2.17.so"),
                "dir link must enumerate its target's children: {names:?}"
            );
        }
    });

    crate::timed_test!(unpack_tar_materializes_posix_apple_links, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let dest = tmp.path();
        let mut archive = tar::Archive::new(Cursor::new(synthetic_apple_sdk_tar()));
        unpack_tar(&mut archive, dest).expect("unpack");

        let pthread = dest.join("package/sdk/usr/include/pthread.h");
        assert_eq!(
            std::fs::read_to_string(&pthread).expect("read POSIX file link"),
            "pthread"
        );
        let headers =
            dest.join("package/sdk/System/Library/Frameworks/CoreFoundation.framework/Headers");
        assert!(headers.is_dir(), "framework Headers must be traversable");
        assert_eq!(
            std::fs::read_to_string(headers.join("CoreFoundation.h"))
                .expect("read through chained framework links"),
            "header!"
        );
        #[cfg(windows)]
        assert!(
            std::fs::read_dir(&headers).is_ok(),
            "framework Headers must support directory enumeration on Windows"
        );
    });

    crate::timed_test!(resolve_link_target_stays_inside_dest, {
        let dest = Path::new("/extract");
        let link = dest.join("usr").join("lib");
        assert_eq!(
            resolve_link_target(dest, &link, Path::new("lib64")),
            Some(dest.join("usr").join("lib64"))
        );
        assert_eq!(
            resolve_link_target(dest, &link, Path::new("../lib64")),
            Some(dest.join("lib64"))
        );
        // Escaping the extraction root or absolute targets never resolve.
        assert_eq!(
            resolve_link_target(dest, &link, Path::new("../../../etc/passwd")),
            None
        );
        assert_eq!(
            resolve_link_target(dest, &link, Path::new("/etc/passwd")),
            None
        );
    });

    crate::timed_test!(unpack_tar_ignores_escaping_symlink_copy, {
        // A link whose target escapes dest must not break extraction and
        // must never be materialized as a copy of a host file.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let dest = tmp.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(tmp.path().join("secret.txt"), "outside").unwrap();

        let mut builder = tar::Builder::new(Vec::new());
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        builder
            .append_link(&mut link, "escape", "../secret.txt")
            .unwrap();
        let tarball = builder.into_inner().unwrap();

        let mut archive = tar::Archive::new(Cursor::new(tarball));
        unpack_tar(&mut archive, &dest).expect("unpack");

        let escape = dest.join("escape");
        if let Ok(meta) = std::fs::symlink_metadata(&escape) {
            // A verbatim symlink matches upstream tar behavior; a regular
            // file here would mean the copy fallback read outside dest.
            assert!(
                meta.file_type().is_symlink(),
                "escaping target must never be copied into dest"
            );
        }
    });
}
