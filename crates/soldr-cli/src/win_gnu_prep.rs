//! Win-gnu (`x86_64-pc-windows-gnu`) toolchain preparation + restore auditing.
//!
//! Split out of `blessed_build` / `prepare_cmd` in soldr#2336 so the two
//! host-dependent shapes of the win-gnu toolchain live in one place:
//!
//! * **Windows x64 host** — the WinLibs `mingw-w64-gcc` bundle: a full
//!   `gcc`/binutils toolchain, prepended to PATH with target-scoped
//!   Cargo/cc-rs env.
//! * **any other host** (soldr#2336 / soldr-toolchain#114) — the host-neutral
//!   `mingw-w64-sysroot` (headers + import libs + CRT, no host executables).
//!   A non-Windows host cannot run the `.exe` toolchain, but a consumer that
//!   brings its own PE-COFF linker (e.g. [reld]) can link against this sysroot.
//!   soldr provides the sysroot and publishes its discovery env; it does not
//!   run a link on this host.
//!
//! [reld]: https://github.com/zackees/reld

use std::path::Path;

use crate::blessed_build::BlessedPrep;
use crate::core::{SoldrError, SoldrPaths};
use crate::prepare_cmd::RestoreEntry;

/// Materialize the win-gnu toolchain for `target_triple` and inject its env
/// onto `prep`. Dispatches on the host: gcc bundle on Windows x64, host-neutral
/// sysroot elsewhere.
pub(crate) async fn prepare_env(
    paths: &SoldrPaths,
    target_triple: &str,
    prep: &mut BlessedPrep,
) -> Result<(), SoldrError> {
    if crate::fetch::mingw_w64_gcc::current_host_supports_mingw_w64_gcc() {
        let mingw_root =
            crate::fetch::mingw_w64_gcc::ensure_mingw_w64_gcc(paths, target_triple).await?;
        add_mingw_w64_gcc_env(prep, target_triple, &mingw_root);
    } else {
        // soldr#2336 / soldr-toolchain#114: a non-Windows host cannot run the
        // WinLibs `.exe` toolchain, but it CAN consume the host-neutral sysroot
        // with a linker it brings itself (reld bridges to lld for PE-COFF).
        // Materialize that sysroot and publish its discovery env instead of
        // hard-erroring, which is what blocked the Linux-hosted win-gnu path.
        let sysroot_root =
            crate::fetch::mingw_w64_sysroot::ensure_mingw_w64_sysroot(paths, target_triple).await?;
        add_mingw_w64_sysroot_env(prep, &sysroot_root);
        eprintln!(
            "soldr: prepared host-neutral MinGW-w64 sysroot for {target_triple} at {} \
             (non-Windows host: set your own win-gnu linker, e.g. reld, to link)",
            sysroot_root.display()
        );
    }
    Ok(())
}

fn add_mingw_w64_gcc_env(prep: &mut BlessedPrep, target_triple: &str, mingw_root: &Path) {
    prep.path_dirs
        .insert(0, crate::fetch::mingw_w64_gcc::bin_dir(mingw_root));
    prep.env.extend(crate::fetch::mingw_w64_gcc::env_for_target(
        mingw_root,
        target_triple,
    ));
}

/// Publish the host-neutral sysroot's discovery env
/// (`MINGW_W64_SYSROOT_{ROOT,INCLUDE,LIBDIR,GCCLIBDIR}`) so a
/// non-Windows-hosted consumer with its own linker can find the CRT objects,
/// import libraries, and headers. Deliberately non-invasive — no `CC_<t>` /
/// linker override, since there is no host gcc driver to point at.
fn add_mingw_w64_sysroot_env(prep: &mut BlessedPrep, sysroot_root: &Path) {
    prep.env
        .extend(crate::fetch::mingw_w64_sysroot::sysroot_env(sysroot_root));
}

