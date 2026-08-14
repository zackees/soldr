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
use std::path::Path;

use crate::core::SoldrError;

fn archive_err(e: impl std::fmt::Display) -> SoldrError {
    SoldrError::Archive(format!("tar unpack: {e}"))
}

/// Unpack `archive` into `dest`.
///
/// See the module docs: identical to `tar::Archive::unpack` on
/// non-Windows; on Windows, symlink entries are deferred and replayed
/// with the correct NTFS link flavor (falling back to copies). The
/// platform crate owns the Windows replay machinery.
pub fn unpack_tar<R: Read>(archive: &mut tar::Archive<R>, dest: &Path) -> Result<(), SoldrError> {
    crate::platform::fs::links::unpack_archive_entries(archive, dest, Some(|_rel: &Path| Ok(false)))
        .map_err(archive_err)
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
    mut filter: F,
) -> Result<(), SoldrError>
where
    F: FnMut(&Path) -> Result<bool, SoldrError>,
{
    crate::platform::fs::links::unpack_archive_entries(
        archive,
        dest,
        Some(move |rel: &Path| {
            filter(rel).map_err(|error| std::io::Error::other(error.to_string()))
        }),
    )
    .map_err(archive_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::PathBuf;

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

    #[test]
    fn unpack_tar_makes_dir_symlinks_traversable() {
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

        // Unix behavior stays byte-identical to `tar::Archive::unpack`:
        // real symlinks with the stored targets, not copies. On Windows
        // the entry is either a correctly-flavored NTFS symlink or a
        // copied directory — both must satisfy is_dir() (asserted above)
        // and support enumeration.
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            let names: Vec<_> = std::fs::read_dir(&usr_lib)
                .expect("read_dir through link")
                .map(|e| e.unwrap().file_name())
                .collect();
            assert!(
                names.iter().any(|n| n == "libc-2.17.so"),
                "dir link must enumerate its target's children: {names:?}"
            );
        } else {
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
    }

    #[test]
    fn unpack_tar_materializes_posix_apple_links() {
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
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            assert!(
                std::fs::read_dir(&headers).is_ok(),
                "framework Headers must support directory enumeration on Windows"
            );
        }
    }

    #[test]
    fn unpack_tar_ignores_escaping_symlink_copy() {
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
    }
}
