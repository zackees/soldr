//! Integration tests for `soldr gc purge --kind rustup_toolchain`
//! (soldr#3507) — the real deletion path that delegates to
//! `rustup toolchain uninstall`, plus its safety rails.
//!
//! Every fixture sandboxes `SOLDR_CACHE_DIR`, `RUSTUP_HOME` and
//! `CARGO_HOME`, points `SOLDR_TEST_RUSTUP_BIN` at a logging fake, and
//! pins the child's cwd at a workspace whose `rust-toolchain.toml`
//! declares the pin — so no real home and no real rustup is ever
//! involved, and no test can actually uninstall anything on the host.

#![allow(unused_imports)]

use crate::common;
use crate::common::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The channel the fixture's `rust-toolchain.toml` pins; directories
/// named after it must never become purge candidates.
const PIN: &str = "1.94.1";

const STABLE_DIR: &str = "stable-x86_64-unknown-linux-gnu";
const PIN_DIR: &str = "1.94.1-x86_64-unknown-linux-gnu";
const OLD_DIR: &str = "1.70-x86_64-unknown-linux-gnu";
const DATED_NIGHTLY_DIR: &str = "nightly-2026-02-28-x86_64-unknown-linux-gnu";
const MANAGED_DIR: &str = "1.85.0-x86_64-unknown-linux-gnu";

struct Fixture {
    cache_root: PathBuf,
    cargo_home: PathBuf,
    rustup_home: PathBuf,
    pin_ws: PathBuf,
    fake_rustup_log: PathBuf,
    fake_rustup: PathBuf,
    /// Toolchain directories seeded, for post-run existence checks.
    seeded_dirs: Vec<PathBuf>,
}

fn seed_toolchain_dir(rustup_home: &Path, name: &str) -> PathBuf {
    let dir = rustup_home.join("toolchains").join(name);
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).expect("failed to create fake toolchain dir");
    fs::write(
        // Host-agnostic: EXE_SUFFIX (""/".exe"), not `cfg!(windows)` —
        // host cfg outside soldr-platform is denied by the #2493 boundary.
        bin.join(format!("rustc{}", std::env::consts::EXE_SUFFIX)),
        format!("fake rustc for {name}"),
    )
    .expect("failed to seed fake rustc");
    dir
}

/// Build the sandbox: a caller rustup home, an optional managed home
/// under `SOLDR_CACHE_DIR`, a pin workspace whose `rust-toolchain.toml`
/// declares [`PIN`], and a logging fake rustup.
fn seed_fixture(
    label: &str,
    caller_toolchains: &[&str],
    default_toolchain: Option<&str>,
    managed_toolchains: &[&str],
) -> Fixture {
    let cache_root = unique_temp_dir(&format!("{label}-cache"));
    let cargo_home = unique_temp_dir(&format!("{label}-cargo-home"));
    let rustup_home = unique_temp_dir(&format!("{label}-rustup-home"));
    let pin_ws = unique_temp_dir(&format!("{label}-pin-ws"));
    let fake_rustup_log = cache_root.join("fake-rustup.log");

    let mut seeded_dirs = Vec::new();
    for name in caller_toolchains {
        seeded_dirs.push(seed_toolchain_dir(&rustup_home, name));
    }
    if let Some(default) = default_toolchain {
        fs::write(
            rustup_home.join("settings.toml"),
            format!("default_toolchain = \"{default}\"\n"),
        )
        .expect("failed to write rustup settings.toml");
    }
    if !managed_toolchains.is_empty() {
        let managed_home = cache_root.join("rustup");
        for name in managed_toolchains {
            seeded_dirs.push(seed_toolchain_dir(&managed_home, name));
        }
    }

    common::seed_rust_toolchain_toml(&pin_ws, &format!("[toolchain]\nchannel = \"{PIN}\"\n"));
    let fake_rustup = common::install_logging_fake_rustup(&fake_rustup_log);

    Fixture {
        cache_root,
        cargo_home,
        rustup_home,
        pin_ws,
        fake_rustup_log,
        fake_rustup,
        seeded_dirs,
    }
}

fn purge_command(fixture: &Fixture, args: &[&str]) -> Command {
    let mut command = common::isolated_soldr_command();
    command
        .args(["gc", "purge", "--kind", "rustup_toolchain"])
        .args(args)
        .env("SOLDR_CACHE_DIR", &fixture.cache_root)
        .env("CARGO_HOME", &fixture.cargo_home)
        .env("RUSTUP_HOME", &fixture.rustup_home)
        .env("SOLDR_TEST_RUSTUP_BIN", &fixture.fake_rustup)
        .env_remove("RUSTUP_TOOLCHAIN")
        // The pin must come from this fixture's rust-toolchain.toml, not
        // from whatever repo the test process happens to run in.
        .current_dir(&fixture.pin_ws);
    command
}

fn run_json(mut command: Command) -> Value {
    let output = command.output().expect("failed to run soldr gc purge");
    assert!(
        output.status.success(),
        "gc purge failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("gc purge must emit JSON")
}

fn home_by_origin<'a>(json: &'a Value, origin: &str) -> &'a Value {
    json["homes"]
        .as_array()
        .expect("homes array")
        .iter()
        .find(|home| home["origin"].as_str() == Some(origin))
        .unwrap_or_else(|| panic!("no {origin} home in {}", json["homes"]))
}

