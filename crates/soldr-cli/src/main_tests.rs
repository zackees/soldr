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
    // --zccache <value> in either form is a value-taking flag; the
    // self-relocate parser must not stop at the value as if it were the
    // subcommand.
    assert!(should_self_relocate_for_invocation(&[
        "soldr".into(),
        "--zccache".into(),
        "system".into(),
        "cargo".into(),
        "build".into(),
    ]));
    assert!(should_self_relocate_for_invocation(&[
        "soldr".into(),
        "--zccache=system".into(),
        "cargo".into(),
        "build".into(),
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
fn cli_parses_zccache_flag_values() {
    let default = Cli::try_parse_from(["soldr", "cargo", "build"]).unwrap();
    assert_eq!(default.zccache, ZccacheSourceArg::Managed);

    let system = Cli::try_parse_from(["soldr", "--zccache=system", "cargo", "build"]).unwrap();
    assert_eq!(system.zccache, ZccacheSourceArg::System);

    let managed = Cli::try_parse_from(["soldr", "--zccache", "managed", "cargo", "build"]).unwrap();
    assert_eq!(managed.zccache, ZccacheSourceArg::Managed);

    let invalid = Cli::try_parse_from(["soldr", "--zccache=bogus", "cargo", "build"]);
    assert!(invalid.is_err(), "unknown zccache source must fail clap");
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

#[test]
fn soldr_builtin_verbs_match_clap_subcommands_and_aliases() {
    // Issue #412: the SOLDR_BUILTIN_VERBS const drives the fuzzy
    // suggestion engine. If it drifts from the actual `Commands`
    // enum (someone adds a verb but forgets to update the list, or
    // renames one without updating the alias entry), users get
    // either bogus "did you mean?" hints or NO hint when one would
    // have helped. This test walks clap's discovered subcommands +
    // their `#[command(alias = ...)]` annotations and asserts every
    // discovered name is in the const, and vice versa.
    use clap::CommandFactory;

    let cmd = Cli::command();
    let mut discovered: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for sub in cmd.get_subcommands() {
        discovered.insert(sub.get_name().to_string());
        for alias in sub.get_all_aliases() {
            discovered.insert(alias.to_string());
        }
    }
    let declared: std::collections::BTreeSet<String> =
        SOLDR_BUILTIN_VERBS.iter().map(|s| s.to_string()).collect();

    let missing_from_const: Vec<_> = discovered.difference(&declared).collect();
    let extra_in_const: Vec<_> = declared.difference(&discovered).collect();
    assert!(
        missing_from_const.is_empty() && extra_in_const.is_empty(),
        "SOLDR_BUILTIN_VERBS is out of sync with the Commands enum.\n  \
         missing from const (clap knows, fuzzy-match doesn't): {missing_from_const:?}\n  \
         extra in const   (fuzzy-match knows, clap doesn't): {extra_in_const:?}\n\
         Update SOLDR_BUILTIN_VERBS in `main.rs` to match.",
    );
}

#[test]
fn fuzzy_match_with_soldr_verbs_recognizes_issue_412_examples() {
    // Black-box sanity check that the in-tree const list + the
    // shared fuzzy engine actually produce the suggestions the issue
    // documents as acceptance criteria. Cheap to keep alongside the
    // pure-logic tests in fuzzy_match.rs because it locks in the
    // const → suggestion contract end-to-end.
    use crate::fuzzy_match::suggest_close_match;

    let typo = "update-zccacheee";
    let suggestion = suggest_close_match(typo, SOLDR_BUILTIN_VERBS);
    assert!(
        matches!(suggestion, Some("update-zccache" | "install-zccache")),
        "expected install-zccache (or alias) for {typo:?}; got {suggestion:?}",
    );

    let typo = "installzccache";
    assert_eq!(
        suggest_close_match(typo, SOLDR_BUILTIN_VERBS),
        Some("install-zccache"),
    );

    // Acceptance criterion: completely-different verb produces no
    // hint (no false suggestion).
    assert_eq!(
        suggest_close_match("completely-made-up-name", SOLDR_BUILTIN_VERBS),
        None,
    );

    // Acceptance criterion: a verb that IS a known built-in must
    // NOT produce a hint — the regular dispatch handles it. Tested
    // by suggest_close_match returning None on exact match.
    assert_eq!(suggest_close_match("doctor", SOLDR_BUILTIN_VERBS), None);
}

/// Issue #683 (parent #682, phase 1): bare cargo-subcommand shorthand.
/// `soldr nextest run` must resolve as `soldr cargo nextest run`, NOT
/// fall through to a crates.io fetch for a literally-named `nextest`
/// crate. The dispatch decision in `main.rs` keys on
/// `parse_tool_spec` + `lookup_by_cargo_subcommand`; these tests
/// exercise that contract directly so the surface stays observable
/// without spawning a full `soldr` process.
#[test]
fn bare_shorthand_recognizes_every_known_cargo_subcommand() {
    // Every entry in `KNOWN_TOOLS` with a `cargo_subcommand: Some(_)`
    // must be reachable via the bare verb (sans `@version`). The
    // list is intentionally hard-coded — adding a new
    // `cargo_subcommand` to the registry without also adding it here
    // is the kind of drift this test exists to catch.
    let expected = [
        "nextest",
        "deny",
        "audit",
        "llvm-cov",
        "udeps",
        "semver-checks",
        "expand",
        "watch",
        "chef",
        "zigbuild",
        "xwin",
        "binstall",
        "machete",
    ];
    for verb in expected {
        let (crate_name, version) = parse_tool_spec(verb);
        assert!(
            matches!(version, VersionSpec::Latest),
            "bare verb {verb:?} must parse to VersionSpec::Latest"
        );
        assert!(
            crate::fetch::lookup_by_cargo_subcommand(&crate_name).is_some(),
            "bare verb {verb:?} must be matched by lookup_by_cargo_subcommand"
        );
    }
}

#[test]
fn bare_shorthand_skips_when_user_pinned_a_version() {
    // `soldr nextest@0.9.x` keeps the existing External fetch path
    // (which today errors with "no crate named nextest"). The
    // shorthand only fires for unversioned forms because the cargo
    // front door has no per-invocation version knob — pins are
    // managed in `KNOWN_TOOLS::pinned_version`.
    let (crate_name, version) = parse_tool_spec("nextest@0.9.x");
    assert_eq!(crate_name, "nextest");
    assert!(
        matches!(version, VersionSpec::Exact(_)),
        "pinned bare verb must parse to VersionSpec::Exact"
    );
    // Sanity: the lookup still matches the verb itself, so the gate
    // condition `matches!(version, VersionSpec::Latest) && lookup…`
    // depends entirely on the version arm. If a future refactor flips
    // this to use the lookup result alone, this test will surface
    // that mistake.
    assert!(crate::fetch::lookup_by_cargo_subcommand(&crate_name).is_some());
}

#[test]
fn bare_shorthand_does_not_capture_unrelated_verbs() {
    // Negative coverage for the phase-1 (cargo subcommand) hop: a
    // verb that ISN'T in `KNOWN_TOOLS::cargo_subcommand` must NOT
    // be matched by `lookup_by_cargo_subcommand`. Top-level fetch
    // tools (cross, mdbook, bacon) and made-up names go to External;
    // cargo built-ins (build, test) take the phase-2 hop instead.
    for verb in [
        "cross",
        "mdbook",
        "bacon",
        "completely-made-up-name",
        "build",
        "test",
    ] {
        let (crate_name, _) = parse_tool_spec(verb);
        assert!(
            crate::fetch::lookup_by_cargo_subcommand(&crate_name).is_none(),
            "verb {verb:?} must NOT be matched as a cargo subcommand in phase 1"
        );
    }
}

/// Issue #685 (parent #682, phase 2): bare cargo-built-in
/// shorthand. `soldr build` / `soldr test` / `soldr clippy` etc.
/// must resolve as `soldr cargo <verb>` via the External arm,
/// NOT fall through to a doomed crates.io fetch. The dispatch
/// decision keys on `parse_tool_spec` + `is_cargo_builtin_verb`;
/// these tests exercise that contract directly.
#[test]
fn cargo_builtin_shorthand_covers_every_verb_in_the_const() {
    // Every entry in `CARGO_BUILTIN_VERBS` must round-trip through
    // `is_cargo_builtin_verb` and through `parse_tool_spec` as a
    // bare, version-unpinned form. The list is intentionally
    // hard-coded — adding a new cargo verb to the const without
    // also adding it here is the kind of drift this test catches.
    let expected = [
        "build",
        "test",
        "check",
        "run",
        "bench",
        "doc",
        "fmt",
        "clippy",
        "tree",
        "update",
        "fix",
        "add",
        "remove",
        "metadata",
        "pkgid",
        "search",
        "vendor",
        "yank",
        "owner",
        "login",
        "logout",
        "init",
        "new",
        "generate-lockfile",
        "verify-project",
        "locate-project",
        "report",
        "install",
        "uninstall",
        "publish",
    ];
    for verb in expected {
        let (crate_name, version) = parse_tool_spec(verb);
        assert!(
            matches!(version, VersionSpec::Latest),
            "bare verb {verb:?} must parse to VersionSpec::Latest"
        );
        assert!(
            is_cargo_builtin_verb(&crate_name),
            "bare cargo built-in verb {verb:?} must be matched by is_cargo_builtin_verb"
        );
    }
    // And the reverse: every const entry is in the expected list.
    // Cheap, but if a future cargo gains a new verb and someone adds
    // it to the const without updating tests, this fails loudly.
    for verb in CARGO_BUILTIN_VERBS {
        assert!(
            expected.contains(verb),
            "CARGO_BUILTIN_VERBS gained {verb:?} without a test update"
        );
    }
}

#[test]
fn cargo_builtin_shorthand_excludes_soldr_native_collision_verbs() {
    // The three collision verbs (`clean`, `config`, `version`) own
    // a soldr-native meaning today and MUST NOT be remapped to
    // cargo by the phase-2 hop. Clap captures them before the
    // External arm runs, but we also assert the const itself
    // excludes them — defense in depth + intent documentation.
    for collision_verb in ["clean", "config", "version"] {
        assert!(
            !is_cargo_builtin_verb(collision_verb),
            "soldr-native verb {collision_verb:?} must NOT be in CARGO_BUILTIN_VERBS"
        );
    }
}

#[test]
fn cargo_builtin_shorthand_skips_when_user_pinned_a_version() {
    // `soldr build@1.0` keeps the existing External fetch path —
    // cargo built-ins have no per-invocation version dimension and
    // the parse-time `@version` form is reserved for the
    // crate-fetch path. Same shape as the phase-1
    // `bare_shorthand_skips_when_user_pinned_a_version` test.
    let (crate_name, version) = parse_tool_spec("build@1.0");
    assert_eq!(crate_name, "build");
    assert!(
        matches!(version, VersionSpec::Exact(_)),
        "pinned bare verb must parse to VersionSpec::Exact"
    );
    // Sanity: the lookup still matches the verb itself, so the gate
    // condition `matches!(version, VersionSpec::Latest) && is_…`
    // depends entirely on the version arm.
    assert!(is_cargo_builtin_verb(&crate_name));
}

#[test]
fn cargo_builtin_shorthand_includes_borderline_install_verb() {
    // `install` is the borderline case: `soldr install-zccache`
    // already exists as a soldr built-in (different name, different
    // verb). Bare `soldr install <crate>` is documented to route to
    // `cargo install <crate>` because that's the far more common
    // interpretation. The const-level expectation is asserted here.
    assert!(
        is_cargo_builtin_verb("install"),
        "bare `soldr install` must route to `cargo install` (zccache install is install-zccache)"
    );
    // Sanity: the clap built-in keeps its long name and is NOT
    // remapped.
    assert!(SOLDR_BUILTIN_VERBS.contains(&"install-zccache"));
    assert!(!is_cargo_builtin_verb("install-zccache"));
}

#[test]
fn cargo_builtin_shorthand_does_not_capture_other_verbs() {
    // Negative coverage. None of:
    //   - phase-1 known cargo subcommands (handled by their own hop)
    //   - top-level fetch tools (cross, mdbook, ...)
    //   - made-up names
    //   - soldr-native verbs
    // should be captured as cargo built-ins.
    for verb in [
        "nextest",  // phase-1 known cargo subcommand
        "zigbuild", // phase-1 known cargo subcommand
        "cross",    // top-level fetch tool
        "mdbook",   // top-level fetch tool
        "bacon",    // top-level fetch tool
        "completely-made-up-name",
        "clean",   // soldr-native
        "config",  // soldr-native
        "version", // soldr-native
        "doctor",  // soldr-native
    ] {
        assert!(
            !is_cargo_builtin_verb(verb),
            "verb {verb:?} must NOT be classified as a cargo built-in"
        );
    }
}
