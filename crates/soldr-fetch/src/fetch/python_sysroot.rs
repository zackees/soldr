//! Explicit target-Python compatibility bundle materialization.
//!
//! Ordinary ABI3 extensions, modern Windows raw-dylib extensions, and
//! Unix extension-module builds do not use this module. The shared PyO3
//! plan calls it only for explicit legacy or embedding compatibility mode.
//! It selects the newest target row from the live toolchain catalogue unless
//! an exact version override is set, verifies the row's SHA-256, and caches
//! the extracted `package/lib` and `package/include` tree per target slug.

use std::path::PathBuf;

use super::manifest_lookup::{self, ManifestIndex};
use crate::core::{SoldrError, SoldrPaths};

/// Exact Python compatibility-bundle version override. When unset, soldr
/// selects the newest version published for the requested target slug.
pub const PYTHON_VERSION_ENV_VAR: &str = "SOLDR_PYTHON_VERSION";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonSysroot {
    pub root: PathBuf,
    pub version: String,
}

impl PythonSysroot {
    pub fn lib_dir(&self) -> PathBuf {
        self.root.join("lib")
    }
}

/// The 8 canonical target triples soldr ships sysroot recipes for.
/// First element = Rust target triple (the soldr-side name). Second
/// element = catalogue slug (`recipes/python-<slug>/`).
pub const PYTHON_SYSROOT_TARGETS: &[(&str, &str)] = &[
    ("x86_64-pc-windows-msvc", "windows-x64"),
    ("aarch64-pc-windows-msvc", "windows-arm64"),
    ("x86_64-apple-darwin", "darwin-x64"),
    ("aarch64-apple-darwin", "darwin-arm64"),
    ("x86_64-unknown-linux-gnu", "linux-x64-gnu"),
    ("aarch64-unknown-linux-gnu", "linux-arm64-gnu"),
    ("x86_64-unknown-linux-musl", "linux-x64-musl"),
    ("aarch64-unknown-linux-musl", "linux-arm64-musl"),
];

/// Look up the catalogue slug for a Rust target triple.
pub fn catalogue_slug_for(triple: &str) -> Option<&'static str> {
    PYTHON_SYSROOT_TARGETS
        .iter()
        .find(|(rust_triple, _)| *rust_triple == triple)
        .map(|(_, slug)| *slug)
}

/// Construct the expected `assets`-branch URL for a Python sysroot
/// asset. The catalogue producer pipeline (forge-conan.yml → ingest)
/// publishes under this layout:
///
/// ```text
/// python/<py-version>/<slug>/bundle.tar.zst
/// ```
///
/// `media.githubusercontent.com/media/` is used (not `raw`) so LFS-
/// tracked blobs follow their pointer files to the actual bytes —
/// matching the apple-sdk fetcher's pattern.
pub fn asset_url_for(py_version: &str, slug: &str) -> String {
    super::syslib_common::asset_url_for("python", py_version, slug)
}

/// Return an explicit version override, if one was supplied.
fn requested_python_version() -> Option<String> {
    if let Ok(value) = std::env::var(PYTHON_VERSION_ENV_VAR) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn version_key(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

fn catalogue_version_from_location<'a>(location: &'a str, slug: &str) -> Option<&'a str> {
    let suffix = location
        .strip_prefix("python/")
        .or_else(|| location.split_once("/python/").map(|(_, suffix)| suffix))?;
    let mut parts = suffix.split('/');
    let version = parts.next()?;
    let row_slug = parts.next()?;
    let asset = parts.next()?;
    (row_slug == slug && asset == "bundle.tar.zst" && parts.next().is_none()).then_some(version)
}

fn newest_catalogue_version_for_slug(index: &ManifestIndex, slug: &str) -> Option<String> {
    index
        .entries
        .iter()
        .filter_map(|entry| {
            entry
                .source_path
                .as_deref()
                .or_else(|| entry.transport.direct_url())
                .and_then(|location| catalogue_version_from_location(location, slug))
        })
        .filter_map(|version| version_key(version).map(|key| (key, version)))
        .max_by_key(|(key, _)| *key)
        .map(|(_, version)| version.to_string())
}

async fn resolve_python_version_for_slug(slug: &str) -> Result<String, SoldrError> {
    if let Some(version) = requested_python_version() {
        return Ok(version);
    }
    let index = manifest_lookup::get_or_fetch().await;
    newest_catalogue_version_for_slug(index.as_ref(), slug).ok_or_else(|| {
        SoldrError::Other(format!(
            "no published Python compatibility bundle for target slug {slug}; \
             set {PYTHON_VERSION_ENV_VAR} to request an exact catalogue version"
        ))
    })
}

