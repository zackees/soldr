//! Workspace shapes `cargo chef` cannot cook (soldr#2788).
//!
//! cargo-chef builds a *recipe*: a skeleton of the workspace with stub
//! sources, cooks third-party dependencies against it, then throws the stubs
//! away. That is sound for a registry dependency. It is not sound for a path
//! dependency that resolves outside the workspace — into `[workspace] exclude`
//! or past the workspace root — because such a dependency is usually its own
//! workspace, and the skeleton never materialises *its* members. The cook then
//! fails partway through with
//!
//! ```text
//! error: extern location for zccache_cli_core does not exist: ...
//! ```
//!
//! soldr's own tree is exactly this shape: `_vender/zccache` and
//! `_vender/running-process` are excluded from the workspace and depended on
//! by path.
//!
//! The cost of finding this out by running is what makes it worth detecting:
//! ~190 s spent, no cache layer saved, and the build still green — so the
//! feature is permanently off and nothing says so. Detecting up front turns a
//! silent permanent regression into one line.

use std::path::{Path, PathBuf};

/// A path dependency that resolves outside the cookable workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalPathDependency {
    /// The dependency name as written in the manifest.
    pub(crate) name: String,
    /// The manifest that declares it, relative to the workspace root.
    pub(crate) declared_in: PathBuf,
    /// Why it is not cookable.
    pub(crate) reason: ExternalReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalReason {
    /// Resolves inside the root but under a `[workspace] exclude` entry.
    Excluded,
    /// Resolves outside the workspace root entirely.
    OutsideRoot,
}

impl ExternalReason {
    fn describe(self) -> &'static str {
        match self {
            Self::Excluded => "excluded from the workspace",
            Self::OutsideRoot => "outside the workspace root",
        }
    }
}

/// Dependency tables that contribute to a cooked build.
///
/// `dev-dependencies` are included: cargo-chef cooks them for `--tests` and
/// the same skeleton problem applies.
const DEPENDENCY_TABLES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

/// Path dependencies in this workspace that `cargo chef` cannot cook.
///
/// Best-effort by construction: this runs before an optimisation, so an
/// unreadable or malformed manifest yields "nothing found" and the cook
/// proceeds exactly as it does today. Failing the build over a preflight
/// would be worse than the problem it detects.
pub(crate) fn external_path_dependencies(root: &Path) -> Vec<ExternalPathDependency> {
    let Some(root_doc) = read_manifest(&root.join("Cargo.toml")) else {
        return Vec::new();
    };
    let workspace = root_doc.get("workspace").and_then(|w| w.as_table());
    let excluded: Vec<PathBuf> = workspace
        .and_then(|w| w.get("exclude"))
        .and_then(|e| e.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e.as_str())
                .map(|e| root.join(e))
                .collect()
        })
        .unwrap_or_default();

    let mut members: Vec<PathBuf> = vec![root.to_path_buf()];
    if let Some(list) = workspace
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    {
        for entry in list.iter().filter_map(|m| m.as_str()) {
            members.extend(expand_member(root, entry));
        }
    }

    let mut found = Vec::new();
    for member in members {
        let manifest = member.join("Cargo.toml");
        let Some(doc) = read_manifest(&manifest) else {
            continue;
        };
        for (name, dep_path) in path_dependencies(&doc) {
            let resolved = normalize(&member.join(dep_path));
            let reason = if excluded
                .iter()
                .any(|ex| resolved.starts_with(normalize(ex)))
            {
                ExternalReason::Excluded
            } else if resolved.starts_with(normalize(root)) {
                continue;
            } else {
                ExternalReason::OutsideRoot
            };
            found.push(ExternalPathDependency {
                name,
                declared_in: manifest
                    .strip_prefix(root)
                    .unwrap_or(&manifest)
                    .to_path_buf(),
                reason,
            });
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found.dedup_by(|a, b| a.name == b.name);
    found
}

/// The line soldr prints instead of spending three minutes failing.
pub(crate) fn skip_message(found: &[ExternalPathDependency]) -> String {
    let mut out = String::from(
        "soldr cook: skipped -- this workspace has path dependencies that \
         cargo-chef cannot cook (soldr#2788).\n",
    );
    for dep in found {
        out.push_str(&format!(
            "  {} ({}), declared in {}\n",
            dep.name,
            dep.reason.describe(),
            dep.declared_in.display()
        ));
    }
    out.push_str(
        "cargo-chef's recipe stubs the workspace out and cooks dependencies \
         against the skeleton. A path dependency that resolves outside the \
         workspace is usually its own workspace, whose members the skeleton \
         never materialises, so the cook fails partway with `extern location \
         ... does not exist`.\n\
         Cooking is skipped rather than attempted: it would spend minutes and \
         produce nothing. The build continues uncooked, exactly as it does \
         when the cook fails today.\n",
    );
    out
}

fn expand_member(root: &Path, entry: &str) -> Vec<PathBuf> {
    if !entry.contains('*') {
        return vec![root.join(entry)];
    }
    // Only the common `dir/*` shape; anything richer falls back to nothing,
    // which costs a missed detection rather than a wrong one.
    let Some(prefix) = entry.strip_suffix("/*") else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(root.join(prefix)) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("Cargo.toml").is_file())
        .collect()
}

