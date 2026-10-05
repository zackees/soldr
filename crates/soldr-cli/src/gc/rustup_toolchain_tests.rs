//! Unit coverage split from `rustup_toolchain.rs` for the soldr#2493
//! 1,000-line production-source ceiling.

use super::*;
use super::*;

fn names(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// -------------------------------------------------------------------
// Triple normalization (rule 2's foundation).
// -------------------------------------------------------------------

#[test]
fn normalize_strips_a_target_triple_suffix() {
    for (input, expected) in [
        ("stable-x86_64-unknown-linux-gnu", "stable"),
        ("1.98.1-x86_64-unknown-linux-gnu", "1.98.1"),
        (
            "nightly-2026-05-28-x86_64-unknown-linux-gnu",
            "nightly-2026-05-28",
        ),
        (
            "nightly-2026-05-28-aarch64-apple-darwin",
            "nightly-2026-05-28",
        ),
        ("stable-aarch64-apple-darwin", "stable"),
    ] {
        assert_eq!(normalize_toolchain_name(input), expected, "{input}");
    }
}

#[test]
fn normalize_keeps_names_that_are_not_prefix_plus_triple() {
    for input in [
        "stable",
        "nightly",
        "1.98.1",
        "nightly-2026-05-28",
        "my-custom-link",
        // A triple alone has no channel in front of it; keep it whole
        // so an exact settings match still compares equal.
        "aarch64-apple-darwin",
        // Fewer than vendor+os segments after the arch-like token is
        // not treated as a triple.
        "proj-x86_64-linux",
    ] {
        assert_eq!(normalize_toolchain_name(input), input, "{input}");
    }
}

// -------------------------------------------------------------------
// Protection matching (rails (a) + (b) at the name level).
// -------------------------------------------------------------------

#[test]
fn protection_matches_exact_normalized_and_version_prefix_forms() {
    // Exact.
    assert!(toolchain_name_is_protected(
        "stable-x86_64-unknown-linux-gnu",
        "stable-x86_64-unknown-linux-gnu"
    ));
    // Pin without triple covers the installed directory.
    assert!(toolchain_name_is_protected(
        "1.98.1-x86_64-unknown-linux-gnu",
        "1.98.1"
    ));
    assert!(toolchain_name_is_protected(
        "nightly-2026-05-28-x86_64-unknown-linux-gnu",
        "nightly-2026-05-28"
    ));
    // Minor-channel pin covers the resolved full version directory.
    assert!(toolchain_name_is_protected(
        "1.95.0-x86_64-unknown-linux-gnu",
        "1.95"
    ));
    // ...and conservatively its patch siblings too (documented
    // one-directional rule: over-protection, never deletion).
    assert!(toolchain_name_is_protected(
        "1.95.1-x86_64-unknown-linux-gnu",
        "1.95"
    ));
    // A full pin does not cover a different version.
    assert!(!toolchain_name_is_protected(
        "1.94.1-x86_64-unknown-linux-gnu",
        "1.98.1"
    ));
    // "1.98" must not look like a prefix of "1.981"-style segment
    // drift: segment-wise comparison only.
    assert!(!toolchain_name_is_protected(
        "1.981-x86_64-unknown-linux-gnu",
        "1.98"
    ));
    // Bare `nightly` never covers a dated nightly — this is what
    // keeps #3507's six dated nightlies eligible.
    assert!(!toolchain_name_is_protected(
        "nightly-2026-05-28-x86_64-unknown-linux-gnu",
        "nightly"
    ));
    assert!(toolchain_name_is_protected(
        "nightly-x86_64-unknown-linux-gnu",
        "nightly"
    ));
    // Custom linked toolchains match by exact name only.
    assert!(toolchain_name_is_protected("my-link", "my-link"));
    assert!(!toolchain_name_is_protected("my-link", "other"));
}

// -------------------------------------------------------------------
// Candidate selection — the core of the regression suite.
// -------------------------------------------------------------------

#[test]
fn protected_toolchains_are_excluded_from_the_purge_candidates() {
    let installed = names(&[
        "1.70-x86_64-unknown-linux-gnu",
        "1.94.1-x86_64-unknown-linux-gnu",
        "nightly-2026-02-28-x86_64-unknown-linux-gnu",
        "stable-x86_64-unknown-linux-gnu",
    ]);
    let protected = names(&["1.94.1", "stable"]);
    assert_eq!(
        select_rustup_toolchain_candidates(&installed, &protected),
        names(&[
            "1.70-x86_64-unknown-linux-gnu",
            "nightly-2026-02-28-x86_64-unknown-linux-gnu",
        ]),
        "the repo pin and the active default must be excluded"
    );
}

#[test]
fn nothing_protected_selects_every_installed_toolchain() {
    let installed = names(&[
        "stable-x86_64-unknown-linux-gnu",
        "nightly-x86_64-unknown-linux-gnu",
    ]);
    assert_eq!(
        select_rustup_toolchain_candidates(&installed, &[]),
        installed
    );
}

#[test]
fn no_installed_toolchains_is_a_clean_empty_candidate_list() {
    assert!(select_rustup_toolchain_candidates(&[], &names(&["stable"])).is_empty());
    assert!(select_rustup_toolchain_candidates(&[], &[]).is_empty());
}

// -------------------------------------------------------------------
// plan_home — rails composed over settings + shared protections.
// -------------------------------------------------------------------

#[test]
fn plan_protects_default_pin_and_rustup_overrides() {
    let settings = HomeSettings {
        default: Some("stable-x86_64-unknown-linux-gnu".into()),
        overrides: vec!["nightly-2026-02-28-x86_64-unknown-linux-gnu".into()],
        unreadable: false,
    };
    let installed = names(&[
        "1.70-x86_64-unknown-linux-gnu",
        "nightly-2026-02-28-x86_64-unknown-linux-gnu",
        "stable-x86_64-unknown-linux-gnu",
    ]);
    let plan = plan_home(&installed, &settings, &names(&["1.98.1"]));
    assert_eq!(
        plan.protected,
        names(&[
            "1.98.1",
            "nightly-2026-02-28-x86_64-unknown-linux-gnu",
            "stable-x86_64-unknown-linux-gnu",
        ])
    );
    assert_eq!(plan.candidates, names(&["1.70-x86_64-unknown-linux-gnu"]));
}

#[test]
fn plan_fails_closed_when_settings_are_unreadable() {
    let settings = HomeSettings {
        unreadable: true,
        ..HomeSettings::default()
    };
    let installed = names(&[
        "1.70-x86_64-unknown-linux-gnu",
        "stable-x86_64-unknown-linux-gnu",
    ]);
    let plan = plan_home(&installed, &settings, &names(&["1.98.1"]));
    assert!(
        plan.candidates.is_empty(),
        "an unreadable settings.toml must protect the whole home"
    );
    assert_eq!(plan.protected, installed);
}

#[test]
fn plan_on_a_default_less_managed_home_protects_only_the_shared_pins() {
    let installed = names(&[
        "1.85.0-x86_64-unknown-linux-gnu",
        "nightly-2026-05-28-x86_64-unknown-linux-gnu",
    ]);
    let plan = plan_home(&installed, &HomeSettings::default(), &names(&["1.98.1"]));
    assert_eq!(plan.protected, names(&["1.98.1"]));
    assert_eq!(
        plan.candidates,
        names(&[
            "1.85.0-x86_64-unknown-linux-gnu",
            "nightly-2026-05-28-x86_64-unknown-linux-gnu",
        ])
    );
}

// -------------------------------------------------------------------
// settings.toml parsing — the fail-closed rail's input.
// -------------------------------------------------------------------

#[test]
fn home_settings_reads_default_and_overrides() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings = dir.path().join("settings.toml");
    std::fs::write(
        &settings,
        "default_toolchain = \"stable-x86_64-unknown-linux-gnu\"\n\
         [overrides]\n\
         \"/home/me/proj\" = \"nightly-2026-05-28-x86_64-unknown-linux-gnu\"\n",
    )
    .expect("write settings");
    let parsed = read_home_settings(&settings);
    assert!(!parsed.unreadable);
    assert_eq!(
        parsed.default.as_deref(),
        Some("stable-x86_64-unknown-linux-gnu")
    );
    assert_eq!(
        parsed.overrides,
        names(&["nightly-2026-05-28-x86_64-unknown-linux-gnu"])
    );
}

