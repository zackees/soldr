//! Parsing for `rust-toolchain.toml` (rustup schema + soldr extensions).
//!
//! Exposes `RustToolchainManifest`, `SoldrManifestSection`, and `PluginSpec`
//! plus the `read_rust_toolchain_manifest` entry point. The `[soldr]`
//! section is soldr-specific and used by `soldr toolchain prepare` /
//! `ensure` to drive `cargo install` invocations.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use super::SoldrError;

#[derive(Debug, Deserialize)]
struct RustToolchainFile {
    toolchain: Option<RustToolchainSection>,
    #[serde(default)]
    soldr: Option<SoldrManifestSection>,
}

#[derive(Debug, Deserialize)]
struct RustToolchainSection {
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    components: Option<Vec<String>>,
    #[serde(default)]
    targets: Option<Vec<String>>,
    #[serde(default)]
    profile: Option<String>,
}

/// Top-level `[soldr]` section of `rust-toolchain.toml`. Carries
/// soldr-specific developer-tooling declarations that aren't part of
/// rustup's own schema. Surfaces:
/// * `[soldr.plugins]` — translated into `cargo install` invocations
///   by `soldr toolchain prepare`.
/// * `[soldr.cook]` — project-scoped overrides for `~/.soldr/config.toml`
///   `[cook]` (issue #578). Today only `auto_hydrate` is honored; the
///   rest of the cook config (size cap, age bound, ...) lives in the
///   user-global config.
#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq)]
pub struct SoldrManifestSection {
    #[serde(default)]
    pub plugins: BTreeMap<String, PluginSpec>,
    #[serde(default)]
    pub cook: Option<SoldrCookManifest>,
}

/// `[soldr.cook]` section of `rust-toolchain.toml` (issue #578).
///
/// ```toml
/// [soldr.cook]
/// auto_hydrate = false   # opt out for this repo
/// ```
#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq)]
pub struct SoldrCookManifest {
    /// Project-scoped override for `~/.soldr/config.toml`
    /// `[cook] auto_hydrate`. `None` means "fall through to the
    /// global config". The env var `SOLDR_COOK_AUTO_HYDRATE` overrides
    /// both.
    #[serde(default)]
    pub auto_hydrate: Option<bool>,
}

/// One entry in `[soldr.plugins]`. The key is the cargo crate name
/// (e.g. `cargo-nextest`); the value is either a bare version string or
/// a detailed table that mirrors `cargo install`'s relevant flags.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum PluginSpec {
    /// `cargo-nextest = "0.9"` — just a version requirement. The literal
    /// `"*"` is treated as "any version" and skips `--version`.
    Version(String),
    /// `cargo-zigbuild = { version = "0.18", locked = true, ... }`.
    /// Every field is optional; omitted fields mean "don't pass the
    /// corresponding cargo install flag".
    Detailed {
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        locked: Option<bool>,
        #[serde(default)]
        features: Option<Vec<String>>,
        #[serde(default)]
        no_default_features: Option<bool>,
    },
}

/// Parsed view of a project's `rust-toolchain.toml`. All fields are
/// optional so callers can treat a missing file or missing `[toolchain]`
/// section the same as a fully-populated section whose fields happen to
/// be unset. Returned by [`read_rust_toolchain_manifest`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RustToolchainManifest {
    pub channel: Option<String>,
    pub components: Option<Vec<String>>,
    pub targets: Option<Vec<String>>,
    pub profile: Option<String>,
    /// Parsed `[soldr]` section. `None` when the file omits it
    /// entirely so callers can short-circuit cleanly.
    pub soldr: Option<SoldrManifestSection>,
}