fn read_manifest(path: &Path) -> Option<toml::Table> {
    std::fs::read_to_string(path)
        .ok()?
        .parse::<toml::Table>()
        .ok()
}

fn path_dependencies(doc: &toml::Table) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for table_name in DEPENDENCY_TABLES {
        collect_from(doc.get(*table_name), &mut out);
    }
    if let Some(targets) = doc.get("target").and_then(|t| t.as_table()) {
        for cfg in targets.values() {
            let Some(cfg) = cfg.as_table() else { continue };
            for table_name in DEPENDENCY_TABLES {
                collect_from(cfg.get(*table_name), &mut out);
            }
        }
    }
    out
}

fn collect_from(table: Option<&toml::Value>, out: &mut Vec<(String, String)>) {
    let Some(table) = table.and_then(|t| t.as_table()) else {
        return;
    };
    for (name, spec) in table {
        if let Some(path) = spec
            .as_table()
            .and_then(|s| s.get("path"))
            .and_then(|p| p.as_str())
        {
            out.push((name.clone(), path.to_string()));
        }
    }
}

/// Lexical normalisation. `canonicalize` is deliberately avoided: it requires
/// the path to exist, and a manifest can legitimately name a dependency whose
/// checkout is missing (an uninitialised submodule) -- which is a case this
/// should still report rather than skip over.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).expect("create dir");
        std::fs::write(path, body).expect("write manifest");
    }

    fn workspace(root: &Path) {
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\"]\nexclude = [\"_vender/dep\"]\n",
        );
    }

    #[test]
    fn an_excluded_path_dependency_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        workspace(root);
        write(
            &root.join("crates/app/Cargo.toml"),
            "[package]\nname='app'\n\n[dependencies]\n\
             vendored = { path = \"../../_vender/dep\" }\n",
        );

        let found = external_path_dependencies(root);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, "vendored");
        assert_eq!(found[0].reason, ExternalReason::Excluded);
    }

    #[test]
    fn a_dependency_outside_the_root_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("ws");
        workspace(&root);
        write(
            &root.join("crates/app/Cargo.toml"),
            "[package]\nname='app'\n\n[dependencies]\n\
             sibling = { path = \"../../../elsewhere\" }\n",
        );

        let found = external_path_dependencies(&root);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].reason, ExternalReason::OutsideRoot);
    }

    // The common case must stay silent, or the preflight turns every cook
    // into a skip and removes the feature it is protecting.
    #[test]
    fn an_ordinary_intra_workspace_path_dependency_is_fine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        workspace(root);
        write(
            &root.join("crates/app/Cargo.toml"),
            "[package]\nname='app'\n\n[dependencies]\n\
             core = { path = \"../core\" }\nserde = \"1\"\n",
        );
        write(
            &root.join("crates/core/Cargo.toml"),
            "[package]\nname='core'\n",
        );

        assert_eq!(external_path_dependencies(root), Vec::new());
    }

    #[test]
    fn registry_dependencies_are_never_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        workspace(root);
        write(
            &root.join("crates/app/Cargo.toml"),
            "[package]\nname='app'\n\n[dependencies]\nserde = \"1\"\ntokio = { version = \"1\" }\n",
        );

        assert_eq!(external_path_dependencies(root), Vec::new());
    }

    #[test]
    fn target_and_dev_tables_are_searched_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        workspace(root);
        write(
            &root.join("crates/app/Cargo.toml"),
            "[package]\nname='app'\n\n[dev-dependencies]\n\
             harness = { path = \"../../_vender/dep\" }\n\n\
             [target.'cfg(windows)'.dependencies]\n\
             winhelp = { path = \"../../_vender/dep\" }\n",
        );

        let found = external_path_dependencies(root);
        assert_eq!(found.len(), 2, "{found:?}");
    }

    // Runs before an optimisation: a broken manifest must mean "nothing
    // found", never a failure.
    #[test]
    fn an_unreadable_workspace_reports_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(external_path_dependencies(dir.path()), Vec::new());

        write(&dir.path().join("Cargo.toml"), "this is not toml {{{");
        assert_eq!(external_path_dependencies(dir.path()), Vec::new());
    }

    #[test]
    fn the_message_names_every_offender_and_the_mechanism() {
        let found = vec![ExternalPathDependency {
            name: "zccache".into(),
            declared_in: PathBuf::from("crates/soldr-cache/Cargo.toml"),
            reason: ExternalReason::Excluded,
        }];
        let msg = skip_message(&found);

        assert!(msg.contains("soldr cook: skipped"), "{msg}");
        assert!(msg.contains("zccache"), "{msg}");
        assert!(msg.contains("crates/soldr-cache/Cargo.toml"), "{msg}");
        assert!(msg.contains("excluded from the workspace"), "{msg}");
        // The reader needs to know the build is not broken by this.
        assert!(msg.contains("build continues uncooked"), "{msg}");
    }

    // The case soldr#2788 was filed from. This repository vendors `zccache`
    // and `running-process` under `_vender/`, both `[workspace] exclude`d and
    // both depended on by path -- so the detector must fire here, or the fix
    // does nothing for the workspace that motivated it.
    //
    // Resolved at RUNTIME by walking up from the working directory, never from
    // `CARGO_MANIFEST_DIR`: these tests also run from a nextest archive on a
    // machine that has no source tree, and `test_archived_source_tests_use_
    // only_runtime_workspace_resolution` enforces that. When the checkout is
    // not present the test has nothing to assert and says so by skipping.
    #[test]
    fn soldrs_own_workspace_is_detected() {
        let Some(root) = soldr_checkout_root() else {
            eprintln!("skipping: no soldr checkout above the working directory");
            return;
        };

        let found = external_path_dependencies(&root);
        let names: Vec<&str> = found.iter().map(|d| d.name.as_str()).collect();
        assert!(
            names.contains(&"zccache"),
            "expected the vendored zccache path dep in {}; got {names:?}",
            root.display()
        );
        assert!(
            found.iter().all(|d| d.reason == ExternalReason::Excluded),
            "these are excluded, not outside the root: {found:?}"
        );
    }

    /// The nearest ancestor that is soldr's own workspace, if any.
    ///
    /// Identified by content rather than location: a root manifest that
    /// excludes `_vender/zccache`. That is the property under test, so a
    /// checkout laid out differently still matches and an unrelated workspace
    /// never does.
    fn soldr_checkout_root() -> Option<PathBuf> {
        let mut dir = std::env::current_dir().ok()?;
        loop {
            let manifest = dir.join("Cargo.toml");
            if let Ok(text) = std::fs::read_to_string(&manifest) {
                if text.contains("_vender/zccache") && text.contains("[workspace]") {
                    return Some(dir);
                }
            }
            if !dir.pop() {
                return None;
            }
        }
    }
}
