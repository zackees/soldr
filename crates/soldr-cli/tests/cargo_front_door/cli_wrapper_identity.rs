//! Process-level coverage for the Soldr-owned effective-wrapper mirror
//! (soldr#2545): a wrapper re-entry whose inherited mirror disagrees with
//! `RUSTC_WRAPPER` must exit 1 before any broker/daemon contact, and a
//! coherent or unmirrored environment must proceed normally.
//! soldr#3504: cargo-llvm-cov's sanctioned chained-wrapper protocol is
//! the one documented mutation of the pair and must pass the guard when
//! its chain resolves back to the mirror.

use std::path::Path;
use std::process::Command;

use crate::common;

/// A wrapper-shaped invocation: `soldr <path-to-rustc> --version`.
/// `wrapper::is_wrapper_invocation` keys on a rustc-shaped path, so the
/// fixture materializes a rustc-named copy of the soldr binary (multicall
/// makes it a working rustc on every platform) in a unique temp dir.
fn wrapper_invocation() -> (Command, String) {
    let dir = std::env::temp_dir().join(format!(
        "soldr-wrapper-identity-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let rustc_path = dir.join(soldr_platform::executable::name::native("rustc"));
    std::fs::copy(common::soldr_bin(), &rustc_path).expect("materialize rustc shim");
    let rustc_like = rustc_path.display().to_string();
    let mut cmd = common::isolated_soldr_command();
    cmd.arg(&rustc_like).arg("--version");
    (cmd, rustc_like)
}

/// Materialize a cargo-llvm-cov-named image beside the fixture's rustc
/// shim so the chain head's path resolves like the real tool (soldr#3504).
fn chain_head(rustc_like: &str) -> String {
    let path = Path::new(rustc_like)
        .with_file_name(soldr_platform::executable::name::native("cargo-llvm-cov"));
    std::fs::copy(common::soldr_bin(), &path).expect("materialize cargo-llvm-cov chain head");
    path.display().to_string()
}

#[test]
fn drifted_owned_wrapper_identity_fails_before_dispatch() {
    let (mut cmd, rustc_like) = wrapper_invocation();
    let output = cmd
        .env("RUSTC_WRAPPER", &rustc_like)
        .env(
            soldr_cli::wrapper_identity::EFFECTIVE_WRAPPER_ENV,
            "/some/other/versioned/shims/rustc",
        )
        .env(
            soldr_cli::wrapper_identity::EFFECTIVE_WRAPPER_ORIGIN_ENV,
            "soldr-managed",
        )
        .output()
        .expect("spawn soldr wrapper");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("RUSTC_WRAPPER identity drifted"),
        "diagnostic must name the drift: {stderr}"
    );
    assert!(
        stderr.contains("soldr-managed") && stderr.contains("wrapper re-entry"),
        "diagnostic must carry origin and boundary: {stderr}"
    );
}

#[test]
fn coherent_owned_wrapper_identity_proceeds() {
    let (mut cmd, rustc_like) = wrapper_invocation();
    let output = cmd
        .env("RUSTC_WRAPPER", &rustc_like)
        .env(
            soldr_cli::wrapper_identity::EFFECTIVE_WRAPPER_ENV,
            &rustc_like,
        )
        .env(
            soldr_cli::wrapper_identity::EFFECTIVE_WRAPPER_ORIGIN_ENV,
            "soldr-managed",
        )
        .output()
        .expect("spawn soldr wrapper");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("identity drifted"),
        "a coherent pair must not be rejected: {stderr}"
    );
}

#[test]
fn unmirrored_environment_is_not_soldr_owned_and_proceeds() {
    let (mut cmd, rustc_like) = wrapper_invocation();
    let output = cmd
        .env("RUSTC_WRAPPER", &rustc_like)
        .output()
        .expect("spawn soldr wrapper");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("identity drifted"),
        "caller-owned wrappers stay unasserted: {stderr}"
    );
}

/// soldr#3504 RED case: cargo-llvm-cov points `RUSTC_WRAPPER` at itself,
/// stashes the soldr shim it displaced in
/// `__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING`, and execs the shim as a
/// chain. The pair legitimately disagrees at re-entry; the chain resolves
/// back to Soldr, so the guard must accept it instead of failing the run.
#[test]
fn cargo_llvm_cov_chained_wrapper_identity_proceeds() {
    let (mut cmd, rustc_like) = wrapper_invocation();
    let llvm_cov = chain_head(&rustc_like);
    let output = cmd
        .env("RUSTC_WRAPPER", &llvm_cov)
        .env(
            soldr_cli::wrapper_identity::EFFECTIVE_WRAPPER_ENV,
            &rustc_like,
        )
        .env(
            soldr_cli::wrapper_identity::EFFECTIVE_WRAPPER_ORIGIN_ENV,
            "soldr-managed",
        )
        .env(
            soldr_cli::wrapper_identity::LLVM_COV_PRE_EXISTING_ENV,
            &rustc_like,
        )
        .output()
        .expect("spawn soldr wrapper");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("identity drifted"),
        "the sanctioned cargo-llvm-cov chain must not be rejected: {stderr}"
    );
}

/// Same protocol with no stash to validate: `RUSTC_WRAPPER` is the
/// cargo-llvm-cov image itself, which is the tool's documented signature.
#[test]
fn cargo_llvm_cov_wrapper_without_stash_proceeds() {
    let (mut cmd, rustc_like) = wrapper_invocation();
    let llvm_cov = chain_head(&rustc_like);
    let output = cmd
        .env("RUSTC_WRAPPER", &llvm_cov)
        .env(
            soldr_cli::wrapper_identity::EFFECTIVE_WRAPPER_ENV,
            &rustc_like,
        )
        .env(
            soldr_cli::wrapper_identity::EFFECTIVE_WRAPPER_ORIGIN_ENV,
            "soldr-managed",
        )
        .output()
        .expect("spawn soldr wrapper");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("identity drifted"),
        "a cargo-llvm-cov head with no stash is the documented chain: {stderr}"
    );
}

/// The chain must resolve back to Soldr: a stash naming some other
/// wrapper means the pair genuinely drifted (soldr#2545 still enforced).
#[test]
fn llvm_cov_chain_resolving_away_from_soldr_fails() {
    let (mut cmd, rustc_like) = wrapper_invocation();
    let llvm_cov = chain_head(&rustc_like);
    let output = cmd
        .env("RUSTC_WRAPPER", &llvm_cov)
        .env(
            soldr_cli::wrapper_identity::EFFECTIVE_WRAPPER_ENV,
            &rustc_like,
        )
        .env(
            soldr_cli::wrapper_identity::EFFECTIVE_WRAPPER_ORIGIN_ENV,
            "soldr-managed",
        )
        .env(
            soldr_cli::wrapper_identity::LLVM_COV_PRE_EXISTING_ENV,
            "/some/other/versioned/shims/rustc",
        )
        .output()
        .expect("spawn soldr wrapper");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("RUSTC_WRAPPER identity drifted"),
        "a chain that does not resolve back to Soldr is drift: {stderr}"
    );
}
