//! The probe facade must not drag injection machinery into the main crate (#633).
//!
//! `running-process` is depended on by consumers who never opt into the probe
//! tier. The #539 contract is that static AV/EDR analysis of those consumers
//! finds no hooking surface — no `CreateRemoteThread`, no `dlopen` of an
//! interposer. The facade depends on `running-process-probe` only for the
//! `probe_diag.v1` schema, with that crate's default features off so its
//! injection vehicles (gated on `embed-helper`) are never compiled.
//!
//! These tests assert that rather than trusting the feature flags to stay
//! arranged correctly. They are the "feature-off symbol test" from the issue's
//! acceptance criteria, expressed as a source/manifest contract so they run on
//! every platform instead of only where a symbol dumper exists.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    // tests/ -> crates/running-process -> crates -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn cargo() -> Command {
    Command::new(env!("CARGO"))
}

/// The facade must never reference the probe *daemon* crate. It talks to the
/// daemon over IPC; a code dependency would invert that and pull the daemon's
/// surface into every consumer.
#[test]
fn facade_does_not_depend_on_the_probe_daemon_crate() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("read manifest");

    assert!(
        !manifest.contains("running-process-probe-daemon"),
        "the main crate must not depend on the probe daemon crate — it talks \
         to it over IPC"
    );

    let probe_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/probe");
    for entry in std::fs::read_dir(&probe_dir).expect("read src/probe") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read source");
        assert!(
            !src.contains("running_process_probe_daemon"),
            "{} references the probe daemon crate",
            path.display()
        );
    }
}

/// The schema dependency must stay `default-features = false`.
///
/// This is the single line that keeps `embed-helper` — and therefore
/// `inject_into_pid` / `inject_via_env` — out of this crate. If it is ever
/// dropped, the injection modules compile into every consumer that enables
/// `probe`.
#[test]
fn schema_dependency_disables_default_features() {
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("read manifest");

    let line = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("running-process-probe ="))
        .expect("running-process-probe dependency line");

    assert!(
        line.contains("default-features = false"),
        "the probe schema dependency must disable default features so the \
         interposer/injection machinery stays out of this crate; got: {line}"
    );
    assert!(
        !line.contains("embed-helper"),
        "the main crate must never enable embed-helper; got: {line}"
    );
}

/// With `probe` enabled, the schema crate must be resolved with **no features**.
///
/// This is the assertion with real discriminating power. `embed-helper` is
/// what compiles `inject_into_pid` / `inject_via_env`; feature unification is
/// what would silently switch it on, because any *other* crate in the graph
/// enabling it turns it on here too. Checking the resolved feature set catches
/// that, where checking for a package name would not.
///
/// (Checking for `retour` or the interposer crates would be vacuous: nothing
/// in this graph depends on them, so those assertions would pass no matter
/// what `embed-helper` did.)
#[test]
fn probe_schema_dependency_resolves_with_no_features() {
    let output = cargo()
        .current_dir(repo_root())
        .args([
            "tree",
            "-p",
            "running-process",
            "--features",
            "probe",
            "--no-default-features",
            "--edges",
            "normal",
            "-f",
            "{p} :: {f}",
        ])
        .output()
        .expect("cargo tree");

    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let tree = String::from_utf8_lossy(&output.stdout);
    let line = tree
        .lines()
        .find(|l| l.contains("running-process-probe v"))
        .unwrap_or_else(|| panic!("schema crate absent from the tree:\n{tree}"));

    let features = line.rsplit("::").next().unwrap_or("").trim();
    assert!(
        features.is_empty(),
        "running-process-probe resolved with features `{features}`; the main \
         crate must pull it in featureless so the injection vehicles behind \
         `embed-helper` are never compiled here"
    );

    // The daemon crate is a separate, absolute prohibition.
    assert!(
        !tree.contains("running-process-probe-daemon"),
        "the probe daemon crate must never appear in the main crate's graph"
    );
}

/// Nothing in the facade may register a global constructor.
///
/// Registration must happen only when the application calls `probe::install`.
/// A `ctor`/`.init_array` hook would make merely linking the crate enroll the
/// process, which is exactly the surprise the design avoids.
#[test]
fn facade_has_no_global_constructors() {
    let probe_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/probe");
    for entry in std::fs::read_dir(&probe_dir).expect("read src/probe") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("read source");

        // Scan code only. The module docs *describe* the absence of these
        // constructs, so a whole-file grep matches its own documentation.
        let code: String = src
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("/*") && !t.starts_with('*')
            })
            .collect::<Vec<_>>()
            .join("\n");

        for forbidden in ["#[ctor]", "ctor::ctor", ".init_array", "lazy_static!"] {
            assert!(
                !code.contains(forbidden),
                "{} contains `{forbidden}` in code; registration must happen \
                 only via an explicit probe::install() call",
                path.display()
            );
        }
    }
}
