//! Which crate in the resolved graph claims a given `links` name.
//!
//! Cargo's `links = "foo"` key is a **collision-avoidance declaration**:
//! at most one package in a dependency graph may claim a native library
//! name. It is emphatically *not* an assertion that the library behind
//! that name is interchangeable with any other library called `foo`.
//!
//! `blessed_build` used to treat it as the latter. It keyed its syslib
//! catalogue on the `links` name alone and injected a prebuilt upstream
//! library via Cargo's build-script override for any graph that claimed
//! it. For a **fork** — a crate that vendors a patched copy of the same C
//! library and exports a superset of its API — that substitution silently
//! swaps the fork's binary for upstream's and the link fails on every
//! symbol the fork added (soldr#2142, `mimalloc-pprof`).
//!
//! So the override is only sound when the package providing the `links`
//! name is the specific crate soldr's prebuilt was cut to match. That is
//! answerable from `cargo metadata`, which reports `links` per package.
//!
//! # Failure policy
//!
//! Note this is the *opposite* of [`crate::pyo3_detect`]'s stance, which
//! treats metadata as advisory and degrades its plan when the probe
//! fails. Here a failed probe means soldr does not know who provides the
//! name, and injecting on a guess is precisely the bug. So an
//! unresolvable graph yields [`LinksProvider::Unknown`] and the caller
//! skips the substitution: that costs the prebuilt (the crate's own
//! vendored compile still works) but can never mis-link.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;

/// Who provides a `links` name in the resolved dependency graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinksProvider {
    /// Exactly one package claims the name.
    Package(String),
    /// No package in the graph claims it.
    Absent,
    /// The graph could not be resolved, or more than one package name
    /// claims the same `links`. Callers must treat this as "do not
    /// substitute".
    Unknown(String),
}

impl LinksProvider {
    /// True when the graph proves `expected` is the sole provider.
    pub(crate) fn is(&self, expected: &str) -> bool {
        matches!(self, LinksProvider::Package(name) if name == expected)
    }
}

/// Resolve the provider of `links` for the graph rooted at
/// `workspace_root`, built for `target` (empty string = no filter).
///
/// The underlying `cargo metadata` probe is memoized per
/// `(workspace_root, target)` for the life of the process, so asking
/// about several `links` names costs one subprocess, not several.
pub(crate) fn resolve(workspace_root: &Path, links: &str, target: &str) -> LinksProvider {
    match links_map(workspace_root, target) {
        Ok(map) => match map.get(links) {
            Some(names) if names.len() == 1 => {
                LinksProvider::Package(names.iter().next().cloned().unwrap_or_default())
            }
            Some(names) => LinksProvider::Unknown(format!(
                "{} packages claim links = \"{links}\": {}",
                names.len(),
                names.iter().cloned().collect::<Vec<_>>().join(", ")
            )),
            None => LinksProvider::Absent,
        },
        Err(error) => LinksProvider::Unknown(error),
    }
}

/// `links` name -> set of package names claiming it.
type LinksMap = HashMap<String, BTreeSet<String>>;

#[allow(clippy::type_complexity)]
static LINKS_CACHE: OnceLock<Mutex<HashMap<(PathBuf, String), Result<LinksMap, String>>>> =
    OnceLock::new();

fn links_map(workspace_root: &Path, target: &str) -> Result<LinksMap, String> {
    let key = (workspace_root.to_path_buf(), target.to_string());
    let cache = LINKS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    // A poisoned lock means some other caller panicked mid-probe. That
    // is not a reason to mis-link, so fall through to a fresh probe
    // rather than unwrapping into a panic of our own.
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.get(&key) {
            return cached.clone();
        }
    }
    let result = probe_links_map(workspace_root, target);
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, result.clone());
    }
    result
}

