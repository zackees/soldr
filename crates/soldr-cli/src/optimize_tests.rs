//! Unit tests for the `soldr optimize` subcommand. See `optimize.rs`,
//! `optimize_detect.rs`, and `optimize_windows.rs` for the implementation.

use super::{
    filter_undo_entries, plan_global_paths, plan_project_paths, resolve_project_target_dir,
    ManagedExclusion, ManagedExclusionFile, OptimizeOutput, OptimizeScope,
};
use crate::optimize_detect::{detect_ci, parse_windows_build, Platform};
use std::path::PathBuf;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(value) = &self.previous {
            std::env::set_var(self.key, value);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// -- CI detection ----------------------------------------------------------

#[test]
fn ci_detection_returns_none_when_no_env_set() {
    let _lock = lock_env();
    let _a = EnvVarGuard::remove("GITHUB_ACTIONS");
    let _b = EnvVarGuard::remove("CI");
    let _c = EnvVarGuard::remove("BUILDKITE");
    let _d = EnvVarGuard::remove("CIRCLECI");
    let _e = EnvVarGuard::remove("TRAVIS");
    let _f = EnvVarGuard::remove("JENKINS_URL");
    assert_eq!(detect_ci(), None);
}

#[test]
fn ci_detection_prefers_github_actions() {
    let _lock = lock_env();
    let _a = EnvVarGuard::set("GITHUB_ACTIONS", "true");
    let _b = EnvVarGuard::set("CI", "true");
    let _c = EnvVarGuard::remove("BUILDKITE");
    let _d = EnvVarGuard::remove("CIRCLECI");
    let _e = EnvVarGuard::remove("TRAVIS");
    let _f = EnvVarGuard::remove("JENKINS_URL");
    assert_eq!(detect_ci(), Some("github_actions"));
}

#[test]
fn ci_detection_picks_up_each_label() {
    let _lock = lock_env();
    // Generic CI
    let _a = EnvVarGuard::remove("GITHUB_ACTIONS");
    let _b = EnvVarGuard::set("CI", "true");
    let _c = EnvVarGuard::remove("BUILDKITE");
    let _d = EnvVarGuard::remove("CIRCLECI");
    let _e = EnvVarGuard::remove("TRAVIS");
    let _f = EnvVarGuard::remove("JENKINS_URL");
    assert_eq!(detect_ci(), Some("ci"));
}

#[test]
fn ci_detection_picks_up_buildkite() {
    let _lock = lock_env();
    let _a = EnvVarGuard::remove("GITHUB_ACTIONS");
    let _b = EnvVarGuard::remove("CI");
    let _c = EnvVarGuard::set("BUILDKITE", "true");
    let _d = EnvVarGuard::remove("CIRCLECI");
    let _e = EnvVarGuard::remove("TRAVIS");
    let _f = EnvVarGuard::remove("JENKINS_URL");
    assert_eq!(detect_ci(), Some("buildkite"));
}

#[test]
fn ci_detection_picks_up_circleci() {
    let _lock = lock_env();
    let _a = EnvVarGuard::remove("GITHUB_ACTIONS");
    let _b = EnvVarGuard::remove("CI");
    let _c = EnvVarGuard::remove("BUILDKITE");
    let _d = EnvVarGuard::set("CIRCLECI", "true");
    let _e = EnvVarGuard::remove("TRAVIS");
    let _f = EnvVarGuard::remove("JENKINS_URL");
    assert_eq!(detect_ci(), Some("circleci"));
}

#[test]
fn ci_detection_picks_up_travis() {
    let _lock = lock_env();
    let _a = EnvVarGuard::remove("GITHUB_ACTIONS");
    let _b = EnvVarGuard::remove("CI");
    let _c = EnvVarGuard::remove("BUILDKITE");
    let _d = EnvVarGuard::remove("CIRCLECI");
    let _e = EnvVarGuard::set("TRAVIS", "true");
    let _f = EnvVarGuard::remove("JENKINS_URL");
    assert_eq!(detect_ci(), Some("travis"));
}

#[test]
fn ci_detection_picks_up_jenkins_via_non_empty_url() {
    let _lock = lock_env();
    let _a = EnvVarGuard::remove("GITHUB_ACTIONS");
    let _b = EnvVarGuard::remove("CI");
    let _c = EnvVarGuard::remove("BUILDKITE");
    let _d = EnvVarGuard::remove("CIRCLECI");
    let _e = EnvVarGuard::remove("TRAVIS");
    let _f = EnvVarGuard::set("JENKINS_URL", "http://example.com/");
    assert_eq!(detect_ci(), Some("jenkins"));
}

#[test]
fn ci_detection_ignores_falsy_values() {
    let _lock = lock_env();
    let _a = EnvVarGuard::set("GITHUB_ACTIONS", "false");
    let _b = EnvVarGuard::set("CI", "0");
    let _c = EnvVarGuard::set("BUILDKITE", "");
    let _d = EnvVarGuard::set("CIRCLECI", "no");
    let _e = EnvVarGuard::set("TRAVIS", "off");
    let _f = EnvVarGuard::remove("JENKINS_URL");
    assert_eq!(detect_ci(), None);
}

// -- Platform detection (boundary build numbers) ---------------------------

#[test]
fn platform_boundary_windows_10_22h2() {
    assert_eq!(parse_windows_build(10, 0, 19045), Platform::Windows10);
}

#[test]
fn platform_boundary_windows_11_initial_build() {
    assert_eq!(
        parse_windows_build(10, 0, 22000),
        Platform::Windows11Pre22H2
    );
}

#[test]
fn platform_boundary_windows_11_22h2() {
    assert_eq!(
        parse_windows_build(10, 0, 22621),
        Platform::Windows11Post22H2
    );
}

#[test]
fn platform_windows_10_early_build_is_still_windows_10() {
    assert_eq!(parse_windows_build(10, 0, 19041), Platform::Windows10);
}

#[test]
fn platform_above_22h2_threshold_remains_post22h2() {
    assert_eq!(
        parse_windows_build(10, 0, 25000),
        Platform::Windows11Post22H2
    );
}

// -- Scope resolution ------------------------------------------------------

#[test]
fn project_scope_errors_when_no_cargo_toml() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = resolve_project_target_dir(dir.path(), None);
    assert!(
        result.is_err(),
        "expected error when no Cargo.toml in ancestor tree"
    );
}

