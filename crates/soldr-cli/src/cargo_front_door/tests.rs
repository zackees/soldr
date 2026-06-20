//! Unit tests for [`crate::cargo_front_door`]: the cargo argv parser, the
//! low-disk warning helper, and the cargo-subcommand sniffer.
//! Lives inside the `cargo_front_door/` module directory so `mod.rs`
//! stays comfortably under the 1000-LOC ceiling.

use super::*;
use crate::LOW_DISK_WARNING_THRESHOLD_BYTES;
use std::ffi::{OsStr, OsString};
use std::sync::Mutex;

/// Serialises tests that mutate process-wide environment variables so
/// they don't race under parallel `cargo test`.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
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

fn command_env_override(
    command: &std::process::Command,
    key: &'static str,
) -> Option<Option<OsString>> {
    command
        .get_envs()
        .find(|(candidate, _)| *candidate == OsStr::new(key))
        .map(|(_, value)| value.map(OsString::from))
}

#[test]
fn child_cargo_scrubs_soldr_cache_lifecycle_controls() {
    let mut command = std::process::Command::new("cargo");
    command.env(SOLDR_CACHE_LIFECYCLE_ENV_VAR, "command");
    command.env(SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR, "1");

    scrub_soldr_cache_lifecycle_env_for_child_cargo(&mut command);

    assert_eq!(
        command_env_override(&command, SOLDR_CACHE_LIFECYCLE_ENV_VAR),
        Some(None)
    );
    assert_eq!(
        command_env_override(&command, SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS_ENV_VAR),
        Some(None)
    );
}

#[test]
fn fresh_workspace_env_guard_removes_and_restores_soldr_workspace_state() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _zccache = EnvVarGuard::set(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR, "/old/zccache");
    let _target_bundle = EnvVarGuard::set(crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR, "/old/bundle");
    let _setup = EnvVarGuard::set("SETUP_SOLDR_WORKSPACE", "/old/workspace");
    let _cache_dir = EnvVarGuard::set("SOLDR_CACHE_DIR", "/intentional/cache");

    {
        let _guard = FreshSoldrWorkspaceEnvGuard::apply_unless_trusted(false);

        assert!(std::env::var_os(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR).is_none());
        assert!(std::env::var_os(crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR).is_none());
        assert!(std::env::var_os("SETUP_SOLDR_WORKSPACE").is_none());
        assert_eq!(
            std::env::var_os("SOLDR_CACHE_DIR"),
            Some(OsString::from("/intentional/cache"))
        );
    }

    assert_eq!(
        std::env::var_os(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR),
        Some(OsString::from("/old/zccache"))
    );
    assert_eq!(
        std::env::var_os(crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR),
        Some(OsString::from("/old/bundle"))
    );
    assert_eq!(
        std::env::var_os("SETUP_SOLDR_WORKSPACE"),
        Some(OsString::from("/old/workspace"))
    );
}

#[test]
fn trusted_workspace_env_guard_leaves_inherited_soldr_state_available() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _zccache = EnvVarGuard::set(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR, "/old/zccache");

    let _guard = FreshSoldrWorkspaceEnvGuard::apply_unless_trusted(true);

    assert_eq!(
        std::env::var_os(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR),
        Some(OsString::from("/old/zccache"))
    );
}

#[test]
fn child_cargo_scrubs_inherited_soldr_workspace_state() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _setup = EnvVarGuard::set("SETUP_SOLDR_WORKSPACE", "/old/workspace");
    let mut command = std::process::Command::new("cargo");
    command.env(crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR, "/old/zccache");
    command.env(crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR, "/old/bundle");

    scrub_inherited_soldr_workspace_env_for_child_cargo(&mut command);

    assert_eq!(
        command_env_override(&command, crate::cache_lib::ZCCACHE_CACHE_DIR_ENV_VAR),
        Some(None)
    );
    assert_eq!(
        command_env_override(&command, crate::TARGET_CACHE_BUNDLE_DIR_ENV_VAR),
        Some(None)
    );
    assert_eq!(
        command_env_override(&command, "SETUP_SOLDR_WORKSPACE"),
        Some(None)
    );
}

#[test]
fn low_disk_warning_formats_yellow_below_threshold() {
    let message = low_disk_warning_for_free_bytes(1536 * 1024 * 1024, true)
        .expect("expected low-disk warning below threshold");
    assert!(message.contains("\x1b[33mwarning\x1b[0m"));
    assert!(message.contains("1.5 GB free"));
    assert!(message.contains("Run `soldr gc`"));
}

#[test]
fn low_disk_warning_omits_at_threshold() {
    assert!(low_disk_warning_for_free_bytes(LOW_DISK_WARNING_THRESHOLD_BYTES, true).is_none());
}