#[test]
fn home_settings_absent_file_is_not_unreadable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let parsed = read_home_settings(&dir.path().join("settings.toml"));
    assert_eq!(parsed, HomeSettings::default());
}

#[test]
fn home_settings_malformed_file_fails_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let settings = dir.path().join("settings.toml");
    for body in [
        "default_toolchain = ",                                   // not TOML
        "just a bare string",                                     // not a table
        "default_toolchain = \"\"",                               // empty default
        "default_toolchain = 42",                                 // wrong type
        "default_toolchain = \"stable\"\n[overrides]\nweird = 1", // bad override
        "default_toolchain = \"stable\"\noverrides = 7",          // wrong-typed table
    ] {
        std::fs::write(&settings, body).expect("write settings");
        let parsed = read_home_settings(&settings);
        assert!(parsed.unreadable, "must fail closed for {body:?}");
    }
}

#[test]
fn installed_toolchain_names_sorts_and_skips_symlinks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("toolchains");
    std::fs::create_dir_all(root.join("stable-x86_64-unknown-linux-gnu")).expect("mkdir");
    std::fs::create_dir_all(root.join("1.70-x86_64-unknown-linux-gnu")).expect("mkdir");
    // Created through the platform crate behind a runtime host gate,
    // not `#[cfg(unix)]`/`std::os::unix` — the #2493 boundary (and on
    // Windows symlink creation needs privileges the target-run lanes
    // may not grant; the assertion below holds either way, since an
    // absent custom-link is skipped exactly like a symlinked one).
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        soldr_platform::fs::links::create(
            root.join("1.70-x86_64-unknown-linux-gnu")
                .to_string_lossy()
                .as_ref(),
            &root.join("custom-link"),
            false,
        )
        .expect("symlink");
    }
    assert_eq!(
        installed_toolchain_names(&root),
        names(&[
            "1.70-x86_64-unknown-linux-gnu",
            "stable-x86_64-unknown-linux-gnu"
        ]),
        "sorted, and the symlinked custom link never becomes a candidate"
    );
    // Missing directory: empty, not an error.
    assert!(installed_toolchain_names(&dir.path().join("nope")).is_empty());
}