/// Ensure a Python sysroot is materialized for the given target triple.
///
/// Returns the extracted root and exact published version. The shared PyO3
/// plan derives its import-library directory and major.minor ABI version from
/// this result. No caller reaches this function unless compatibility mode was
/// selected explicitly.
pub async fn ensure_python_sysroot(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PythonSysroot, SoldrError> {
    let slug = catalogue_slug_for(target_triple).ok_or_else(|| {
        SoldrError::UnsupportedPlatform(format!(
            "no python sysroot recipe for target {target_triple}; \
             supported: {:?}",
            PYTHON_SYSROOT_TARGETS
                .iter()
                .map(|(t, _)| *t)
                .collect::<Vec<_>>()
        ))
    })?;
    let version = resolve_python_version_for_slug(slug).await?;
    let root = super::syslib_common::ensure_syslib_bundle(paths, "python", &version, slug).await?;
    Ok(PythonSysroot { root, version })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_covers_all_canonical_targets() {
        for (triple, _slug) in PYTHON_SYSROOT_TARGETS {
            assert!(
                crate::core::canonical_targets::is_canonical(triple),
                "{triple} not in canonical target list"
            );
        }
        for canonical in crate::core::canonical_targets::canonical_targets() {
            if *canonical == "x86_64-pc-windows-gnu" {
                assert!(catalogue_slug_for(canonical).is_none());
                continue;
            }
            assert!(
                catalogue_slug_for(canonical).is_some(),
                "canonical target {canonical} has no python sysroot row"
            );
        }
    }

    #[test]
    fn asset_url_layout_matches_catalogue() {
        let u = asset_url_for("3.13.0", "windows-x64");
        assert!(u.starts_with("https://media.githubusercontent.com/media/"));
        assert!(u.contains("/zackees/soldr-toolchain/assets/"));
        assert!(u.contains("/python/3.13.0/windows-x64/"));
        assert!(u.ends_with("/bundle.tar.zst"));
    }

    #[test]
    fn catalogue_slug_for_known_triples() {
        assert_eq!(
            catalogue_slug_for("x86_64-pc-windows-msvc"),
            Some("windows-x64")
        );
        assert_eq!(
            catalogue_slug_for("aarch64-pc-windows-msvc"),
            Some("windows-arm64")
        );
        assert_eq!(
            catalogue_slug_for("x86_64-apple-darwin"),
            Some("darwin-x64")
        );
        assert_eq!(
            catalogue_slug_for("aarch64-apple-darwin"),
            Some("darwin-arm64")
        );
        assert_eq!(
            catalogue_slug_for("x86_64-unknown-linux-musl"),
            Some("linux-x64-musl")
        );
        assert_eq!(catalogue_slug_for("wasm32-unknown-unknown"), None);
    }

    #[test]
    fn requested_python_version_accepts_exact_override_serial() {
        let prev = std::env::var_os(PYTHON_VERSION_ENV_VAR);

        std::env::remove_var(PYTHON_VERSION_ENV_VAR);
        assert_eq!(requested_python_version(), None);

        std::env::set_var(PYTHON_VERSION_ENV_VAR, "3.13.14");
        assert_eq!(requested_python_version().as_deref(), Some("3.13.14"));

        match prev {
            Some(v) => std::env::set_var(PYTHON_VERSION_ENV_VAR, v),
            None => std::env::remove_var(PYTHON_VERSION_ENV_VAR),
        }
    }

    #[test]
    fn lib_dir_matches_published_bundle_layout() {
        let sysroot = PythonSysroot {
            root: PathBuf::from("sdk").join("package"),
            version: "3.13.14".into(),
        };
        assert_eq!(sysroot.lib_dir(), PathBuf::from("sdk/package/lib"));
    }

    #[test]
    fn newest_version_is_selected_from_target_catalogue_rows() {
        let index = super::super::manifest_lookup::ManifestIndex::from_json(
            r#"{"entries":[
                {"owner":"zackees","repo":"soldr-toolchain","tag":"3.12.7","asset":"bundle.tar.zst","url":"https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/python/3.12.7/windows-x64/bundle.tar.zst","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                {"owner":"zackees","repo":"soldr-toolchain","tag":"3.13.14","asset":"bundle.tar.zst","url":"https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/python/3.13.14/windows-x64/bundle.tar.zst","sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},
                {"owner":"zackees","repo":"soldr-toolchain","tag":"9.9.9","asset":"bundle.tar.zst","url":"https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/python/9.9.9/darwin-arm64/bundle.tar.zst","sha256":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            newest_catalogue_version_for_slug(&index, "windows-x64").as_deref(),
            Some("3.13.14")
        );
        assert_eq!(
            catalogue_version_from_location(
                "python/3.14.1/windows-x64/bundle.tar.zst",
                "windows-x64"
            ),
            Some("3.14.1")
        );
    }
}
