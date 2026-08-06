//! Target-scoped Cargo override for managed static liblzma bundles.

use super::{links_provider, toml_string, BlessedPrep};
use crate::core::SoldrPaths;

/// The crate the managed liblzma bundle was cut to match.
const LZMA_SYS_CRATE: &str = "lzma-sys";

pub(super) async fn inject(paths: &SoldrPaths, target_triple: &str, prep: &mut BlessedPrep) {
    match crate::fetch::lzma_sysroot::ensure_lzma_sysroot(paths, target_triple).await {
        Ok(sysroot) => {
            super::prepend_pkg_config_path_for_target(prep, target_triple, &sysroot);
            // Managed Linux bundles contain only a static liblzma archive.
            // pkg-config's unqualified `-llzma` is rejected by rust-lld's
            // cross-target no-fallback policy, so select it explicitly.
            //
            // Unlike the pkg-config path above — advice a build script
            // may ignore — this override *replaces* the build script, so
            // it may only be applied when the crate claiming
            // `links = "lzma"` is the one the bundle matches. Same
            // reasoning as the mimalloc gate; see soldr#2142.
            if target_triple.contains("-unknown-linux-")
                && links_provider::resolve(
                    &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                    "lzma",
                    target_triple,
                )
                .is(LZMA_SYS_CRATE)
            {
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

    // `soldr build` carries the links override below as Cargo CLI config.
    // `soldr prepare --github-env`, however, is also a public way to hand the
    // blessed toolchain to direct Cargo consumers such as Maturin. Export the
    // matching search path through the target's Rust flags so that a normal
    // `-llzma` emitted by lzma-sys resolves the catalogue's static archive.
    // target_lifecycle promotes this value into CARGO_ENCODED_RUSTFLAGS, which
    // preserves paths containing spaces and gives it Cargo's highest priority.
    let rustflags_key = format!(
        "CARGO_TARGET_{}_RUSTFLAGS",
        target_triple.replace('-', "_").to_ascii_uppercase()
    );
    let link_search = format!("-Lnative={}", lib_dir.display());
    if let Some((_, rustflags)) = prep.env.iter_mut().find(|(key, _)| key == &rustflags_key) {
        rustflags.push(' ');
        rustflags.push_str(&link_search);
    } else {
        prep.env.push((rustflags_key, link_search));
    }

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
        prep.env.push((
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS".to_string(),
            "-C link-arg=--sysroot=/catalogue/sysroot".to_string(),
        ));

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
        assert_eq!(
            prep.env,
            vec![(
                "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS".to_string(),
                format!(
                    "-C link-arg=--sysroot=/catalogue/sysroot -Lnative={}",
                    sysroot.join("lib").display()
                ),
            )]
        );
    });
}
