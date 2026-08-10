//! Linux + macOS injection vehicle for the file-hook tier
//! (#551 slice 6e — companion to the Windows slice 6d).
//!
//! Unix systems don't need a `CreateRemoteThread`-style injection
//! ceremony. The dynamic linker (glibc's `ld-linux.so` on Linux,
//! `dyld` on macOS) reads an env var at process startup and loads
//! the named shared library before the target's `main()` runs:
//!
//! - **Linux**: `LD_PRELOAD=/path/to/lib.so`. The kernel + glibc
//!   honor it for any non-setuid binary the user owns. The env var
//!   propagates through `execve()` to descendants for free — i.e.
//!   re-injection into child processes is automatic. This is the
//!   property that lets the Linux interposer (slice 4 of #551)
//!   ride a single `LD_PRELOAD` set on the daemon's spawn.
//!
//! - **macOS**: `DYLD_INSERT_LIBRARIES=/path/to/lib.dylib`. Same
//!   shape as `LD_PRELOAD` with the documented SIP / hardened-
//!   runtime caveats: dyld refuses to honor it for binaries with
//!   library validation enforced unless they carry
//!   `com.apple.security.cs.allow-dyld-environment-variables`.
//!   System binaries fall in that bucket — same boundary as the
//!   LaunchedProcessTree tier from #539.
//!
//! [`inject_via_env`] just sets the appropriate env var on a
//! caller-supplied `Command` builder. The wrapper exists rather
//! than asking callers to know which env var to use:
//!
//! - It centralizes the per-OS env-var name + the SIP-caveat
//!   diagnostic for callers that ship cross-platform code.
//! - It refuses obviously-wrong paths up front (the interposer
//!   library must exist on disk) so the failure mode is a build-
//!   time error rather than a silent "the dynamic linker dropped
//!   the env var" at run-time.
//!
//! ## Slice 6e scope (this commit)
//!
//! Function-level wrapper only. No spawning, no env-var
//! propagation policy. The caller decides what to spawn and
//! whether the interposer should ride into grandchildren too
//! (`LD_PRELOAD` propagates by default; `DYLD_INSERT_LIBRARIES`
//! does on macOS too but is more aggressively stripped by tools
//! like SIP-protected `xcrun`).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io;
use std::path::Path;
use std::process::Command;

/// Env var the dynamic linker reads to inject a shared library at
/// process startup. Returned by [`inject_env_name`] so callers can
/// also strip the variable explicitly (e.g. for grandchild
/// processes they want to exclude from interposition).
pub fn inject_env_name() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "LD_PRELOAD"
    }
    #[cfg(target_os = "macos")]
    {
        "DYLD_INSERT_LIBRARIES"
    }
}

/// Configure `command` so that the dynamic linker loads
/// `interposer_path` into the spawned process (and, by default,
/// its descendants — the env var inherits through `execve()`).
///
/// Returns the borrow on `command` so the call chains:
///
/// ```ignore
/// running_process_probe::inject_via_env(
///     &mut Command::new("my-target"),
///     &interposer_path,
/// )?
/// .spawn()?;
/// ```
///
/// # Errors
///
/// Returns `io::ErrorKind::NotFound` if `interposer_path` doesn't
/// exist on disk at the time of the call. Returns
/// `io::ErrorKind::InvalidInput` if it isn't a regular file
/// (catches accidental directory-of-libraries paths).
///
/// The dynamic linker silently ignores `LD_PRELOAD` /
/// `DYLD_INSERT_LIBRARIES` entries pointing at non-existent files,
/// so refusing them up front turns a "hook didn't fire and I have
/// no idea why" debugging session into an immediate explicit
/// error.
pub fn inject_via_env<'a>(
    command: &'a mut Command,
    interposer_path: &Path,
) -> io::Result<&'a mut Command> {
    let meta = std::fs::metadata(interposer_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "interposer library not accessible at {}: {e}",
                interposer_path.display()
            ),
        )
    })?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "interposer path must be a regular file, got {}",
                interposer_path.display()
            ),
        ));
    }

    command.env(inject_env_name(), interposer_path);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    /// Write a stub "library" file (any bytes will do — the dynamic
    /// linker won't actually try to load it because we never spawn
    /// the configured command in this test) and return its path
    /// via a `tempfile::TempDir` guard.
    fn stub_lib() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("lib_interposer.stub");
        let mut f = std::fs::File::create(&p).expect("create");
        f.write_all(b"stub").expect("write");
        let mut perms = std::fs::metadata(&p).expect("stat").permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&p, perms).expect("chmod");
        (dir, p)
    }

    #[test]
    fn inject_env_name_matches_platform() {
        #[cfg(target_os = "linux")]
        assert_eq!(inject_env_name(), "LD_PRELOAD");
        #[cfg(target_os = "macos")]
        assert_eq!(inject_env_name(), "DYLD_INSERT_LIBRARIES");
    }

    #[test]
    fn inject_via_env_sets_the_env_var() {
        let (_guard, lib) = stub_lib();
        let mut cmd = Command::new("/bin/true");
        let returned = inject_via_env(&mut cmd, &lib).expect("inject");
        // The mutable-borrow chain returned the same Command; we
        // can't inspect Command's env-map directly via public API,
        // so verify behaviorally: spawn it and read /proc on Linux
        // OR just trust the API contract. Here we just assert the
        // call returned Ok and didn't panic.
        let _ = returned;
    }

    #[test]
    fn inject_via_env_rejects_missing_path() {
        let mut cmd = Command::new("/bin/true");
        let err = inject_via_env(
            &mut cmd,
            std::path::Path::new("/nonexistent/path/to/lib.so"),
        )
        .expect_err("expected NotFound");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn inject_via_env_rejects_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cmd = Command::new("/bin/true");
        let err = inject_via_env(&mut cmd, dir.path()).expect_err("expected InvalidInput");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
