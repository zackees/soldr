//! Regression guard that enforces use of the `timed_test!` macro.
//!
//! This test walks every `.rs` file under each workspace crate's
//! `src/` and `tests/` (`crates/*/`) and asserts that *new* `#[test]`
//! attributes are wrapped by the `timed_test!` macro instead of being
//! declared bare. A bare `#[test]` outside the legacy allowlist fails
//! the build — the contributor must either:
//!
//!   1. Convert the test to `timed_test!(name, { ... })` (default
//!      2-minute deadline) or `timed_test!(name, Duration::from_secs(N), { ... })`,
//!   2. Add an explicit `#[ignore]` next to the `#[test]` so the test
//!      cannot hang the suite, or
//!   3. As a last resort, add the file to `LEGACY_ALLOWLIST` below
//!      with a comment explaining why.
//!
//! `LEGACY_ALLOWLIST` is intentionally non-empty: the watchdog landed
//! after ~780 pre-existing tests, and retrofitting every single one
//! is out of scope for the watchdog-infrastructure PR. The allowlist
//! shrinks as files are migrated.
//!
//! Implementation notes:
//!
//! * The lint runs as a normal integration test (`cargo test --test
//!   timed_test_lint`) so it executes on every CI run without needing
//!   any extra cargo machinery (dylint, clippy plugin, build script,
//!   …). Running it under `timed_test!` itself keeps it honest — if
//!   the lint hangs walking the tree, the watchdog catches it.
//! * Lines containing `// allow-bare-test:` are treated as opt-out
//!   markers (intentional bare-`#[test]` declarations, e.g. for tests
//!   that must run inside `#[should_panic]` with no watchdog wrapper).
//! * Lines containing `#[ignore]` immediately before / after the
//!   `#[test]` attribute are exempt — an ignored test cannot hang the
//!   suite because it never runs without `--ignored`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use soldr_cli::timed_test;

mod common;