#[test]
fn low_disk_probe_failure_is_nonfatal() {
    let warning = low_disk_warning_for_path(std::path::Path::new("."), true, |_| {
        Err(std::io::Error::other("probe failed"))
    });
    assert!(warning.is_none());
}

#[test]
fn cargo_args_detect_explicit_target_flag() {
    assert!(cargo_args_specify_target(&[
        "build".into(),
        "--target".into(),
        "x86_64-pc-windows-msvc".into(),
    ]));
    assert!(cargo_args_specify_target(&[
        "build".into(),
        "--target=x86_64-pc-windows-msvc".into(),
    ]));
}

#[test]
fn cargo_args_ignore_target_after_passthrough_separator() {
    assert!(!cargo_args_specify_target(&[
        "test".into(),
        "--".into(),
        "--target".into(),
        "ignored".into(),
    ]));
}

#[test]
fn cargo_args_reject_reserved_no_cache_before_passthrough_separator() {
    assert!(cargo_args_use_reserved_no_cache(&[
        "build".into(),
        "--no-cache".into(),
    ]));
    assert!(!cargo_args_use_reserved_no_cache(&[
        "test".into(),
        "--".into(),
        "--no-cache".into(),
    ]));
}

#[test]
fn first_cargo_subcommand_skips_leading_flags() {
    assert_eq!(
        first_cargo_subcommand(&["--verbose".into(), "nextest".into(), "run".into()]),
        Some("nextest")
    );
    assert_eq!(
        first_cargo_subcommand(&["nextest".into(), "run".into()]),
        Some("nextest")
    );
    assert_eq!(first_cargo_subcommand(&["--help".into()]), None);
    assert_eq!(first_cargo_subcommand(&[]), None);
}

#[test]
fn first_cargo_subcommand_stops_at_passthrough_separator() {
    assert_eq!(
        first_cargo_subcommand(&["--".into(), "nextest".into()]),
        None
    );
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn cargo_args_are_cacheable_for_direct_build() {
    assert!(cargo_args_are_cacheable(&argv(&["build"])));
    assert!(cargo_args_are_cacheable(&argv(&["build", "--release"])));
    assert!(cargo_args_are_cacheable(&argv(&["b"])));
}

#[test]
fn cargo_args_are_cacheable_for_chef_cook() {
    // soldr cook (issue #359) routes `cargo chef cook` through this front
    // door. The outer process orchestrates an inner `cargo build` against
    // a stub project, so we must seed RUSTC_WRAPPER for the inner build
    // to pick zccache up.
    assert!(cargo_args_are_cacheable(&argv(&["chef", "cook"])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "chef",
        "cook",
        "--release",
    ])));
    assert!(cargo_args_are_cacheable(&argv(&["chef", "prepare"])));
}

#[test]
fn cargo_args_are_not_cacheable_for_direct_clean() {
    assert!(!cargo_args_are_cacheable(&argv(&["clean"])));
    assert!(!cargo_args_are_cacheable(&argv(&["fmt"])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_short_exec_single_token() {
    assert!(cargo_args_are_cacheable(&argv(&["watch", "-x", "build"])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_short_exec_multi_token() {
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "-x",
        "build --release",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_long_exec_equals_form() {
    assert!(cargo_args_are_cacheable(&argv(&["watch", "--exec=build"])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "--exec=build --release",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_long_exec_space_form() {
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch", "--exec", "build",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_shell_form_strips_leading_cargo() {
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "-s",
        "cargo build --release",
    ])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "--shell",
        "cargo build --release",
    ])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "--shell=cargo build --release",
    ])));
}

#[test]
fn cargo_args_are_not_cacheable_for_watch_with_uncacheable_inner() {
    assert!(!cargo_args_are_cacheable(&argv(&["watch", "-x", "clean"])));
    assert!(!cargo_args_are_cacheable(&argv(&["watch", "-x", "fmt"])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_when_any_inner_is_cacheable() {
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch", "-x", "build", "-x", "clean",
    ])));
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch", "-x", "clean", "-x", "build",
    ])));
}

#[test]
fn cargo_args_are_not_cacheable_for_bare_watch() {
    assert!(!cargo_args_are_cacheable(&argv(&["watch"])));
    assert!(!cargo_args_are_cacheable(&argv(&["watch", "--clear"])));
}

