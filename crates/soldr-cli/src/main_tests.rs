//! Unit tests for the dispatch layer in `main.rs`: clap top-level
//! parsing, `--as` extraction, version normalisation, and the
//! self-relocate gate. Lives in a sibling file referenced via
//! `#[path]` so `main.rs` stays comfortably under the 1000-LOC ceiling.

use super::*;
use clap::Parser;
use std::ffi::{OsStr, OsString};
use std::sync::Mutex;

/// Serialises tests that mutate process-wide environment variables so
/// they do not race with each other under parallel `cargo test`. The
/// guard objects below restore the previous value on drop, but two
/// tests touching the same key concurrently would still observe each
/// other's mid-test state without this lock.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that sets or removes an environment variable for the
/// duration of a test and restores the previous value on drop.
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
fn gc_cli_parses_summary_and_purge_modes() {
    let summary = Cli::try_parse_from(["soldr", "gc", "--json"]).unwrap();
    match summary.command {
        Commands::Gc {
            command: None,
            json,
            ..
        } => assert!(json, "gc --json should parse as summary JSON"),
        _ => panic!("expected gc summary command"),
    }

    let purge = Cli::try_parse_from([
        "soldr",
        "gc",
        "purge",
        "--all",
        "--older-than",
        "30d",
        "--larger-than",
        "1GB",
    ])
    .unwrap();
    match purge.command {
        Commands::Gc {
            command:
                Some(GcSubcommand::Purge {
                    all,
                    older_than,
                    larger_than,
                    ..
                }),
            ..
        } => {
            assert!(all);
            assert_eq!(older_than, "30d");
            assert_eq!(larger_than, "1GB");
        }
        _ => panic!("expected gc purge command"),
    }
}

#[test]
fn self_relocate_gate_targets_managed_cacheable_cargo_builds() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _wrapper = EnvVarGuard::remove(RUSTC_WRAPPER_OVERRIDE_ENV_VAR);

    assert!(should_self_relocate_for_invocation(&[
        "soldr".into(),
        "cargo".into(),
        "build".into(),
    ]));
    assert!(should_self_relocate_for_invocation(&[
        "soldr".into(),
        "--as".into(),
        env!("CARGO_PKG_VERSION").into(),
        "cargo".into(),
        "test".into(),
    ]));
    assert!(!should_self_relocate_for_invocation(&[
        "soldr".into(),
        "cargo".into(),
        "--version".into(),
    ]));
    assert!(!should_self_relocate_for_invocation(&[
        "soldr".into(),
        "--no-cache".into(),
        "cargo".into(),
        "build".into(),
    ]));
    assert!(!should_self_relocate_for_invocation(&[
        "soldr".into(),
        "version".into(),
    ]));

    let _custom = EnvVarGuard::set(RUSTC_WRAPPER_OVERRIDE_ENV_VAR, "sccache");
    assert!(!should_self_relocate_for_invocation(&[
        "soldr".into(),
        "cargo".into(),
        "build".into(),
    ]));
}

#[test]
fn extract_as_pin_extracts_space_separated_flag_before_subcommand() {
    let (version, rest) = extract_as_pin(&[
        "--as".into(),
        "0.5.2".into(),
        "cargo".into(),
        "build".into(),
    ])
    .unwrap();
    assert_eq!(version, Some("0.5.2".into()));
    assert_eq!(rest, vec!["cargo".to_string(), "build".into()]);
}

#[test]
fn extract_as_pin_extracts_equals_form() {
    let (version, rest) =
        extract_as_pin(&["--as=0.5.2".into(), "cargo".into(), "build".into()]).unwrap();
    assert_eq!(version, Some("0.5.2".into()));
    assert_eq!(rest, vec!["cargo".to_string(), "build".into()]);
}

#[test]
fn extract_as_pin_preserves_other_leading_flags() {
    let (version, rest) = extract_as_pin(&[
        "--no-cache".into(),
        "--as".into(),
        "0.5.2".into(),
        "cargo".into(),
    ])
    .unwrap();
    assert_eq!(version, Some("0.5.2".into()));
    assert_eq!(rest, vec!["--no-cache".to_string(), "cargo".into()]);
}

#[test]
fn extract_as_pin_ignores_flag_after_subcommand() {
    let args = vec!["cargo".into(), "--as".into(), "0.5.2".into()];
    let (version, rest) = extract_as_pin(&args).unwrap();
    assert_eq!(version, None);
    assert_eq!(rest, args);
}

#[test]
fn extract_as_pin_ignores_flag_after_passthrough_separator() {
    let args = vec!["cargo".into(), "--".into(), "--as".into(), "0.5.2".into()];
    let (version, rest) = extract_as_pin(&args).unwrap();
    assert_eq!(version, None);
    assert_eq!(rest, args);
}

#[test]
fn extract_as_pin_rejects_missing_value() {
    let err = extract_as_pin(&["--as".into()]).unwrap_err();
    assert!(err.to_string().contains("requires a version"));
}

#[test]
fn extract_as_pin_rejects_empty_value() {
    let err = extract_as_pin(&["--as".into(), "".into()]).unwrap_err();
    assert!(err.to_string().contains("must not be empty"));
    let err2 = extract_as_pin(&["--as=".into()]).unwrap_err();
    assert!(err2.to_string().contains("requires a version"));
}

#[test]
fn extract_as_pin_rejects_duplicate_flag() {
    let err = extract_as_pin(&["--as".into(), "0.5.2".into(), "--as=0.4.0".into()]).unwrap_err();
    assert!(err.to_string().contains("more than once"));
}

#[test]
fn normalize_version_strips_leading_v() {
    assert_eq!(normalize_version("0.5.2"), "0.5.2");
    assert_eq!(normalize_version("v0.5.2"), "0.5.2");
    assert_eq!(normalize_version("  v0.5.2 "), "0.5.2");
}

#[test]
fn should_trampoline_matches_current_version_as_no_op() {
    assert!(!should_trampoline(env!("CARGO_PKG_VERSION")));
    assert!(!should_trampoline(&format!(
        "v{}",
        env!("CARGO_PKG_VERSION")
    )));
    assert!(should_trampoline("0.0.0-not-this-version"));
}
