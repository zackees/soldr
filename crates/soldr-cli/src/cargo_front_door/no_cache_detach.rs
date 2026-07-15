//! Prepare a target directory for an unmediated no-cache build.
//!
//! zccache's fastest non-reflink delivery tier is a protected hardlink: the
//! cache blob and requested target path share an inode and are read-only.
//! Mediated compiles copy-detach that output before rustc writes it. A
//! no-cache recovery deliberately removes the wrapper and daemon, so this
//! module performs the same ownership transition locally.
//!
//! Never clear read-only on a shared file. On Unix, unlinking a read-only file
//! is controlled by its parent directory and leaves the cache inode untouched.
//! On Windows, `FileDispositionInfoEx` with
//! `FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE` removes only the opened
//! directory entry without changing the shared file attributes.

use crate::core::{command_output_with_timeout, suppress_windows_console_window, SoldrError};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static DETACH_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const DETACH_TEMP_PREFIX: &str = ".soldr-no-cache-detach-";

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct NoCacheDetachReport {
    pub(super) target_dir: PathBuf,
    pub(super) detached_shared: usize,
    pub(super) made_writable: usize,
    scanned_files: usize,
}

#[derive(Deserialize)]
struct TargetDirectoryMetadata {
    target_directory: PathBuf,
}

pub(super) fn prepare_target_for_unmediated_build(
    tool: &Path,
    args: &[String],
    child_command: &std::process::Command,
) -> Result<NoCacheDetachReport, SoldrError> {
    let target_dir = resolve_target_directory(tool, args, child_command)?;
    detach_target_tree(&target_dir)
}

/// Resolve the target root in the build tool's own order. Explicit CLI/env
/// overrides are cheap and authoritative; otherwise metadata is the only
/// reliable way to honor workspace discovery plus `.cargo/config.toml`
/// `build.target-dir` settings.
fn resolve_target_directory(
    tool: &Path,
    args: &[String],
    child_command: &std::process::Command,
) -> Result<PathBuf, SoldrError> {
    let cargo_target_dir = std::env::var_os("CARGO_TARGET_DIR");
    let test_cargo_bin = std::env::var_os(crate::TEST_CARGO_BIN_ENV_VAR);
    resolve_target_directory_with_env(
        tool,
        args,
        child_command,
        cargo_target_dir.as_deref(),
        test_cargo_bin.as_deref(),
        super::resolve_target_dir_for_gc,
    )
}

fn resolve_target_directory_with_env(
    tool: &Path,
    args: &[String],
    child_command: &std::process::Command,
    cargo_target_dir: Option<&OsStr>,
    test_cargo_bin: Option<&OsStr>,
    test_default_target: impl FnOnce(&[String]) -> Option<PathBuf>,
) -> Result<PathBuf, SoldrError> {
    if let Some(target_dir) = super::disk::cargo_arg_value(args, "--target-dir") {
        return Ok(super::disk::absolutize_path(PathBuf::from(target_dir)));
    }
    if let Some(target_dir) = cargo_target_dir.filter(|value| !value.is_empty()) {
        return Ok(super::disk::absolutize_path(PathBuf::from(target_dir)));
    }

    // Test Cargo binaries are intentionally tiny argv recorders and need not
    // implement metadata. Use the same inexpensive selected/default target
    // resolution as target-GC hooks only for this explicit seam. Real Cargo
    // always takes the metadata path below, preserving workspace/config rules.
    if test_cargo_bin.is_some() {
        return test_default_target(args).ok_or_else(|| {
            SoldrError::Other(format!(
                "no-cache preflight could not select a target directory for {}",
                crate::TEST_CARGO_BIN_ENV_VAR,
            ))
        });
    }

    let mut probe = std::process::Command::new(tool);
    probe.args(["metadata", "--format-version", "1", "--no-deps"]);
    probe.args(crate::rust_plan::cargo_metadata_passthrough_args(args));
    crate::apply_implicit_toolchain_homes(&mut probe);
    suppress_windows_console_window(&mut probe);
    probe.env_remove("MAKEFLAGS");
    probe.env_remove("CARGO_MAKEFLAGS");

    // Match the already-prepared child environment. This carries the selected
    // toolchain, SDK variables, and explicit removals without forwarding the
    // build subcommand itself to the metadata probe.
    for (key, value) in child_command.get_envs() {
        if let Some(value) = value {
            probe.env(key, value);
        } else {
            probe.env_remove(key);
        }
    }

    let output = command_output_with_timeout(&mut probe, "metadata probe for no-cache preflight")?;
    if !output.status.success() {
        return Err(SoldrError::Other(format!(
            "metadata failed while preparing the no-cache target directory: {}",
            crate::zccache::command_stderr(&output),
        )));
    }
    let metadata: TargetDirectoryMetadata =
        serde_json::from_slice(&output.stdout).map_err(|error| {
            SoldrError::Other(format!(
                "failed to parse metadata for the no-cache target directory: {error}"
            ))
        })?;
    Ok(metadata.target_directory)
}

