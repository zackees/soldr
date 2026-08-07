//! PATH placement for `soldr install` (soldr#2310).
//!
//! Installed tools land in a *new* user-facing bin dir
//! `~/.soldr/bin/installed/<name>/` (today only the five toolchain shims
//! reach PATH). The built binary is materialized there via the single
//! blessed writer [`crate::shim_materialize::materialize_executable`],
//! and the user is warned if that dir is not on `PATH`.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};
use crate::shim_materialize::materialize_executable;

use super::plan::{paint_yellow, use_color};

/// Resolve the install root (the PATH bin dir): `--root` or
/// `<paths.bin>/installed`.
pub(crate) fn install_root(paths: &SoldrPaths, root_override: Option<&Path>) -> PathBuf {
    match root_override {
        Some(dir) => dir.to_path_buf(),
        None => paths.bin.join("installed"),
    }
}

/// `.exe` on Windows targets, empty otherwise. Mirrors
/// `build_from_source_cmd::binary_ext_for_triple`.
pub(crate) fn binary_ext_for_triple(triple: &str) -> &'static str {
    if triple.contains("-pc-windows-") {
        ".exe"
    } else {
        ""
    }
}

/// Placement outcome: the final on-PATH binary path.
#[derive(Debug, Clone)]
pub(crate) struct Placement {
    pub binary: PathBuf,
    pub on_path: bool,
}

/// Materialize `built_binary` into `<install_root>/<name>/<name>[.exe]`.
pub(crate) fn place_binary(
    name: &str,
    built_binary: &Path,
    install_root: &Path,
    triple: &str,
    force: bool,
) -> Result<Placement, SoldrError> {
    let tool_dir = install_root.join(name);
    let target = tool_dir.join(format!("{name}{}", binary_ext_for_triple(triple)));

    if target.exists() && !force {
        return Err(SoldrError::Other(format!(
            "install: {} already exists; pass --force to overwrite",
            target.display()
        )));
    }
    std::fs::create_dir_all(&tool_dir)?;
    materialize_executable(built_binary, &target)?;

    let on_path = install_root_on_path(install_root);
    if !on_path {
        eprintln!(
            "soldr: {}",
            paint_yellow(
                &format!(
                    "warning: {} is not on PATH; add it to run '{name}' directly",
                    install_root.display()
                ),
                use_color(),
            )
        );
    }
    Ok(Placement {
        binary: target,
        on_path,
    })
}

/// True when `install_root` is a component of the current `PATH`.
fn install_root_on_path(install_root: &Path) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|p| paths_equal(&p, install_root))
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    // Best-effort canonicalization; fall back to a literal compare so a
    // not-yet-created dir still matches a PATH entry spelled the same way.
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_root_defaults_to_bin_installed() {
        let paths = SoldrPaths::with_root(PathBuf::from("/home/x/.soldr"));
        assert_eq!(
            install_root(&paths, None),
            PathBuf::from("/home/x/.soldr/bin/installed")
        );
        let custom = PathBuf::from("/opt/tools");
        assert_eq!(install_root(&paths, Some(&custom)), custom);
    }

    #[test]
    fn binary_ext_matches_triple() {
        assert_eq!(binary_ext_for_triple("x86_64-pc-windows-msvc"), ".exe");
        assert_eq!(binary_ext_for_triple("x86_64-unknown-linux-gnu"), "");
    }

    crate::timed_test!(place_binary_lands_executable_and_respects_force, {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("mytool");
        std::fs::write(&src, b"#!/bin/sh\necho hi\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let root = tmp.path().join("installed");
        let placement =
            place_binary("mytool", &src, &root, "x86_64-unknown-linux-gnu", false).unwrap();
        assert!(placement.binary.is_file());
        // Second placement without --force must error.
        let err = place_binary("mytool", &src, &root, "x86_64-unknown-linux-gnu", false)
            .expect_err("must refuse to overwrite without --force");
        assert!(format!("{err}").contains("--force"), "{err}");
        // With --force it succeeds.
        place_binary("mytool", &src, &root, "x86_64-unknown-linux-gnu", true).unwrap();
    });
}