fn candidate_names(home: &Value) -> Vec<String> {
    home["candidates"]
        .as_array()
        .expect("candidates array")
        .iter()
        .map(|c| c["toolchain"].as_str().expect("toolchain name").to_string())
        .collect()
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("string array")
        .iter()
        .map(|v| v.as_str().expect("string").to_string())
        .collect()
}

#[test]
fn gc_purge_rustup_dry_run_reports_candidates_and_deletes_nothing() {
    let fixture = seed_fixture(
        "gc-rustup-dry-run",
        &[STABLE_DIR, PIN_DIR, OLD_DIR, DATED_NIGHTLY_DIR],
        Some(STABLE_DIR),
        &[MANAGED_DIR],
    );

    // The fake rustup stays wired even for a dry-run: if the command
    // ever regresses into spawning rustup here, the log assertion below
    // fails instead of the host's real rustup being reached.
    let json = run_json(purge_command(&fixture, &["--all", "--dry-run", "--json"]));

    assert_eq!(json["schema_version"], 3);
    assert_eq!(json["command"], "gc");
    assert_eq!(json["mode"], "purge");
    assert_eq!(json["kind"], "rustup_toolchain");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["uninstalled_count"], 0);
    assert_eq!(json["failed_count"], 0);
    assert_eq!(json["reclaimed_bytes"], 0);

    let caller = home_by_origin(&json, "caller");
    assert_eq!(
        Path::new(caller["rustup_home"].as_str().expect("path")),
        fixture.rustup_home
    );
    assert_eq!(caller["installed_count"], 4);
    assert_eq!(
        caller["default_toolchain"].as_str(),
        Some(STABLE_DIR),
        "the home's rustup default must be reported"
    );
    assert_eq!(caller["settings_unreadable"], false);
    assert_eq!(
        candidate_names(caller),
        vec![OLD_DIR.to_string(), DATED_NIGHTLY_DIR.to_string()],
        "the default and the repo pin must be excluded; everything else is eligible"
    );
    let protected = string_list(&caller["protected"]);
    assert!(
        protected.contains(&STABLE_DIR.to_string()),
        "default missing from protected: {protected:?}"
    );
    assert!(
        protected.contains(&PIN.to_string()),
        "repo pin missing from protected: {protected:?}"
    );
    assert_eq!(
        string_list(&caller["uninstalled"]),
        Vec::<String>::new(),
        "a dry-run must not uninstall anything"
    );

    let managed = home_by_origin(&json, "managed");
    assert_eq!(
        Path::new(managed["rustup_home"].as_str().expect("path")),
        fixture.cache_root.join("rustup"),
        "the managed home (the 20 GB of #3507) must be enumerated and named"
    );
    assert_eq!(candidate_names(managed), vec![MANAGED_DIR.to_string()]);

    assert_eq!(json["selected_count"], 3);
    assert_eq!(
        json["uninstalled_count"], 0,
        "dry-run reports selections but never uninstalls"
    );

    for dir in &fixture.seeded_dirs {
        assert!(dir.exists(), "dry-run must not delete {}", dir.display());
    }
    assert!(
        !fixture.fake_rustup_log.exists(),
        "a dry-run must never invoke rustup"
    );
}

#[test]
fn gc_purge_rustup_all_uninstalls_via_rustup_and_keeps_protected_pins() {
    let fixture = seed_fixture(
        "gc-rustup-all",
        &[STABLE_DIR, PIN_DIR, OLD_DIR, DATED_NIGHTLY_DIR],
        Some(STABLE_DIR),
        &[MANAGED_DIR],
    );

    let json = run_json(purge_command(&fixture, &["--all", "--json"]));

    assert_eq!(json["dry_run"], false);
    assert_eq!(json["selected_count"], 3);
    assert_eq!(json["uninstalled_count"], 3);
    assert_eq!(json["failed_count"], 0);

    let caller = home_by_origin(&json, "caller");
    assert_eq!(
        string_list(&caller["uninstalled"]),
        vec![OLD_DIR.to_string(), DATED_NIGHTLY_DIR.to_string()]
    );
    let managed = home_by_origin(&json, "managed");
    assert_eq!(
        string_list(&managed["uninstalled"]),
        vec![MANAGED_DIR.to_string()],
        "the managed home's toolchains must be purged through its own home"
    );

    // Delegation proof: exactly one `rustup toolchain uninstall` per
    // selection, and the protected pins are never named.
    let invocations = common::read_logged_rustup_invocations(&fixture.fake_rustup_log);
    let mut uninstalled_via_rustup: Vec<String> = Vec::new();
    for argv in &invocations {
        assert_eq!(
            &argv[..2],
            ["toolchain", "uninstall"],
            "soldr must delegate deletion as `rustup toolchain uninstall <name>`, got {argv:?}"
        );
        uninstalled_via_rustup.push(argv[2].clone());
    }
    uninstalled_via_rustup.sort();
    let mut expected = vec![
        OLD_DIR.to_string(),
        DATED_NIGHTLY_DIR.to_string(),
        MANAGED_DIR.to_string(),
    ];
    expected.sort();
    assert_eq!(uninstalled_via_rustup, expected);
    for protected in [STABLE_DIR, PIN_DIR] {
        assert!(
            !uninstalled_via_rustup.iter().any(|name| name == protected),
            "{protected} is protected but rustup was asked to uninstall it"
        );
    }

    // The fake rustup exits 0 without touching the directories, so their
    // survival here proves soldr itself never hand-deleted a toolchain —
    // rustup owns the bytes (soldr#3507's delegation contract).
    for dir in &fixture.seeded_dirs {
        assert!(
            dir.exists(),
            "{} must only ever be removed by rustup, not by soldr",
            dir.display()
        );
    }
}