fn detach_target_tree(target_dir: &Path) -> Result<NoCacheDetachReport, SoldrError> {
    detach_target_tree_with_hook(target_dir, |_| {})
}

struct OpenDirectory {
    dir: Dir,
    relative_path: PathBuf,
    display_path: PathBuf,
}

fn open_target_root(target_dir: &Path) -> Result<Option<OpenDirectory>, SoldrError> {
    let canonical = match std::fs::canonicalize(target_dir) {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(detach_error(
                target_dir,
                "resolve target directory (including symlink/junction roots)",
                error,
            ))
        }
    };
    let dir = Dir::open_ambient_dir(&canonical, ambient_authority())
        .map_err(|error| detach_error(&canonical, "open target directory capability", error))?;
    let metadata = dir
        .dir_metadata()
        .map_err(|error| detach_error(&canonical, "inspect opened target directory", error))?;
    if !metadata.is_dir() {
        return Err(SoldrError::Other(format!(
            "no-cache preflight target is not a directory: {}",
            canonical.display(),
        )));
    }
    Ok(Some(OpenDirectory {
        dir,
        relative_path: PathBuf::new(),
        display_path: canonical,
    }))
}

fn detach_target_tree_with_hook(
    target_dir: &Path,
    mut after_directory_open: impl FnMut(&Path),
) -> Result<NoCacheDetachReport, SoldrError> {
    let Some(root) = open_target_root(target_dir)? else {
        return Ok(NoCacheDetachReport {
            target_dir: target_dir.to_path_buf(),
            ..NoCacheDetachReport::default()
        });
    };
    let mut report = NoCacheDetachReport {
        target_dir: root.display_path.clone(),
        ..NoCacheDetachReport::default()
    };

    // Retain every acquired Cargo lock until the complete ownership
    // transition is done. Releasing each successful probe immediately would
    // leave a window for another Cargo process to start during the scan.
    let _build_lock_guards = acquire_build_locks(&root)?;
    let mut pending = vec![root];
    while let Some(directory) = pending.pop() {
        for name in snapshot_directory(&directory, "read target directory")? {
            if is_detach_temp(&name) {
                continue;
            }
            let display_path = directory.display_path.join(&name);
            let metadata = match directory.dir.symlink_metadata(&name) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(detach_error(&display_path, "inspect target entry", error))
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                let Some(child) = open_child_directory(&directory, &name)? else {
                    continue;
                };
                after_directory_open(&child.display_path);
                pending.push(child);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            report.scanned_files += 1;
            match prepare_file(&directory, &name)? {
                PreparedFile::Unchanged => {}
                PreparedFile::DetachedShared => report.detached_shared += 1,
                PreparedFile::MadeWritable => report.made_writable += 1,
            }
        }
    }
    Ok(report)
}

