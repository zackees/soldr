//! Linux fs implementation: permission primitives.

use std::path::Path;

/// Restore an archived Unix mode onto `path`. `None` means the archive
/// carried no mode — nothing to apply.
pub fn restore_mode(path: &Path, mode: Option<u32>) -> std::io::Result<()> {
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Adopt the source's full mode onto `file` with the owner-write bit
/// added. A freshly created private copy lands at the umask default
/// (0o644) and must carry the original's permission set — including the
/// execute bits — instead of its own.
pub fn make_writable_like(
    file: &std::fs::File,
    source: &std::fs::Permissions,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(source.mode() | 0o200))
}

/// Make `path` executable by applying a fixed 0o755, deliberately
/// umask-independent: OR-ing exec bits onto a umask-derived base (the
/// prior behavior) produced 0o777 world-writable shims under a
/// container root's umask 0000 (issue #3327).
pub fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Publish permissions for a materialized executable: the published
/// shim must be runnable regardless of the source's own mode, so apply
/// a fixed 0o755.
pub fn make_executable_from(path: &Path, _source: &std::fs::Permissions) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Restrict a directory to its owner (0o700).
pub fn make_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// Read the file's Unix permission bits (e.g. `0o755`). `None` when the
/// metadata read fails; on hosts without mode semantics (Windows) the
/// concrete tree always returns `None`, letting callers branch at
/// runtime instead of by cfg.
pub fn mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).ok().map(|m| m.permissions().mode())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn make_writable_like_adopts_source_exec_bits() {
        // A freshly created private copy lands at the umask default
        // (0o644); the source's exec bits must survive the adoption.
        let path = std::env::temp_dir().join(format!(
            "soldr-platform-make-writable-like-{}-{}",
            std::process::id(),
            "exec"
        ));
        let file = std::fs::File::create(&path).unwrap();
        let source = std::fs::Permissions::from_mode(0o755);
        make_writable_like(&file, &source).unwrap();
        drop(file);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        let _ = std::fs::remove_file(&path);
        assert_eq!(mode & 0o7777, 0o755);
    }

    #[test]
    fn make_writable_like_keeps_private_modes_private() {
        // A 0o600 source stays 0o600: only the owner-write bit is added,
        // group/other bits are not invented.
        let path = std::env::temp_dir().join(format!(
            "soldr-platform-make-writable-like-{}-{}",
            std::process::id(),
            "private"
        ));
        let file = std::fs::File::create(&path).unwrap();
        let source = std::fs::Permissions::from_mode(0o600);
        make_writable_like(&file, &source).unwrap();
        drop(file);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        let _ = std::fs::remove_file(&path);
        assert_eq!(mode & 0o7777, 0o600);
    }

    #[test]
    fn make_executable_is_umask_independent() {
        // chmod (set_permissions), not a plain file create, is the point:
        // umask masks a create's requested bits, so a freshly created file
        // can never actually reach 0o666 under a permissive CI umask (022).
        // Explicitly chmod-ing to 0o666 reproduces the exact base that the
        // old `mode | 0o111` logic saw under umask 0000 (root in
        // containers), so this test reproduces the 0o777 regression even
        // on a CI host with umask 022. Do not "simplify" this back to a
        // plain create — that would make the test pass unconditionally
        // and CI could never turn red again.
        let path = std::env::temp_dir().join(format!(
            "soldr-platform-make-executable-{}-{}",
            std::process::id(),
            "umask-independent"
        ));
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        make_executable(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        let _ = std::fs::remove_file(&path);
        // Old code: 0o666 | 0o111 = 0o777 (world-writable, fails).
        // New code: fixed 0o755 (passes).
        assert_eq!(mode & 0o777, 0o755);
    }
}