fn probe_links_map(workspace_root: &Path, target: &str) -> Result<LinksMap, String> {
    // Answer without a subprocess when there is plainly nothing to
    // resolve. `blessed_build::prepare` runs on paths that do not all
    // start from a workspace, and spawning cargo only to have it fail
    // is both slower and a less predictable failure than saying so.
    if !manifest_exists(workspace_root) {
        return Err(format!(
            "no Cargo.toml at {}",
            workspace_root.to_string_lossy()
        ));
    }
    let cargo =
        crate::binaries::resolve_toolchain_binary("cargo").map_err(|error| error.to_string())?;
    let mut command = Command::new(cargo);
    command.args(["metadata", "--format-version", "1"]);
    command.current_dir(workspace_root);
    // Same rationale as the PyO3 probe: `cargo metadata` reaches rustc
    // through a rustup proxy, and in managed CI the pinned toolchain is
    // not the user's default, so carry the explicit channel across or
    // rustup reports that no default toolchain is configured.
    if let Some(toolchain) = std::env::var_os("RUSTUP_TOOLCHAIN") {
        if !toolchain.is_empty() {
            command.env("RUSTUP_TOOLCHAIN", toolchain);
        }
    } else if let Ok(manifest) = crate::core::read_rust_toolchain_manifest(workspace_root) {
        if let Some(channel) = manifest.channel {
            let channel = channel.trim();
            if !channel.is_empty() {
                command.env("RUSTUP_TOOLCHAIN", channel);
            }
        }
    }
    // Never re-enter soldr from a probe.
    command.env_remove("RUSTC_WRAPPER");
    command.env_remove("RUSTC_WORKSPACE_WRAPPER");
    if !target.is_empty() {
        command.args(["--filter-platform", target]);
    }
    // Feature selection is deliberately NOT forwarded. Without it the
    // graph is a superset of what will actually build, so an optional
    // fork dependency still shows up and we conservatively skip the
    // substitution. The reverse error — resolving a narrower graph,
    // missing the fork, and injecting upstream anyway — is the one that
    // breaks the link.
    let output = crate::core::command_output_with_timeout(&mut command, "cargo metadata links")
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    links_map_from_metadata_json(&output.stdout)
}

fn manifest_exists(workspace_root: &Path) -> bool {
    workspace_root.join("Cargo.toml").is_file()
}

#[derive(Deserialize)]
struct CargoMetadata {
    #[serde(default)]
    packages: Vec<CargoPackage>,
}

#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    #[serde(default)]
    links: Option<String>,
}

fn links_map_from_metadata_json(bytes: &[u8]) -> Result<LinksMap, String> {
    let metadata: CargoMetadata =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let mut map: LinksMap = HashMap::new();
    for package in metadata.packages {
        let Some(links) = package.links else { continue };
        if links.is_empty() {
            continue;
        }
        map.entry(links).or_default().insert(package.name);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(packages: &str) -> Vec<u8> {
        format!("{{\"packages\":[{packages}]}}").into_bytes()
    }

    #[test]
    fn sole_provider_is_reported_by_name() {
        let map = links_map_from_metadata_json(&json(
            r#"{"name":"libmimalloc-sys","links":"mimalloc"},
               {"name":"serde","links":null}"#,
        ))
        .expect("parse");
        assert_eq!(
            map.get("mimalloc").map(|names| names.len()),
            Some(1),
            "exactly one package claims the name"
        );
        assert!(map.get("mimalloc").unwrap().contains("libmimalloc-sys"));
        assert!(!map.contains_key("serde"), "a null links is not an entry");
    }

    #[test]
    fn a_fork_claiming_the_same_links_is_distinguishable() {
        let map =
            links_map_from_metadata_json(&json(r#"{"name":"mimalloc-pprof","links":"mimalloc"}"#))
                .expect("parse");
        let names = map.get("mimalloc").expect("entry");
        assert_eq!(names.len(), 1);
        assert!(
            !names.contains("libmimalloc-sys"),
            "the fork must not be mistaken for upstream"
        );
    }

    #[test]
    fn missing_and_empty_links_are_ignored() {
        let map = links_map_from_metadata_json(&json(r#"{"name":"a"},{"name":"b","links":""}"#))
            .expect("parse");
        assert!(map.is_empty());
    }

    #[test]
    fn provider_predicate_matches_only_the_named_crate() {
        assert!(LinksProvider::Package("libmimalloc-sys".to_string()).is("libmimalloc-sys"));
        assert!(!LinksProvider::Package("mimalloc-pprof".to_string()).is("libmimalloc-sys"));
        assert!(!LinksProvider::Absent.is("libmimalloc-sys"));
        assert!(!LinksProvider::Unknown("probe failed".to_string()).is("libmimalloc-sys"));
    }

    #[test]
    fn a_manifestless_dir_resolves_without_spawning_cargo() {
        // The point is that this returns promptly and does not depend
        // on a cargo binary being resolvable at all.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let provider = resolve(tmp.path(), "mimalloc", "x86_64-pc-windows-msvc");
        match provider {
            LinksProvider::Unknown(reason) => assert!(
                reason.contains("no Cargo.toml"),
                "expected the manifest short-circuit, got: {reason}"
            ),
            other => panic!("expected Unknown, got {other:?}"),
        }
        assert!(
            !LinksProvider::Unknown(String::new()).is("libmimalloc-sys"),
            "and Unknown must never authorize a substitution"
        );
    }

    #[test]
    fn malformed_metadata_is_an_error_not_a_panic() {
        assert!(links_map_from_metadata_json(b"not json").is_err());
    }
}