#[test]
fn project_scope_resolves_workspace_root_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("Cargo.toml");
    std::fs::write(&manifest, "[package]\nname=\"x\"\nversion=\"0.0.1\"\n").unwrap();

    let target = resolve_project_target_dir(dir.path(), None).expect("resolution");
    assert_eq!(target, dir.path().join("target"));
}

#[test]
fn project_scope_walks_up_to_find_cargo_toml() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("Cargo.toml");
    std::fs::write(&manifest, "[package]\nname=\"x\"\nversion=\"0.0.1\"\n").unwrap();
    let nested = dir.path().join("a").join("b").join("c");
    std::fs::create_dir_all(&nested).unwrap();

    let target = resolve_project_target_dir(&nested, None).expect("resolution");
    assert_eq!(target, dir.path().join("target"));
}

#[test]
fn project_scope_uses_explicit_manifest_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest_root = dir.path().join("sub");
    std::fs::create_dir_all(&manifest_root).unwrap();
    let manifest = manifest_root.join("Cargo.toml");
    std::fs::write(&manifest, "[package]\nname=\"x\"\nversion=\"0.0.1\"\n").unwrap();

    let unrelated = dir.path().join("else");
    std::fs::create_dir_all(&unrelated).unwrap();

    let target = resolve_project_target_dir(&unrelated, Some(&manifest)).expect("resolution");
    assert_eq!(target, manifest_root.join("target"));
}