/// Read `rust-toolchain.toml` from `workspace_root` (non-recursive — the
/// caller is expected to already point at the directory containing the
/// manifest, mirroring how cargo resolves the file). A missing file is
/// not an error; an empty `RustToolchainManifest` is returned so callers
/// can branch on `manifest.channel.is_none()` without juggling IO error
/// kinds. Malformed TOML or unreadable files surface as
/// [`SoldrError::Other`].
pub fn read_rust_toolchain_manifest(
    workspace_root: &Path,
) -> Result<RustToolchainManifest, SoldrError> {
    let path = workspace_root.join("rust-toolchain.toml");
    if !path.exists() {
        return Ok(RustToolchainManifest::default());
    }
    let text = std::fs::read_to_string(&path).map_err(|err| {
        SoldrError::Other(format!(
            "failed to read rust-toolchain.toml at {}: {err}",
            path.display()
        ))
    })?;
    let parsed: RustToolchainFile = toml::from_str(&text).map_err(|err| {
        SoldrError::Other(format!(
            "failed to parse rust-toolchain.toml at {}: {err}",
            path.display()
        ))
    })?;
    let soldr = parsed.soldr;
    let Some(section) = parsed.toolchain else {
        return Ok(RustToolchainManifest {
            soldr,
            ..RustToolchainManifest::default()
        });
    };
    Ok(RustToolchainManifest {
        channel: section
            .channel
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        components: section.components,
        targets: section.targets,
        profile: section
            .profile
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        soldr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn rust_toolchain_manifest_parses_full_section() {
        let dir = tempdir().unwrap();
        let manifest_path = dir.path().join("rust-toolchain.toml");
        fs::write(
            &manifest_path,
            "[toolchain]\n\
             channel = \"1.94.1\"\n\
             components = [\"clippy\", \"rustfmt\"]\n\
             targets = [\"x86_64-unknown-linux-musl\", \"aarch64-apple-darwin\"]\n\
             profile = \"minimal\"\n",
        )
        .unwrap();

        let manifest = read_rust_toolchain_manifest(dir.path()).unwrap();
        assert_eq!(manifest.channel.as_deref(), Some("1.94.1"));
        assert_eq!(
            manifest.components.as_deref(),
            Some(&["clippy".to_string(), "rustfmt".to_string()][..])
        );
        assert_eq!(
            manifest.targets.as_deref(),
            Some(
                &[
                    "x86_64-unknown-linux-musl".to_string(),
                    "aarch64-apple-darwin".to_string()
                ][..]
            )
        );
        assert_eq!(manifest.profile.as_deref(), Some("minimal"));
    }

    #[test]
    fn rust_toolchain_manifest_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let manifest = read_rust_toolchain_manifest(dir.path()).unwrap();
        assert_eq!(manifest, RustToolchainManifest::default());
        assert!(manifest.channel.is_none());
        assert!(manifest.components.is_none());
        assert!(manifest.targets.is_none());
        assert!(manifest.profile.is_none());
        assert!(manifest.soldr.is_none());
    }

    #[test]
    fn manifest_parses_soldr_plugins_section() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\n\
             channel = \"1.94.1\"\n\
             \n\
             [soldr.plugins]\n\
             cargo-nextest = \"0.9\"\n\
             cargo-zigbuild = { version = \"0.18\", locked = true }\n\
             cargo-deny = \"*\"\n",
        )
        .unwrap();

        let manifest = read_rust_toolchain_manifest(dir.path()).unwrap();
        let soldr = manifest.soldr.expect("expected [soldr] section to parse");
        assert_eq!(soldr.plugins.len(), 3);
        match soldr
            .plugins
            .get("cargo-nextest")
            .expect("cargo-nextest missing")
        {
            PluginSpec::Version(value) => assert_eq!(value, "0.9"),
            other => panic!("cargo-nextest should parse as Version(\"0.9\"), got {other:?}"),
        }
        match soldr
            .plugins
            .get("cargo-zigbuild")
            .expect("cargo-zigbuild missing")
        {
            PluginSpec::Detailed {
                version,
                locked,
                features,
                no_default_features,
            } => {
                assert_eq!(version.as_deref(), Some("0.18"));
                assert_eq!(*locked, Some(true));
                assert!(features.is_none());
                assert!(no_default_features.is_none());
            }
            other => panic!("cargo-zigbuild should parse as Detailed, got {other:?}"),
        }
        match soldr.plugins.get("cargo-deny").expect("cargo-deny missing") {
            PluginSpec::Version(value) => assert_eq!(value, "*"),
            other => panic!("cargo-deny should parse as Version(\"*\"), got {other:?}"),
        }
    }
}