fn open_child_directory(
    parent: &OpenDirectory,
    name: &OsStr,
) -> Result<Option<OpenDirectory>, SoldrError> {
    let display_path = parent.display_path.join(name);
    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    let file = match parent.dir.open_with(name, &options) {
        Ok(file) => file.into_std(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_error) if entry_is_now_symlink(&parent.dir, Path::new(name)) => return Ok(None),
        Err(error) => {
            return Err(detach_error(
                &display_path,
                "open target subdirectory",
                error,
            ))
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| detach_error(&display_path, "inspect target subdirectory", error))?;
    if !metadata.is_dir() || is_reparse_or_symlink(&metadata) {
        return Ok(None);
    }
    let dir = Dir::from_std_file(file);
    Ok(Some(OpenDirectory {
        dir,
        relative_path: parent.relative_path.join(name),
        display_path,
    }))
}

fn snapshot_directory(
    directory: &OpenDirectory,
    operation: &str,
) -> Result<Vec<OsString>, SoldrError> {
    let entries = match directory.dir.entries() {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(detach_error(&directory.display_path, operation, error)),
    };
    let mut names = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => names.push(entry.file_name()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(detach_error(
                    &directory.display_path,
                    "read target entry",
                    error,
                ))
            }
        }
    }
    names.sort_unstable();
    Ok(names)
}

fn is_detach_temp(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with(DETACH_TEMP_PREFIX))
}

/// Discover locks first, then acquire them in a stable order and retain the
/// file guards through the full detach. Refusing is safer than racing an
/// active compiler while replacing its outputs.
fn acquire_build_locks(root: &OpenDirectory) -> Result<Vec<File>, SoldrError> {
    use fs2::FileExt;

    let mut pending = vec![OpenDirectory {
        dir: root
            .dir
            .try_clone()
            .map_err(|error| detach_error(&root.display_path, "clone target capability", error))?,
        relative_path: PathBuf::new(),
        display_path: root.display_path.clone(),
    }];
    let mut lock_paths = Vec::new();
    while let Some(directory) = pending.pop() {
        for name in snapshot_directory(&directory, "inspect build locks")? {
            if is_detach_temp(&name) {
                continue;
            }
            let display_path = directory.display_path.join(&name);
            let metadata = match directory.dir.symlink_metadata(&name) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(detach_error(
                        &display_path,
                        "inspect build lock entry",
                        error,
                    ))
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                if let Some(child) = open_child_directory(&directory, &name)? {
                    pending.push(child);
                }
            } else if metadata.is_file() && name == std::ffi::OsStr::new(".cargo-lock") {
                lock_paths.push(directory.relative_path.join(name));
            }
        }
    }
    lock_paths.sort_unstable();

    let mut guards = Vec::with_capacity(lock_paths.len());
    for path in lock_paths {
        let display_path = root.display_path.join(&path);
        let file = match open_lock_no_follow(&root.dir, &path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_error) if entry_is_now_symlink(&root.dir, &path) => continue,
            Err(error) => return Err(detach_error(&display_path, "open build lock", error)),
        };
        match file.try_lock_exclusive() {
            Ok(()) => guards.push(file),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                return Err(active_lock_error(&root.display_path, &display_path));
            }
            Err(error) => return Err(detach_error(&display_path, "acquire build lock", error)),
        }
    }
    Ok(guards)
}

