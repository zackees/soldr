//! Integration-ish tests for `soldr install` (soldr#2310, Phase 1):
//! clap flag mutual-exclusion, dry-run planning, and a local-path
//! end-to-end install into a synthetic `SoldrPaths`.

use std::time::Duration;

use crate::cli_args::Cli;
use crate::core::SoldrPaths;
use clap::Parser;

use super::InstallArgs;

fn try_parse(argv: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(argv)
}

#[test]
fn install_release_and_tag_are_mutually_exclusive() {
    // `--release` (install a Release) and `--tag` (build SOURCE at a git
    // tag) are contradictory; clap must reject the combination.
    let err = try_parse(&[
        "soldr",
        "install",
        "https://github.com/o/r",
        "--release",
        "--tag",
        "v1",
    ]);
    assert!(err.is_err(), "--release + --tag must be a parse error");
}

#[test]
fn install_head_branch_rev_tag_conflict() {
    // Any two raw source-ref flags conflict via the `source_ref` group.
    for pair in [
        ["--head", "--branch"],
        ["--branch", "--rev"],
        ["--head", "--rev"],
    ] {
        // Provide values where the flag needs one.
        let mut argv = vec!["soldr", "install", "https://github.com/o/r"];
        for flag in pair {
            argv.push(flag);
            if flag != "--head" {
                argv.push("x");
            }
        }
        assert!(
            try_parse(&argv).is_err(),
            "conflicting source refs {pair:?} must be a parse error"
        );
    }
}

#[test]
fn install_prebuilt_and_build_conflict() {
    let err = try_parse(&[
        "soldr",
        "install",
        "https://github.com/o/r",
        "--prebuilt",
        "--build",
    ]);
    assert!(err.is_err(), "--prebuilt + --build must be a parse error");
}

#[test]
fn install_bare_release_parses_as_latest() {
    // Bare `--release` must parse (default_missing_value = "").
    let cli = try_parse(&["soldr", "install", "https://github.com/o/r", "--release"])
        .expect("bare --release must parse");
    match cli.command {
        crate::cli_args::Commands::Install(args) => {
            assert_eq!(args.release.as_deref(), Some(""));
        }
        _ => panic!("expected Commands::Install"),
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime")
}

fn synthetic_paths(tmp: &std::path::Path) -> SoldrPaths {
    let root = tmp.join("soldr-home");
    std::fs::create_dir_all(&root).unwrap();
    let paths = SoldrPaths::with_root(root);
    paths.ensure_dirs().unwrap();
    paths
}

fn local_args(target: &str) -> InstallArgs {
    InstallArgs {
        target: target.to_string(),
        release: None,
        head: false,
        branch: None,
        tag: None,
        rev: None,
        prebuilt: false,
        build: false,
        debug: false,
        bins: vec![],
        features: vec![],
        target_triple: None,
        root: None,
        force: false,
        dry_run: false,
        locked: false,
    }
}

fn write_fixture_crate(dir: &std::path::Path, name: &str) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[[bin]]\nname = \"{name}\"\npath = \"src/main.rs\"\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.rs"),
        "fn main() { println!(\"hello from fixture\"); }\n",
    )
    .unwrap();
}

crate::timed_test!(install_dry_run_prints_plan_and_does_not_fetch, {
    // A local-path dry-run resolves with no network and produces no
    // installed binary.
    let tmp = tempfile::tempdir().unwrap();
    let paths = synthetic_paths(tmp.path());
    let crate_dir = tmp.path().join("fixture");
    write_fixture_crate(&crate_dir, "soldr-install-fixture-dry");

    let mut args = local_args(crate_dir.to_str().unwrap());
    args.dry_run = true;

    runtime()
        .block_on(super::run_with_paths(args, &paths))
        .expect("dry-run must succeed");

    // Nothing was installed.
    let installed = paths.bin.join("installed");
    assert!(
        !installed.join("soldr-install-fixture-dry").exists(),
        "dry-run must not place a binary"
    );
});

crate::timed_test!(install_local_dot_end_to_end, Duration::from_secs(300), {
    // Build a tiny fixture crate and install it from a local path into
    // a synthetic SoldrPaths; assert a runnable binary lands under
    // bin/installed/<name>/.
    let tmp = tempfile::tempdir().unwrap();
    let paths = synthetic_paths(tmp.path());
    let name = "soldrinstallfixture";
    let crate_dir = tmp.path().join("fixture");
    write_fixture_crate(&crate_dir, name);

    let args = local_args(crate_dir.to_str().unwrap());
    runtime()
        .block_on(super::run_with_paths(args, &paths))
        .expect("local install must succeed");

    let host = crate::core::TargetTriple::detect().unwrap().triple();
    let ext = super::place::binary_ext_for_triple(&host);
    let expected = paths
        .bin
        .join("installed")
        .join(name)
        .join(format!("{name}{ext}"));
    assert!(
        expected.is_file(),
        "expected installed binary at {}",
        expected.display()
    );
});
