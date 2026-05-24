//! Unit tests for [`crate::cargo_front_door`]: the cargo argv parser, the
//! low-disk warning helper, and the cargo-subcommand sniffer.
//! Lives in a sibling file referenced via `#[path]` so `cargo_front_door.rs`
//! stays comfortably under the 1000-LOC ceiling.

use super::*;
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
