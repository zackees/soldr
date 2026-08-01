//! Target-scoped Cargo build-script override for managed mimalloc.
//!
//! `libmimalloc-sys` declares `links = "mimalloc"` but exposes no
//! target-scoped env hook, so unlike its `PKG_CONFIG_PATH_<triple>`
//! siblings it cannot be *offered* a sysroot — the only lever is
//! Cargo's build-script override, which skips `build.rs` entirely and
//! supplies the link metadata directly.
//!
//! That makes this injection unusual: a pkg-config path is advice a
//! build script may ignore, whereas an override replaces the build
//! script outright. It is therefore also the only injection here that
//! can substitute the *wrong* library, which is what soldr#2142 is —
//! see [`super::links_provider`] for why `links` is not a safe lookup
//! key on its own.

use super::{links_provider, toml_string, BlessedPrep};
use crate::core::SoldrPaths;

/// The crate soldr's prebuilt mimalloc was cut to match.
const MIMALLOC_SYS_CRATE: &str = "libmimalloc-sys";

pub(super) async fn inject(paths: &SoldrPaths, target_triple: &str, prep: &mut BlessedPrep) {
    if !override_applies(target_triple) {
        return;
    }
    match crate::fetch::mimalloc_sysroot::ensure_mimalloc_sysroot(paths, target_triple).await {
        Ok(sysroot) => add_build_script_override(prep, target_triple, &sysroot),
        Err(error) => super::log_sys_unavailable("mimalloc", target_triple, &error),
    }
}

/// Whether substituting the managed upstream mimalloc is sound for the
/// graph being built.
///
/// Sound only when `libmimalloc-sys` is the sole package claiming
/// `links = "mimalloc"`. A fork such as `mimalloc-pprof` vendors a
/// patched mimalloc and exports a superset of its API, so serving it
/// upstream's binary drops every symbol the fork added and the link
/// fails. When the graph cannot be resolved we skip rather than guess:
/// the crate's own vendored compile still produces a working build,
/// which a mis-substitution does not.
fn override_applies(target_triple: &str) -> bool {
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let provider = links_provider::resolve(&workspace_root, "mimalloc", target_triple);
    match provider {
        links_provider::LinksProvider::Package(name) if name == MIMALLOC_SYS_CRATE => true,
        // Nothing claims the name — the override would be inert, and
        // skipping saves materializing the sysroot too.
        links_provider::LinksProvider::Absent => false,
        links_provider::LinksProvider::Package(name) => {
            eprintln!(
                "soldr build: not substituting the managed mimalloc for {target_triple}: \
                 `links = \"mimalloc\"` is provided by `{name}`, not `{MIMALLOC_SYS_CRATE}`"
            );
            eprintln!(
                "soldr build: `{name}` will build its own vendored copy, which is what a \
                 mimalloc fork needs (soldr#2142)"
            );
            false
        }
        links_provider::LinksProvider::Unknown(reason) => {
            eprintln!(
                "soldr build: not substituting the managed mimalloc for {target_triple}: \
                 could not determine which crate provides `links = \"mimalloc\"` ({reason})"
            );
            false
        }
    }
}

fn add_build_script_override(
    prep: &mut BlessedPrep,
    target_triple: &str,
    sysroot: &std::path::Path,
) {
    let table = format!("target.{target_triple}.mimalloc");
    let lib_dir = sysroot.join("lib");
    let include_dir = sysroot.join("include");
    prep.cargo_args.extend([
        "--config".to_string(),
        format!("{table}.rustc-link-lib=[\"static=mimalloc\"]"),
        "--config".to_string(),
        format!(
            "{table}.rustc-link-search=[{}]",
            toml_string(&lib_dir.to_string_lossy())
        ),
        "--config".to_string(),
        format!(
            "{table}.metadata_include_dir={}",
            toml_string(&include_dir.to_string_lossy())
        ),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(build_script_override_uses_target_config, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let sysroot = tmp.path().join("mimalloc sysroot");
        let mut prep = BlessedPrep::default();

        add_build_script_override(&mut prep, "x86_64-unknown-linux-gnu", &sysroot);

        assert_eq!(prep.cargo_args.len(), 6);
        assert_eq!(prep.cargo_args[0], "--config");
        assert_eq!(
            prep.cargo_args[1],
            "target.x86_64-unknown-linux-gnu.mimalloc.rustc-link-lib=[\"static=mimalloc\"]"
        );
        assert_eq!(prep.cargo_args[2], "--config");
        assert!(prep.cargo_args[3]
            .starts_with("target.x86_64-unknown-linux-gnu.mimalloc.rustc-link-search=[\""));
        assert!(prep.cargo_args[3].contains("mimalloc sysroot"));
        assert_eq!(prep.cargo_args[4], "--config");
        assert!(prep.cargo_args[5]
            .starts_with("target.x86_64-unknown-linux-gnu.mimalloc.metadata_include_dir=\""));
    });
}