/// Files that still contain bare `#[test]` declarations. New entries
/// should NOT be added without explicit reviewer sign-off — migrate
/// the file to `timed_test!` instead. The list is sorted to make diffs
/// stable.
const LEGACY_ALLOWLIST: &[&str] = &[
    // ----- src/ unit-test modules -----
    "crates/soldr-cli/src/binaries.rs",
    "crates/soldr-cli/src/cache/session.rs",
    "crates/soldr-cache/src/cache_lib/auto_gc.rs",
    "crates/soldr-cache/src/cache_lib/auto_target_gc.rs",
    "crates/soldr-cache/src/cache_lib/gc.rs",
    "crates/soldr-cache/src/cache_lib/mod.rs",
    "crates/soldr-cache/src/cache_lib/prune_target_tests.rs",
    "crates/soldr-cache/src/cache_lib/state_db.rs",
    "crates/soldr-cache/src/cache_lib/strip_target.rs",
    "crates/soldr-cache/src/cache_lib/target_registry.rs",
    "crates/soldr-cli/src/cargo_diagnostics.rs",
    "crates/soldr-cli/src/cargo_front_door/clang_cl_shim.rs",
    "crates/soldr-cli/src/cargo_front_door/tests.rs",
    "crates/soldr-cli/src/cook_tests.rs",
    "crates/soldr-core/src/core/mod.rs",
    "crates/soldr-core/src/core/paths.rs",
    "crates/soldr-core/src/core/target_triple.rs",
    "crates/soldr-core/src/core/toolchain_manifest.rs",
    "crates/soldr-core/src/core/toolchain_resolve.rs",
    "crates/soldr-daemon/src/daemon/db.rs",
    "crates/soldr-daemon/src/daemon/ipc.rs",
    "crates/soldr-daemon/src/daemon/lifecycle.rs",
    "crates/soldr-core/src/defender_probe.rs",
    "crates/soldr-cli/src/doctor.rs",
    "crates/soldr-fetch/src/fetch/github.rs",
    "crates/soldr-fetch/src/fetch/known_tools.rs",
    "crates/soldr-fetch/src/fetch/mod.rs",
    "crates/soldr-fetch/src/fetch/rustup_init.rs",
    "crates/soldr-fetch/src/fetch/trust.rs",
    "crates/soldr-core/src/fuzzy_match.rs",
    "crates/soldr-cli/src/gc/tests.rs",
    "crates/soldr-cli/src/linker.rs",
    "crates/soldr-cli/src/main_tests.rs",
    "crates/soldr-cli/src/native_cc_tests.rs",
    "crates/soldr-cli/src/optimize_detect.rs",
    "crates/soldr-cli/src/optimize_tests.rs",
    "crates/soldr-cli/src/optimize_windows.rs",
    "crates/soldr-cli/src/rust_plan_tests/bundle_walk.rs",
    "crates/soldr-cli/src/rust_plan_tests/manifest.rs",
    "crates/soldr-cli/src/rust_plan_tests/orphan_rmeta.rs",
    "crates/soldr-cli/src/rust_plan_tests/plan_build.rs",
    "crates/soldr-cli/src/rust_plan_tests/prepopulated_target.rs",
    "crates/soldr-cli/src/rust_plan_tests/warm_restore.rs",
    "crates/soldr-cli/src/rust_plan_tests/wire_compat.rs",
    "crates/soldr-core/src/self_relocate.rs",
    "crates/soldr-cli/src/shim_dir.rs",
    "crates/soldr-core/src/startup_profile.rs",
    "crates/soldr-cli/src/toolchain_ensure.rs",
    "crates/soldr-cli/src/toolchain_link.rs",
    "crates/soldr-cli/src/trampoline_config_tests.rs",
    "crates/soldr-cli/src/trampoline_tests.rs",
    "crates/soldr-cli/src/wrapper.rs",
    "crates/soldr-cli/src/wrapper_target.rs",
    "crates/soldr-cli/src/zccache.rs",
    // ----- tests/ integration suites -----
    "crates/soldr-cli/tests/cli_build_alias_parity.rs",
    "crates/soldr-cli/tests/cli_cache.rs",
    "crates/soldr-cli/tests/cli_cache_prune.rs",
    "crates/soldr-cli/tests/cli_cache_trim.rs",
    "crates/soldr-cli/tests/cli_cargo_basic.rs",
    "crates/soldr-cli/tests/cli_cargo_linker.rs",
    "crates/soldr-cli/tests/cli_cargo_run_trampoline.rs",
    "crates/soldr-cli/tests/cli_cargo_wrappers.rs",
    "crates/soldr-cli/tests/cli_cook.rs",
    "crates/soldr-cli/tests/cli_daemon_builds.rs",
    "crates/soldr-cli/tests/cli_daemon_lifecycle.rs",
    "crates/soldr-cli/tests/cli_daemon_target_touch.rs",
    "crates/soldr-cli/tests/cli_dispatch.rs",
    "crates/soldr-cli/tests/cli_doctor.rs",
    "crates/soldr-cli/tests/cli_gc.rs",
    "crates/soldr-cli/tests/cli_gc_extras.rs",
    "crates/soldr-cli/tests/cli_gc_git_checkouts.rs",
    "crates/soldr-cli/tests/cli_gc_registry_src.rs",
    "crates/soldr-cli/tests/cli_optimize.rs",
    "crates/soldr-cli/tests/cli_rust_plan.rs",
    "crates/soldr-cli/tests/cli_toolchain.rs", // partially retrofitted; still contains bare #[test]s
    "crates/soldr-cli/tests/cli_unknown_session_retry.rs",
    "crates/soldr-cli/tests/cli_wrapper.rs",
    "crates/soldr-cli/tests/cli_wrapper_perf.rs",
    "crates/soldr-cli/tests/save_roundtrip.rs",
];

/// Files that are part of the lint itself / the watchdog impl. These
/// are exempt because (a) the watchdog's own unit tests cannot use
/// the macro recursively, and (b) the lint scanner here is a single
/// `timed_test!` and runs under its own watchdog already.
const LINT_OWN_FILES: &[&str] = &[
    "crates/soldr-core/src/test_util.rs",
    "crates/soldr-cli/tests/test_watchdog_self_test.rs",
    "crates/soldr-cli/tests/timed_test_lint.rs",
];

/// Resolve the workspace root (two levels up from this crate's
/// `CARGO_MANIFEST_DIR`, which is `crates/soldr-cli`). The lint scans
/// every workspace crate under `crates/` (#1490 split) so moved
/// modules stay covered.
fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/soldr-cli has a workspace root two levels up")
        .to_path_buf()
}