#[test]
fn cargo_args_ignore_exec_after_passthrough_separator() {
    // Anything after `--` is not parsed as a watch-flag value.
    assert!(!cargo_args_are_cacheable(&argv(&[
        "watch", "--", "-x", "build",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_inner_release_flag() {
    // `-x 'build --release'` — tokens after `build` should not break the
    // detection, and the outer cacheable answer is still true.
    assert!(cargo_args_are_cacheable(&argv(&[
        "watch",
        "-x",
        "build --release --workspace",
    ])));
}

#[test]
fn cargo_args_are_cacheable_for_watch_with_toolchain_pin() {
    // `+nightly` is a cargo toolchain shorthand that should be skipped when
    // locating the `watch` subcommand.
    assert!(cargo_args_are_cacheable(&argv(&[
        "+nightly", "watch", "-x", "build",
    ])));
}

// -------------------------------------------------------------------------
// Auto target-GC flag stripping (#485). The soldr-private flags get pulled
// out of the arg vector before cargo ever sees them. The env var path is
// covered separately because it touches process state.
// -------------------------------------------------------------------------

#[test]
fn strip_no_gc_target_flag_removes_combined_form() {
    let (cleaned, opt) = strip_no_gc_target_flags(&argv(&["build", "--no-gc-target", "--release"]));
    assert_eq!(cleaned, argv(&["build", "--release"]));
    assert!(opt.before);
    assert!(opt.after);
}

#[test]
fn strip_no_gc_target_flag_removes_before_only() {
    let (cleaned, opt) = strip_no_gc_target_flags(&argv(&["check", "--no-gc-target-before"]));
    assert_eq!(cleaned, argv(&["check"]));
    assert!(opt.before);
    assert!(!opt.after);
}

#[test]
fn strip_no_gc_target_flag_removes_after_only() {
    let (cleaned, opt) =
        strip_no_gc_target_flags(&argv(&["build", "--no-gc-target-after", "--workspace"]));
    assert_eq!(cleaned, argv(&["build", "--workspace"]));
    assert!(!opt.before);
    assert!(opt.after);
}

#[test]
fn strip_no_gc_target_flag_default_no_op() {
    let (cleaned, opt) = strip_no_gc_target_flags(&argv(&["build", "--release"]));
    assert_eq!(cleaned, argv(&["build", "--release"]));
    assert!(!opt.before);
    assert!(!opt.after);
}

#[test]
fn strip_no_gc_target_flag_passes_through_after_separator() {
    // Flags after `--` belong to the program cargo runs and must not be
    // touched. This mirrors how `--no-trampoline` is handled.
    let (cleaned, opt) = strip_no_gc_target_flags(&argv(&[
        "run",
        "--bin",
        "foo",
        "--",
        "--no-gc-target",
        "--no-gc-target-after",
    ]));
    assert_eq!(
        cleaned,
        argv(&[
            "run",
            "--bin",
            "foo",
            "--",
            "--no-gc-target",
            "--no-gc-target-after",
        ])
    );
    assert!(!opt.before);
    assert!(!opt.after);
}

#[test]
fn strip_no_gc_target_flag_handles_repeated_flags() {
    let (cleaned, opt) = strip_no_gc_target_flags(&argv(&[
        "build",
        "--no-gc-target-before",
        "--no-gc-target-after",
    ]));
    assert_eq!(cleaned, argv(&["build"]));
    assert!(opt.before);
    assert!(opt.after);
}

#[test]
fn env_disables_target_gc_truthy_values() {
    let _lock = ENV_LOCK.lock().unwrap();
    for value in ["1", "true", "yes", "anything"] {
        let _guard = EnvVarGuard::set(NO_GC_TARGET_ENV_VAR, value);
        let merged = GcTargetOptOut::default().merged_with_env();
        assert!(
            merged.before && merged.after,
            "env value {value:?} should force both opt-outs"
        );
    }
}

#[test]
fn env_disables_target_gc_falsey_values_dont_opt_out() {
    let _lock = ENV_LOCK.lock().unwrap();
    for value in ["", "0", "false", "False"] {
        let _guard = EnvVarGuard::set(NO_GC_TARGET_ENV_VAR, value);
        let merged = GcTargetOptOut::default().merged_with_env();
        assert!(
            !merged.before && !merged.after,
            "env value {value:?} must not opt out"
        );
    }
}

#[test]
fn env_disables_target_gc_preserves_explicit_flag_opt_outs() {
    // If --no-gc-target-before is on the cli, the env var being unset
    // must not silently re-enable the after pass.
    let _lock = ENV_LOCK.lock().unwrap();
    let _guard = EnvVarGuard::remove(NO_GC_TARGET_ENV_VAR);
    let merged = GcTargetOptOut {
        before: true,
        after: false,
    }
    .merged_with_env();
    assert!(merged.before);
    assert!(!merged.after);
}

// Issue #755: cargo built-in verbs must not trigger the fuzzy "did you
// mean: cargo X?" hint. The External arm hands them straight to cargo;
// treating them as typos is misleading.
#[test]
fn suggest_cargo_subcommand_typo_skips_cargo_builtin_verbs() {
    for verb in crate::cli_args::CARGO_BUILTIN_VERBS {
        assert_eq!(
            suggest_cargo_subcommand_typo(verb),
            None,
            "cargo built-in verb {verb:?} must not be suggested as a typo of a known subcommand",
        );
    }
}

#[test]
fn suggest_cargo_subcommand_typo_still_catches_genuine_typos() {
    // Regression guard for issue #412: a clear typo of a registered
    // cargo subcommand (e.g. `ntest` → `nextest`) still gets the hint.
    assert_eq!(
        suggest_cargo_subcommand_typo("ntest").as_deref(),
        Some("nextest"),
        "fuzzy hint must still fire for genuine typos of known cargo subcommands",
    );
}

#[test]
fn suggest_cargo_subcommand_typo_returns_none_for_unrelated_input() {
    // Sanity check: random garbage that isn't close to any candidate
    // gets no suggestion at all.
    assert_eq!(
        suggest_cargo_subcommand_typo("completely-made-up-name"),
        None,
    );
}

// Issue #816: SOLDR_FORCE_MANAGED_CARGO_SUBCOMMANDS env-var handling.
// The bool guard parses truthy / falsy values consistently with the
// pattern other soldr env vars use.

#[test]
fn force_managed_cargo_subcommands_defaults_to_false_when_unset() {
    // Serialize on the env mutex used elsewhere in this file so we don't
    // race against other env-touching tests in the same binary.
    let _guard = ENV_LOCK.lock().unwrap();
    let prev = std::env::var_os(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    // SAFETY: the test acquires ENV_LOCK to serialize against any other
    // test that mutates process env.
    unsafe {
        std::env::remove_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    }
    assert!(!force_managed_cargo_subcommands());
    if let Some(value) = prev {
        unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, value);
        }
    }
}

#[test]
fn force_managed_cargo_subcommands_parses_falsey_strings_as_false() {
    let _guard = ENV_LOCK.lock().unwrap();
    let prev = std::env::var_os(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    for falsey in ["", " ", "0", "false", "no", "off", "  off  "] {
        unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, falsey);
        }
        assert!(
            !force_managed_cargo_subcommands(),
            "value {falsey:?} should parse as false",
        );
    }
    match prev {
        Some(value) => unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, value);
        },
        None => unsafe {
            std::env::remove_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
        },
    }
}

#[test]
fn force_managed_cargo_subcommands_parses_truthy_strings_as_true() {
    let _guard = ENV_LOCK.lock().unwrap();
    let prev = std::env::var_os(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
    for truthy in ["1", "true", "yes", "on", "anything-else"] {
        unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, truthy);
        }
        assert!(
            force_managed_cargo_subcommands(),
            "value {truthy:?} should parse as true",
        );
    }
    match prev {
        Some(value) => unsafe {
            std::env::set_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR, value);
        },
        None => unsafe {
            std::env::remove_var(FORCE_MANAGED_CARGO_SUBCOMMANDS_ENV_VAR);
        },
    }
}