fn active_lock_error(target_dir: &Path, lock_path: &Path) -> SoldrError {
    SoldrError::Other(format!(
        "no-cache preflight refused to modify {} while build lock {} is active",
        target_dir.display(),
        lock_path.display(),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedFile {
    Unchanged,
    DetachedShared,
    MadeWritable,
}

fn prepare_file(parent: &OpenDirectory, name: &OsStr) -> Result<PreparedFile, SoldrError> {
    prepare_file_with_final_rename(parent, name, |directory, temp_name, final_name| {
        directory.rename(temp_name, directory, final_name)
    })
}

fn prepare_file_with_final_rename(
    parent: &OpenDirectory,
    name: &OsStr,
    final_rename: impl FnOnce(&Dir, &OsStr, &OsStr) -> std::io::Result<()>,
) -> Result<PreparedFile, SoldrError> {
    let display_path = parent.display_path.join(name);
    let mut source = match open_source_for_detach(&parent.dir, Path::new(name)) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PreparedFile::Unchanged)
        }
        Err(_error) if entry_is_now_symlink(&parent.dir, Path::new(name)) => {
            return Ok(PreparedFile::Unchanged)
        }
        Err(error) => return Err(detach_error(&display_path, "open target file", error)),
    };
    let metadata = source
        .metadata()
        .map_err(|error| detach_error(&display_path, "inspect target file", error))?;
    if !metadata.is_file() || is_reparse_or_symlink(&metadata) {
        return Ok(PreparedFile::Unchanged);
    }
    let links = hard_link_count(&source, &metadata)
        .map_err(|error| detach_error(&display_path, "read target hard-link count", error))?;
    if links <= 1 {
        if metadata.permissions().readonly() {
            make_file_writable(&source, &metadata).map_err(|error| {
                detach_error(&display_path, "make private target file writable", error)
            })?;
            return Ok(PreparedFile::MadeWritable);
        }
        return Ok(PreparedFile::Unchanged);
    }

    let (temp_name, mut temp) = create_private_temp(&parent.dir, name)
        .map_err(|error| detach_error(&display_path, "create detach temporary file", error))?;
    let prepare_result = (|| {
        std::io::copy(&mut source, &mut temp)?;
        temp.flush()?;
        set_private_permissions(&temp, &metadata)?;
        filetime::set_file_handle_times(
            &temp,
            Some(filetime::FileTime::from_last_access_time(&metadata)),
            Some(filetime::FileTime::from_last_modification_time(&metadata)),
        )?;
        temp.sync_all()?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(error) = prepare_result {
        drop(temp);
        let _ = parent.dir.remove_file(&temp_name);
        return Err(detach_error(
            &display_path,
            "detach shared target file",
            error,
        ));
    }

    if let Err(error) = remove_shared_alias(&parent.dir, name, source) {
        drop(temp);
        let _ = parent.dir.remove_file(&temp_name);
        return Err(detach_error(
            &display_path,
            "detach shared target file",
            error,
        ));
    }

    // The original directory entry no longer exists. From this point onward,
    // the temporary is the sole private copy and must never be cleaned up on
    // error. The capability-relative rename is atomic when it succeeds; when
    // it fails, retain the copy under its exact reported recovery path.
    drop(temp);
    if let Err(error) = final_rename(&parent.dir, &temp_name, name) {
        let preserved_path = parent.display_path.join(&temp_name);
        return Err(SoldrError::Other(format!(
            "no-cache preflight could not finalize detached target file {}: {error}; the shared alias was removed and the private copy was preserved at {}",
            display_path.display(),
            preserved_path.display(),
        )));
    }
    Ok(PreparedFile::DetachedShared)
}

fn create_private_temp(parent: &Dir, name: &OsStr) -> std::io::Result<(OsString, File)> {
    let name = name.to_string_lossy();
    let mut last_error = None;
    for _ in 0..32 {
        let counter = DETACH_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_name = OsString::from(format!(
            "{DETACH_TEMP_PREFIX}{}-{counter}-{name}",
            std::process::id(),
        ));
        let mut options = CapOpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        match parent.open_with(&temp_name, &options) {
            Ok(file) => return Ok((temp_name, file.into_std())),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "exhausted no-cache detach temporary names",
        )
    }))
}

fn detach_error(path: &Path, operation: &str, error: std::io::Error) -> SoldrError {
    SoldrError::Other(format!(
        "no-cache preflight could not {operation} {}: {error}",
        path.display(),
    ))
}

