//! Target-scoped Cargo override for managed static liblzma bundles.

use super::{toml_string, BlessedPrep};
use crate::core::SoldrPaths;

pub(super) async fn inject(paths: &SoldrPaths, target_triple: &str, prep: &mut BlessedPrep) {
    match crate::fetch::lzma_sysroot::ensure_lzma_sysroot(paths, target_triple).await {
        Ok(sysroot) => {
            super::prepend_pkg_config_path_for_target(prep, target_triple, &sysroot);
            // Managed Linux bundles contain only a static liblzma archive.
            // pkg-config's unqualified `-llzma` is rejected by rust-lld's
            // cross-target no-fallback policy, so select it explicitly.
            if target_triple.contains("-unknown-linux-") {
                add_static_links_override(prep, target_triple, &sysroot);
            }
        }
        Err(error) => super::log_sys_unavailable("lzma", target_triple, &error),
    }
}

fn add_static_links_override(
    prep: &mut BlessedPrep,
    target_triple: &str,
    sysroot: &std::path::Path,
) {
    let table = format!("target.{target_triple}.lzma");
    let lib_dir = sysroot.join("lib");
    let include_dir = sysroot.join("include");
    prep.cargo_args.extend([
        "--config".to_string(),
        format!("{table}.rustc-link-lib=[\"static=lzma\"]"),
        "--config".to_string(),
        format!(
            "{table}.rustc-link-search=[{}]",
            toml_string(&lib_dir.to_string_lossy())
        ),
        "--config".to_string(),
        format!(
            "{table}.metadata_root={}",
            toml_string(&sysroot.to_string_lossy())
        ),
        "--config".to_string(),
        format!(
            "{table}.metadata_include={}",
            toml_string(&include_dir.to_string_lossy())
        ),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(uses_static_target_config, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let sysroot = tmp.path().join("lzma sysroot");
        let mut prep = BlessedPrep::default();

        add_static_links_override(&mut prep, "aarch64-unknown-linux-gnu", &sysroot);

        assert_eq!(prep.cargo_args.len(), 8);
        assert_eq!(
            prep.cargo_args[1],
            "target.aarch64-unknown-linux-gnu.lzma.rustc-link-lib=[\"static=lzma\"]"
        );
        assert!(prep.cargo_args[3]
            .starts_with("target.aarch64-unknown-linux-gnu.lzma.rustc-link-search=[\""));
        assert!(prep.cargo_args[3].contains("lzma sysroot"));
        assert!(prep.cargo_args[5]
            .starts_with("target.aarch64-unknown-linux-gnu.lzma.metadata_root=\""));
        assert!(prep.cargo_args[7]
            .starts_with("target.aarch64-unknown-linux-gnu.lzma.metadata_include=\""));
    });
}