#[test]
fn find_on_path_locates_executable_in_a_path_dir() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let exe_name = if cfg!(windows) {
        "soldr-test-find-on-path-fixture.exe"
    } else {
        "soldr-test-find-on-path-fixture"
    };
    let exe_path = dir.path().join(exe_name);
    std::fs::write(&exe_path, b"#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&exe_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&exe_path, perms).unwrap();
    }

    let prev_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = std::ffi::OsString::from(dir.path());
    if !prev_path.is_empty() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        new_path.push(sep);
        new_path.push(&prev_path);
    }
    unsafe {
        std::env::set_var("PATH", &new_path);
    }

    // The probe name is intentionally unsuffixed on both platforms — on
    // Windows the PATHEXT sweep in find_on_path picks up the `.exe`.
    let resolved = find_on_path("soldr-test-find-on-path-fixture");
    unsafe {
        std::env::set_var("PATH", &prev_path);
    }

    let resolved_path = resolved.expect("fixture must be found on PATH");
    assert!(
        resolved_path.is_file(),
        "resolved path {resolved_path:?} must exist",
    );
    assert!(
        resolved_path
            .parent()
            .map(|p| p == dir.path())
            .unwrap_or(false),
        "resolved path {resolved_path:?} must live under the fixture dir {:?}",
        dir.path(),
    );
}

#[test]
fn find_on_path_returns_none_when_missing() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let prev_path = std::env::var_os("PATH").unwrap_or_default();
    let new_path: std::ffi::OsString = dir.path().into();
    unsafe {
        std::env::set_var("PATH", &new_path);
    }
    let resolved = find_on_path("definitely-not-on-path-soldr-test-816");
    unsafe {
        std::env::set_var("PATH", &prev_path);
    }
    assert_eq!(resolved, None);
}