#[test]
fn gc_purge_rustup_with_nothing_eligible_is_a_clean_no_op() {
    // Only the protected default is installed, and there is no managed
    // home at all: nothing is eligible anywhere.
    let fixture = seed_fixture("gc-rustup-noop", &[STABLE_DIR], Some(STABLE_DIR), &[]);

    let mut command = purge_command(&fixture, &["--json"]);
    let output = command.output().expect("failed to run soldr gc purge");
    assert!(
        output.status.success(),
        "purge with nothing eligible must be a clean no-op\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("JSON payload");
    assert_eq!(json["selected_count"], 0);
    assert_eq!(json["uninstalled_count"], 0);
    assert_eq!(json["failed_count"], 0);
    let caller = home_by_origin(&json, "caller");
    assert_eq!(candidate_names(caller), Vec::<String>::new());
    assert_eq!(json["homes"].as_array().expect("homes").len(), 1);

    assert!(fixture.seeded_dirs[0].exists());
    assert!(
        !fixture.fake_rustup_log.exists(),
        "a no-op must not spawn rustup at all"
    );
}

#[test]
fn gc_purge_rustup_closed_stdin_never_deletes() {
    // Candidates exist, but there is no `--all` and stdin is closed
    // (Command::output() wires stdin to the null device). The prompt
    // must answer no rather than inherit the other gc prompts'
    // default-yes semantics — a GB-scale deletion is opt-in only.
    let fixture = seed_fixture(
        "gc-rustup-eof",
        &[STABLE_DIR, OLD_DIR, DATED_NIGHTLY_DIR],
        Some(STABLE_DIR),
        &[],
    );

    let json = run_json(purge_command(&fixture, &["--json"]));

    assert_eq!(json["selected_count"], 0, "no prompt was answered yes");
    assert_eq!(json["uninstalled_count"], 0);
    assert_eq!(json["failed_count"], 0);
    let caller = home_by_origin(&json, "caller");
    assert_eq!(
        candidate_names(caller),
        vec![OLD_DIR.to_string(), DATED_NIGHTLY_DIR.to_string()],
        "candidates are still reported even though none were selected"
    );
    for dir in &fixture.seeded_dirs {
        assert!(
            dir.exists(),
            "closed stdin must not delete {}",
            dir.display()
        );
    }
    assert!(!fixture.fake_rustup_log.exists());
}

#[test]
fn gc_purge_rustup_human_mode_reports_scan_and_no_op_summary() {
    let fixture = seed_fixture("gc-rustup-human", &[STABLE_DIR], Some(STABLE_DIR), &[]);

    let mut command = purge_command(&fixture, &[]);
    let output = command.output().expect("failed to run soldr gc purge");
    assert!(
        output.status.success(),
        "gc purge failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("(caller)"),
        "the report must say which home it scanned: {stderr}"
    );
    assert!(
        stderr.contains("protected"),
        "the report must name the protection it applied: {stderr}"
    );
    assert!(
        stderr.contains("selected 0; uninstalled 0; failed 0"),
        "summary line missing: {stderr}"
    );
    assert!(output.stdout.is_empty(), "human mode writes no JSON");
}

#[test]
fn gc_purge_dry_run_is_rejected_for_every_non_rustup_kind() {
    // The flag must be loud about its scope instead of silently doing
    // nothing for kinds it does not support.
    for args in [
        vec!["--kind", "cargo_target_incremental", "--dry-run", "--all"],
        vec!["--kind", "cargo_registry_src", "--dry-run", "--all"],
        vec!["--dry-run", "--all"],
    ] {
        let fixture = seed_fixture("gc-rustup-dryrun-reject", &[], None, &[]);
        let output = Command::new(common::soldr_bin())
            .args(["gc", "purge"])
            .args(&args)
            .env("SOLDR_CACHE_DIR", &fixture.cache_root)
            .env("CARGO_HOME", &fixture.cargo_home)
            .env("RUSTUP_HOME", &fixture.rustup_home)
            .current_dir(&fixture.pin_ws)
            .output()
            .expect("failed to run soldr gc purge");
        assert!(
            !output.status.success(),
            "gc purge {args:?} must be rejected"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("only supported with `--kind rustup_toolchain`"),
            "stderr must explain the flag's scope for {args:?}: {stderr}"
        );
    }
}
