//! CLAUDE.md's MSRV must match `[workspace.package].rust-version`.
//!
//! The two drifted for roughly two and a half months: soldr#298 moved
//! `rust-version` from `1.75` to `1.94.1` on 2026-05-14 and CLAUDE.md kept
//! saying `MSRV 1.75`.
//!
//! That is worse than an ordinary stale comment because CLAUDE.md is loaded
//! into every agent session, so the wrong number is read constantly and by
//! everyone. It also changes decisions rather than merely describing them: a
//! believed-1.75 floor is a reason to avoid a newer std API, or to suspect
//! version-dependent behaviour in a bug report, neither of which applies when
//! the MSRV equals the pinned toolchain. It was doing exactly that during the
//! soldr#2199 / soldr#2200 investigation.
//!
//! `version_lockstep.rs` already guards the three places the *package*
//! version appears; this is the same idea for the compiler floor.

use crate::common;

/// `rust-version = "X"` from `[workspace.package]`.
fn manifest_rust_version(manifest: &str) -> String {
    manifest
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("rust-version"))
        .and_then(|rest| rest.split('"').nth(1))
        .expect("[workspace.package].rust-version must be declared")
        .to_string()
}

/// Every `MSRV <version>` CLAUDE.md states.
fn documented_msrvs(doc: &str) -> Vec<String> {
    doc.split("MSRV ")
        .skip(1)
        .filter_map(|rest| {
            let token: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            // "MSRV and the pinned toolchain" and similar prose carry no
            // version; only a real number is a claim to check.
            (!token.is_empty()).then(|| token.trim_end_matches('.').to_string())
        })
        .collect()
}

#[test]
fn claude_md_msrv_matches_the_workspace_manifest() {
    // `common::workspace_root()` is the sanctioned resolver. These tests also
    // run replayed from a nextest archive with no checkout beside the binary,
    // so compile-time manifest-dir resolution is banned in `crates/**` outside
    // three allowlisted files -- enforced by
    // tests/test_cross_compile_workflows.py, which substring-matches file
    // bodies and so does not exempt comments.
    let root = common::workspace_root();
    let manifest =
        std::fs::read_to_string(root.join("Cargo.toml")).expect("read workspace Cargo.toml");
    let doc = std::fs::read_to_string(root.join("CLAUDE.md")).expect("read CLAUDE.md");

    let declared = manifest_rust_version(&manifest);
    let documented = documented_msrvs(&doc);

    assert!(
        !documented.is_empty(),
        "CLAUDE.md no longer states an MSRV; if that is deliberate, delete this test \
         rather than leaving it passing vacuously",
    );
    for stated in &documented {
        assert_eq!(
            stated, &declared,
            "CLAUDE.md says MSRV {stated}, Cargo.toml declares rust-version {declared}. \
             CLAUDE.md is loaded into every session, so a stale floor is read constantly \
             (soldr#298 left it wrong for ~2.5 months).",
        );
    }
}

/// soldr#3305: the MSRV names a compiler *train*, never a patch release.
///
/// `soldr ci-test` compiles this workspace with two compilers: the stable pin
/// from `rust-toolchain.toml`, and the dated nightly that `dylints/*` declare.
/// That nightly reports a `.0` release of its train (`nightly-2026-05-28` is
/// `1.98.0-nightly`), so a patch-level `rust-version` of `1.98.1` makes Cargo
/// refuse to build anything under it:
///
/// ```text
/// error: rustc 1.98.0-nightly is not supported by the following packages:
/// ```
///
/// Cargo reads a two-component `rust-version` as `>=1.98.0`, which both
/// compilers satisfy. The coupling is otherwise invisible — the nightly's
/// rustc version appears nowhere in this repo, and every local check runs
/// under the stable pin, so a patch-level MSRV passes `cargo build`, clippy
/// and the whole test suite and fails only in the Linux `ci-test` lane.
#[test]
fn msrv_names_a_compiler_train_not_a_patch_release() {
    let root = common::workspace_root();
    let manifest =
        std::fs::read_to_string(root.join("Cargo.toml")).expect("read workspace Cargo.toml");

    let declared = manifest_rust_version(&manifest);

    assert_eq!(
        declared.split('.').count(),
        2,
        "[workspace.package].rust-version is {declared}; declare the train \
         (e.g. \"1.98\") instead. A patch-level MSRV excludes the Dylint \
         nightly, which reports the .0 release of its train, and nothing \
         outside the Linux ci-test lane catches it (soldr#3305).",
    );
}

#[test]
fn the_msrv_parsers_read_what_they_claim_to() {
    // A guard whose parsers silently find nothing would pass forever. Pin
    // both against fixtures, including the shapes that previously drifted.
    assert_eq!(
        manifest_rust_version("[workspace.package]\nrust-version = \"1.95.0\"\n"),
        "1.95.0"
    );
    assert_eq!(manifest_rust_version("rust-version = \"1.75\"\n"), "1.75");

    assert_eq!(documented_msrvs("edition 2021, MSRV 1.75\n"), vec!["1.75"]);
    assert_eq!(
        documented_msrvs("MSRV 1.95.0 (`rust-version`)."),
        vec!["1.95.0"]
    );
    // Prose mentioning the term without a number is not a claim.
    assert!(documented_msrvs("The MSRV and the toolchain agree.").is_empty());
}