/// The restore-audit entry for the materialized win-gnu toolchain (soldr#2336
/// item 3). Verifies the binutils + sysroot a link actually needs, not just the
/// `gcc` driver, so a truncated restore is reported "missing" rather than
/// failing far later at the link. Host-shaped: the gcc bundle on Windows x64,
/// the host-neutral sysroot elsewhere.
pub(crate) fn restore_entry(paths: &SoldrPaths) -> RestoreEntry {
    if crate::fetch::mingw_w64_gcc::current_host_supports_mingw_w64_gcc() {
        let install = paths
            .bin
            .join("syslib")
            .join(crate::fetch::mingw_w64_gcc::MINGW_W64_GCC_TOOL)
            .join(crate::fetch::mingw_w64_gcc::MANAGED_MINGW_W64_GCC_VERSION)
            .join(crate::fetch::mingw_w64_gcc::MINGW_W64_GCC_SLUG);
        let package = install.join("package");
        RestoreEntry {
            label: format!(
                "MinGW-w64 GCC {}",
                crate::fetch::mingw_w64_gcc::MANAGED_MINGW_W64_GCC_VERSION
            ),
            present: install.join(".complete").is_file()
                && crate::fetch::mingw_w64_gcc::verification_paths(&package)
                    .iter()
                    .all(|p| p.is_file()),
            path: package,
        }
    } else {
        let install = paths
            .bin
            .join("syslib")
            .join(crate::fetch::mingw_w64_sysroot::MINGW_W64_SYSROOT_TOOL)
            .join(crate::fetch::mingw_w64_sysroot::MANAGED_MINGW_W64_SYSROOT_VERSION)
            .join(crate::fetch::mingw_w64_sysroot::MINGW_W64_SYSROOT_SLUG);
        let package = install.join("package");
        RestoreEntry {
            label: format!(
                "MinGW-w64 sysroot {}",
                crate::fetch::mingw_w64_sysroot::MANAGED_MINGW_W64_SYSROOT_VERSION
            ),
            present: install.join(".complete").is_file()
                && crate::fetch::mingw_w64_sysroot::verification_paths(&package)
                    .iter()
                    .all(|p| p.is_file()),
            path: package,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(mingw_w64_gcc_env_injects_target_scoped_tools, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let root = tmp.path().join("mingw");
        let mut prep = BlessedPrep::default();

        add_mingw_w64_gcc_env(&mut prep, "x86_64-pc-windows-gnu", &root);

        assert_eq!(
            prep.path_dirs,
            vec![crate::fetch::mingw_w64_gcc::bin_dir(&root)]
        );
        let names: std::collections::HashSet<&str> =
            prep.env.iter().map(|(name, _)| name.as_str()).collect();
        for required in [
            "MINGW_W64_GCC_ROOT",
            "MINGW_W64_GCC_BIN",
            "CC_x86_64_pc_windows_gnu",
            "CXX_x86_64_pc_windows_gnu",
            "AR_x86_64_pc_windows_gnu",
            "RANLIB_x86_64_pc_windows_gnu",
            "WINDRES_x86_64_pc_windows_gnu",
            "DLLTOOL_x86_64_pc_windows_gnu",
            "CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER",
        ] {
            assert!(names.contains(required), "missing env var {required}");
        }
    });

    crate::timed_test!(mingw_w64_sysroot_env_publishes_discovery_vars, {
        // soldr#2336: on a non-Windows host, win-gnu prep materializes the
        // host-neutral sysroot and publishes discovery env rather than
        // hard-erroring. The materialization itself is exercised end-to-end by
        // the scheduled Linux CI lane; here we pin the non-invasive env
        // contract without a network fetch.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let root = tmp.path().join("mingw-sysroot").join("package");
        let mut prep = BlessedPrep::default();

        add_mingw_w64_sysroot_env(&mut prep, &root);

        let names: std::collections::HashSet<&str> =
            prep.env.iter().map(|(name, _)| name.as_str()).collect();
        for required in [
            "MINGW_W64_SYSROOT_ROOT",
            "MINGW_W64_SYSROOT_INCLUDE",
            "MINGW_W64_SYSROOT_LIBDIR",
            "MINGW_W64_SYSROOT_GCCLIBDIR",
        ] {
            assert!(
                names.contains(required),
                "missing sysroot env var {required}"
            );
        }
        // Non-invasive: it never sets a compiler or linker override, since
        // there is no host gcc driver to point at.
        assert!(!names.contains("CC_x86_64_pc_windows_gnu"));
        assert!(!names.contains("CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER"));
        assert!(prep.path_dirs.is_empty());
    });
}