/// Recursively collect `.rs` files under `dir`, skipping `target/`
/// and other build-output directories that should never contain
/// hand-written tests.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if name == "target" || name == ".git" || name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Returns true if the file body contains at least one bare `#[test]`
/// attribute that is NOT already paired with `#[ignore]` and is NOT
/// marked with the `// allow-bare-test:` opt-out comment.
///
/// The check is intentionally a textual scan rather than a full Rust
/// parser: we want zero extra dev-dependencies for the lint, and the
/// shape of `#[test]` is stable enough that string matching is
/// reliable for this codebase.
fn has_bare_test(body: &str) -> bool {
    let lines: Vec<&str> = body.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !(trimmed == "#[test]" || trimmed.starts_with("#[test]")) {
            continue;
        }
        // The macro definition itself contains the substring `#[test]`
        // inside `macro_rules!`. Skip lines that are clearly inside a
        // macro body by checking for a leading `$crate` / `stringify!`
        // / `fn $name` neighbor.
        // The dedicated macro file is on LINT_OWN_FILES so we never
        // reach here for it, but be defensive.

        // Opt-out marker on the same line.
        if line.contains("// allow-bare-test") {
            continue;
        }

        // Allow when the immediately following or preceding line
        // contains `#[ignore`. An ignored test never runs by default.
        let neighbor_has_ignore = idx
            .checked_sub(1)
            .and_then(|i| lines.get(i))
            .map(|l| l.contains("#[ignore"))
            .unwrap_or(false)
            || lines
                .get(idx + 1)
                .map(|l| l.contains("#[ignore"))
                .unwrap_or(false)
            || lines
                .get(idx + 2)
                .map(|l| l.contains("#[ignore"))
                .unwrap_or(false);
        if neighbor_has_ignore {
            continue;
        }

        return true;
    }
    false
}

/// Render a file path relative to the crate root using forward slashes
/// so the legacy allowlist is stable across Windows and Unix.
fn relative_unix_path(abs: &Path, root: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    Some(s)
}

timed_test!(every_test_uses_timed_test_macro_or_is_allowlisted, {
    // soldr#1040 phase 2: source-tree-coupled — scans src/ + tests/
    // directories from disk. Skip on cross-build target runners.
    if common::should_skip_source_tree_test("every_test_uses_timed_test_macro_or_is_allowlisted") {
        return;
    }
    let root = workspace_root();
    let mut files = Vec::new();
    let crates_dir = root.join("crates");
    let entries = fs::read_dir(&crates_dir).expect("read crates/ dir");
    for entry in entries.flatten() {
        let crate_dir = entry.path();
        if !crate_dir.is_dir() {
            continue;
        }
        collect_rs_files(&crate_dir.join("src"), &mut files);
        collect_rs_files(&crate_dir.join("tests"), &mut files);
    }

    let allow: BTreeSet<&str> = LEGACY_ALLOWLIST
        .iter()
        .copied()
        .chain(LINT_OWN_FILES.iter().copied())
        .collect();

    let mut offenders: Vec<String> = Vec::new();
    let mut allowlist_clean: BTreeSet<String> = BTreeSet::new();

    for file in &files {
        let rel = match relative_unix_path(file, &root) {
            Some(r) => r,
            None => continue,
        };
        let body = match fs::read_to_string(file) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let bare = has_bare_test(&body);
        let listed = allow.contains(rel.as_str());

        if bare && !listed {
            offenders.push(rel);
        } else if !bare && LEGACY_ALLOWLIST.contains(&rel.as_str()) {
            // Allowlisted file no longer has bare `#[test]` — it's
            // safe to remove from LEGACY_ALLOWLIST. Surface this so
            // contributors are nudged to trim the list.
            allowlist_clean.insert(rel);
        }
    }

    if !offenders.is_empty() {
        let mut msg = String::from(
            "The following files contain bare `#[test]` declarations \
             that must be wrapped with the `timed_test!` macro \
             (see crates/soldr-cli/src/test_util.rs). Either convert \
             them, add `#[ignore]`, mark the line with \
             `// allow-bare-test: <reason>`, or — as a last resort — \
             add the file to LEGACY_ALLOWLIST in \
             tests/timed_test_lint.rs.\n\n",
        );
        offenders.sort();
        for f in &offenders {
            msg.push_str("  - ");
            msg.push_str(f);
            msg.push('\n');
        }
        panic!("{msg}");
    }

    if !allowlist_clean.is_empty() {
        let mut msg = String::from(
            "The following files are on LEGACY_ALLOWLIST in \
             tests/timed_test_lint.rs but no longer contain bare \
             `#[test]` declarations. Please remove them from the \
             allowlist to keep the lint tight:\n\n",
        );
        for f in &allowlist_clean {
            msg.push_str("  - ");
            msg.push_str(f);
            msg.push('\n');
        }
        panic!("{msg}");
    }
});