fn entry_is_now_symlink(parent: &Dir, name: &Path) -> bool {
    parent
        .symlink_metadata(name)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn open_cap_file_no_follow(parent: &Dir, name: &Path, write: bool) -> std::io::Result<File> {
    let mut options = CapOpenOptions::new();
    options.read(true).write(write).follow(FollowSymlinks::No);
    parent.open_with(name, &options).map(|file| file.into_std())
}

#[cfg(unix)]
fn open_source_for_detach(parent: &Dir, name: &Path) -> std::io::Result<File> {
    open_cap_file_no_follow(parent, name, false)
}

#[cfg(windows)]
fn open_source_for_detach(parent: &Dir, name: &Path) -> std::io::Result<File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        ReOpenFile, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };

    const DELETE_ACCESS: u32 = 0x0001_0000;
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
    let initial = open_cap_file_no_follow(parent, name, false)?;
    // ReOpenFile upgrades the already-resolved capability handle rather than
    // resolving an ambient path again, preserving the beneath-root guarantee.
    let handle = unsafe {
        ReOpenFile(
            initial.as_raw_handle() as _,
            GENERIC_READ | DELETE_ACCESS | FILE_WRITE_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: ReOpenFile returned a new owned handle on success.
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

fn open_lock_no_follow(parent: &Dir, name: &Path) -> std::io::Result<File> {
    let file = open_cap_file_no_follow(parent, name, true)?;
    if is_reparse_or_symlink(&file.metadata()?) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "build lock is a reparse point",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn is_reparse_or_symlink(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn is_reparse_or_symlink(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(unix)]
fn hard_link_count(_source: &File, metadata: &std::fs::Metadata) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(metadata.nlink())
}

#[cfg(windows)]
fn hard_link_count(source: &File, _metadata: &std::fs::Metadata) -> std::io::Result<u64> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `source` owns a valid file handle, and `info` points to enough
    // writable storage for BY_HANDLE_FILE_INFORMATION.
    if unsafe { GetFileInformationByHandle(source.as_raw_handle() as _, info.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful call initialized the full structure.
    Ok(u64::from(unsafe { info.assume_init() }.nNumberOfLinks))
}

#[cfg(unix)]
fn set_private_permissions(file: &File, source: &std::fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    file.set_permissions(std::fs::Permissions::from_mode(source.mode() | 0o200))
}

#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)] // Windows clears only FILE_ATTRIBUTE_READONLY.
fn set_private_permissions(file: &File, source: &std::fs::Metadata) -> std::io::Result<()> {
    let mut permissions = source.permissions();
    permissions.set_readonly(false);
    file.set_permissions(permissions)
}

fn make_file_writable(file: &File, metadata: &std::fs::Metadata) -> std::io::Result<()> {
    set_private_permissions(file, metadata)
}

#[cfg(unix)]
fn remove_shared_alias(parent: &Dir, name: &OsStr, source: File) -> std::io::Result<()> {
    drop(source);
    parent.remove_file(name)
}

#[cfg(windows)]
fn remove_shared_alias(_parent: &Dir, _name: &OsStr, source: File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfoEx, SetFileInformationByHandle, FILE_DISPOSITION_FLAG_DELETE,
        FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX,
    };

    let mut disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: `source` was opened with DELETE access and remains alive for
    // the call. `disposition` has the exact layout required by
    // FileDispositionInfoEx.
    let success = unsafe {
        SetFileInformationByHandle(
            source.as_raw_handle() as _,
            FileDispositionInfoEx,
            std::ptr::from_mut(&mut disposition).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if success == 0 {
        let error = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            error.kind(),
            format!(
                "safe read-only hardlink detachment requires FileDispositionInfoEx; refusing an unsafe attribute-clearing fallback: {error}"
            ),
        ));
    }
    drop(source);
    Ok(())
}

#[cfg(test)]
mod tests;