#[test]
fn global_paths_include_soldr_owned_roots() {
    let cache_root = PathBuf::from("/fake/.soldr");
    let zccache = cache_root.join("cache").join("zccache");
    let paths = plan_global_paths(&cache_root, &zccache);
    // Cache, bench, runtime, state.redb, and the zccache subdir.
    assert!(paths.iter().any(|p: &PathBuf| p.ends_with("cache")));
    assert!(paths.iter().any(|p: &PathBuf| p.ends_with("bench")));
    assert!(paths.iter().any(|p: &PathBuf| p.ends_with("runtime")));
    assert!(paths.iter().any(|p: &PathBuf| p.ends_with("state.redb")));
    assert!(paths.iter().any(|p: &PathBuf| p == &zccache));
}

#[test]
fn project_paths_use_workspace_target() {
    let workspace = PathBuf::from("/fake/proj");
    let paths = plan_project_paths(&workspace);
    assert_eq!(paths, vec![workspace.join("target")]);
}

// -- Undo filtering --------------------------------------------------------

#[test]
fn undo_filter_only_returns_soldr_added_entries() {
    let managed = ManagedExclusionFile {
        schema_version: 1,
        exclusions: vec![
            ManagedExclusion {
                path: "C:\\Users\\you\\.soldr\\cache".into(),
                added_at_unix: 1_715_000_000,
                scope: "global".into(),
            },
            ManagedExclusion {
                path: "C:\\Users\\you\\dev\\proj\\target".into(),
                added_at_unix: 1_715_001_000,
                scope: "project".into(),
            },
        ],
    };
    // Defender currently has soldr-added entries PLUS a user-added one.
    let current_defender = vec![
        "C:\\Users\\you\\.soldr\\cache".to_string(),
        "C:\\Users\\you\\dev\\proj\\target".to_string(),
        "C:\\Users\\you\\Documents\\Personal".to_string(), // user-added
    ];
    let to_remove = filter_undo_entries(&managed, &current_defender, None);
    assert_eq!(to_remove.len(), 2);
    assert!(to_remove
        .iter()
        .all(|p: &String| p.contains(".soldr") || p.contains("target")));
    assert!(!to_remove
        .iter()
        .any(|p: &String| p.contains("Documents\\Personal")));
}

#[test]
fn undo_filter_scoped_to_global_only() {
    let managed = ManagedExclusionFile {
        schema_version: 1,
        exclusions: vec![
            ManagedExclusion {
                path: "C:\\Users\\you\\.soldr\\cache".into(),
                added_at_unix: 1_715_000_000,
                scope: "global".into(),
            },
            ManagedExclusion {
                path: "C:\\Users\\you\\dev\\proj\\target".into(),
                added_at_unix: 1_715_001_000,
                scope: "project".into(),
            },
        ],
    };
    let to_remove = filter_undo_entries(
        &managed,
        &["C:\\Users\\you\\.soldr\\cache".into()],
        Some(OptimizeScope::Global),
    );
    assert_eq!(to_remove.len(), 1);
    assert!(to_remove[0].contains(".soldr"));
}

#[test]
fn undo_skips_entries_already_absent_from_defender() {
    let managed = ManagedExclusionFile {
        schema_version: 1,
        exclusions: vec![ManagedExclusion {
            path: "C:\\Users\\you\\.soldr\\cache".into(),
            added_at_unix: 1_715_000_000,
            scope: "global".into(),
        }],
    };
    // User already removed this exclusion manually.
    let current_defender: Vec<String> = Vec::new();
    let to_remove = filter_undo_entries(&managed, &current_defender, None);
    // Still want to attempt removal (or at least drop from the manifest);
    // we expose the entries to caller; the action layer decides.
    assert_eq!(to_remove.len(), 1);
}

// -- JSON schema stability -------------------------------------------------

#[test]
fn optimize_output_round_trips_through_json() {
    let output = OptimizeOutput::sample();
    let json = serde_json::to_string(&output).expect("serialize");
    let back: OptimizeOutput = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(output, back);
}

#[test]
fn optimize_output_advertises_schema_version() {
    let output = OptimizeOutput::sample();
    let json = serde_json::to_value(&output).expect("serialize");
    assert_eq!(json["schema_version"], crate::JSON_SCHEMA_VERSION);
    assert_eq!(json["command"], "optimize");
}
