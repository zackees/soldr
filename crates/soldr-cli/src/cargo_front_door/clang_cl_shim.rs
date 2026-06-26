//! Write a `clang` shim that forces `--driver-mode=cl` for soldr's
//! `cargo xwin build --target *-pc-windows-msvc` dispatch.
//!
//! Why this exists (ring + cc-rs + cargo-xwin tri-incompatibility):
//!
//! ring 0.17.14's `build.rs` has a hard-coded
//!     `let _ = c.compiler("clang");`
//! call for `aarch64-pc-windows-msvc` when cc-rs's detected compiler
//! isn't `is_like_clang()`. cc-rs classifies `clang-cl` as family
//! `Msvc` (not `Clang`), so even when CC_<triple>=clang-cl is set,
//! ring overrides the compiler back to plain `clang`. cc-rs then
//! invokes the PATH-resolved `clang` binary with cargo-xwin's
//! MSVC-style `/imsvc` include flags. Plain clang's GNU driver
//! doesn't understand `/imsvc` and fails with
//!     `clang: error: no such file or directory: '/imsvc'`
//!
//! Setting `CC_<triple>` in the env doesn't help — ring's
//! `c.compiler("clang")` overrides whatever cc-rs picked up from env.
//!
//! The fix is a tiny shim NAMED `clang` placed on PATH ahead of the
//! real clang. ring's `c.compiler("clang")` triggers a PATH lookup
//! that finds our shim first. The shim `exec`s the real clang with
//! `--driver-mode=cl` prepended, putting clang into MSVC-compatibility
//! mode where `/imsvc` parses correctly. Result: ring's build script
//! succeeds without any user-facing env wiring.
//!
//! Soldr's "bootstrapper" charter: this is the only sane place to put
//! this fix. Pushing it into workflow YAML or per-user env setup would
//! mean every consumer of `soldr cargo xwin build` has to know about
//! ring's quirk. Doing it inside soldr keeps the user-facing surface
//! clean — they just type `soldr cargo xwin build --target ...` and it
//! works on a stock device with no dependencies.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

const SHIM_DIR_BASENAME: &str = "xwin-clang-shim";

/// Ensure a `clang` shim exists at `~/.soldr/bin/xwin-clang-shim/`
/// (or platform-equivalent under `paths.bin`) and return that directory
/// so the caller can prepend it to PATH for the child cargo subprocess.
///
/// The shim is a tiny one-line script that `exec`s the real PATH-resolved
/// clang with `--driver-mode=cl` prepended. It's regenerated only when
/// the real clang's path changes (idempotent across runs).
pub fn ensure_clang_cl_shim(paths: &SoldrPaths) -> Result<PathBuf, SoldrError> {
    let dir = paths.bin.join(SHIM_DIR_BASENAME);
    std::fs::create_dir_all(&dir)?;

    let real_clang = discover_real_clang(&dir)?;
    write_clang_cl_shim(&dir, &real_clang)
}

pub fn ensure_clang_cl_shim_for_real_clang(
    paths: &SoldrPaths,
    real_clang: &Path,
) -> Result<PathBuf, SoldrError> {
    if !real_clang.is_file() {
        return Err(SoldrError::Other(format!(
            "managed clang not found at {}",
            real_clang.display()
        )));
    }
    let dir = paths.bin.join(SHIM_DIR_BASENAME);
    std::fs::create_dir_all(&dir)?;
    write_clang_cl_shim(&dir, real_clang)
}

fn write_clang_cl_shim(dir: &Path, real_clang: &Path) -> Result<PathBuf, SoldrError> {
    let shim_path = dir.join(if cfg!(windows) { "clang.cmd" } else { "clang" });
    let shim_body = render_shim_body(real_clang);

    let existing = std::fs::read_to_string(&shim_path).ok();
    // `Some(shim_body.as_str())` is spelled out (rather than the more idiomatic
    // `Some(&shim_body)`) because the mandatory `zccache` git dep (used by the
    // daemon's embedded service) pulls in `rkyv` whose blanket `PartialEq`
    // impls for `Option<T>` defeat deref coercion from `&String` to `&str` at
    // this comparison site.
    if existing.as_deref() != Some(shim_body.as_str()) {
        std::fs::write(&shim_path, &shim_body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&shim_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&shim_path, perms)?;
        }
    }

    Ok(dir.to_path_buf())
}

fn render_shim_body(real_clang: &Path) -> String {
    if cfg!(windows) {
        // Windows .cmd shim. `%*` forwards all args.
        format!(
            "@echo off\r\n\"{}\" --driver-mode=cl %*\r\n",
            real_clang.display(),
        )
    } else {
        format!(
            "#!/bin/sh\nexec \"{}\" --driver-mode=cl \"$@\"\n",
            real_clang.display(),
        )
    }
}

/// Walk PATH for a `clang` binary, EXCLUDING the shim directory itself
/// (otherwise the shim would invoke itself recursively when re-run after
/// it's already on PATH). Returns the first hit.
fn discover_real_clang(skip_dir: &Path) -> Result<PathBuf, SoldrError> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let exe_name = if cfg!(windows) { "clang.exe" } else { "clang" };
    let skip_dir_canon = std::fs::canonicalize(skip_dir).unwrap_or_else(|_| skip_dir.to_path_buf());
    for dir in std::env::split_paths(&path) {
        // Skip our own shim dir + Windows `PATHEXT` resolution quirks.
        let dir_canon = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if dir_canon == skip_dir_canon {
            continue;
        }
        let candidate = dir.join(exe_name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(SoldrError::Other(format!(
        "could not find `{exe_name}` on PATH — install LLVM / clang to \
         cross-compile to *-pc-windows-msvc via cargo-xwin"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_body_unix_uses_real_clang_path_with_driver_mode_cl() {
        let body = if cfg!(windows) {
            // On Windows the format differs; just verify both helpers
            // produce non-empty output. The body verified below.
            return;
        } else {
            render_shim_body(Path::new("/usr/bin/clang"))
        };
        assert!(body.starts_with("#!/bin/sh\n"));
        assert!(body.contains("--driver-mode=cl"));
        assert!(body.contains("/usr/bin/clang"));
        assert!(body.contains("\"$@\""));
    }

    #[test]
    fn shim_body_windows_quotes_path_and_forwards_args() {
        if !cfg!(windows) {
            return;
        }
        let body = render_shim_body(Path::new(r"C:\Program Files\LLVM\bin\clang.exe"));
        assert!(body.contains("--driver-mode=cl"));
        assert!(body.contains("%*"));
        assert!(body.contains(r"C:\Program Files\LLVM\bin\clang.exe"));
    }
}
