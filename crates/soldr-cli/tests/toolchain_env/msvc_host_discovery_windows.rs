//! End-to-end TDD acceptance test for issue #1079.
//!
//! Proves the contract: from a Windows shell with NO MSVC env vars
//! loaded (no vcvars, no Developer Command Prompt), calling
//! `msvc_host::ensure_msvc_env_for_native("x86_64-pc-windows-msvc")`
//! produces a process env where `link.exe` is findable on `PATH` and
//! `LIB` is populated — i.e. the user no longer needs the
//! `$env:LIB = '...'` workaround documented in the issue.
//!
//! ## Why a separate integration binary
//!
//! cargo runs unit tests in parallel by default, so any test that
//! mutates process-global env vars can race other tests. Integration
//! tests under `tests/` each compile to their own binary with their
//! own process — mutation here cannot leak into the lib `cargo test`
//! workers. We put exactly ONE test in this file and accept the
//! single-test cost of a fresh binary in exchange for safety.
//!
//! ## Skip behavior
//!
//! - Non-Windows: the whole file is `cfg(target_os = "windows")` — it
//!   compiles to an empty test binary on Linux/macOS lanes.
//! - Windows without VS C++ tools: the test skips with a clear stderr
//!   message instead of failing. CI Windows runners without the VC++
//!   workload (rare but possible) shouldn't fail this gate; the
//!   per-PR perf-matrix runners that DO have the workload will.

use soldr_cli::msvc_host::{
    discover_msvc_layout, ensure_msvc_env_for_native, vswhere_path, which_on_path,
    MsvcDetectionError, SOLDR_MSVC_DISCOVERY_ENV_VAR,
};

#[test]
fn ensure_msvc_env_for_native_makes_link_exe_findable_on_plain_powershell() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    // ----- Skip gracefully on hosts without VS C++ tooling. -----
    if !vswhere_path().is_file() {
        eprintln!(
            "msvc_host integration test: skipping — vswhere not present at {}",
            vswhere_path().display()
        );
        return;
    }
    match discover_msvc_layout() {
        Ok(_) => {}
        Err(MsvcDetectionError::NoVsInstall) => {
            eprintln!("msvc_host: skipping — vswhere installed but no VS with C++ tools");
            return;
        }
        Err(MsvcDetectionError::NoSdkRoot | MsvcDetectionError::NoSdkVersion(_)) => {
            eprintln!("msvc_host: skipping — no Windows 10/11 SDK on host");
            return;
        }
        Err(MsvcDetectionError::NoToolsVersion(_)) => {
            eprintln!("msvc_host: skipping — VS install missing VCToolsVersion file");
            return;
        }
        Err(e) => {
            panic!("unexpected discovery error setting up the test: {e}");
        }
    }

    // ----- Save the env we're about to mutate. -----
    let saved_lib = std::env::var_os("LIB");
    let saved_include = std::env::var_os("INCLUDE");
    let saved_libpath = std::env::var_os("LIBPATH");
    let saved_path = std::env::var_os("PATH");
    let saved_optout = std::env::var_os(SOLDR_MSVC_DISCOVERY_ENV_VAR);

    // ----- Sanitize: simulate a plain PowerShell with no vcvars. -----
    // Remove LIB / INCLUDE / LIBPATH entirely.
    std::env::remove_var("LIB");
    std::env::remove_var("INCLUDE");
    std::env::remove_var("LIBPATH");
    // Make sure the opt-out var isn't set to a disabling value left
    // over from earlier tests in this process.
    std::env::remove_var(SOLDR_MSVC_DISCOVERY_ENV_VAR);
    // Strip any PATH entries that contain a `link.exe` so the test
    // really proves *our* injection put it back. We rebuild PATH
    // from only directories that DON'T already resolve link.exe.
    if let Some(orig_path) = saved_path.as_ref() {
        let cleaned: Vec<std::path::PathBuf> = std::env::split_paths(orig_path)
            .filter(|d| !d.join("link.exe").is_file())
            .collect();
        let joined = std::env::join_paths(cleaned).expect("clean PATH");
        std::env::set_var("PATH", joined);
    }

    // ----- RED check: confirm sanitization actually removed link.exe. -----
    let link_before = which_on_path("link.exe");
    let lib_before = std::env::var_os("LIB");
    assert!(
        link_before.is_none(),
        "test setup bug: link.exe still on PATH after sanitization at {}",
        link_before
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    assert!(
        lib_before.is_none(),
        "test setup bug: LIB still set after sanitization"
    );

    // ----- The contract under test. -----
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let applied = rt
        .block_on(ensure_msvc_env_for_native("x86_64-pc-windows-msvc"))
        .expect("discovery should succeed on a Windows host with VS installed");

    // ----- GREEN check: env is now usable. -----
    // Snapshot results BEFORE restoring saved env so failure messages
    // describe the state we actually care about.
    let applied_was_true = applied;
    let link_after = which_on_path("link.exe");
    let lib_after = std::env::var_os("LIB");
    let include_after = std::env::var_os("INCLUDE");
    let libpath_after = std::env::var_os("LIBPATH");

    // ----- Restore. -----
    match saved_lib {
        Some(v) => std::env::set_var("LIB", v),
        None => std::env::remove_var("LIB"),
    }
    match saved_include {
        Some(v) => std::env::set_var("INCLUDE", v),
        None => std::env::remove_var("INCLUDE"),
    }
    match saved_libpath {
        Some(v) => std::env::set_var("LIBPATH", v),
        None => std::env::remove_var("LIBPATH"),
    }
    match saved_path {
        Some(v) => std::env::set_var("PATH", v),
        None => std::env::remove_var("PATH"),
    }
    match saved_optout {
        Some(v) => std::env::set_var(SOLDR_MSVC_DISCOVERY_ENV_VAR, v),
        None => std::env::remove_var(SOLDR_MSVC_DISCOVERY_ENV_VAR),
    }

    // ----- Now assert (after restoration is safely done). -----
    assert!(
        applied_was_true,
        "ensure_msvc_env_for_native should report it injected env (returned false)"
    );
    let link_resolved = link_after.expect(
        "issue #1079 contract broken: link.exe NOT findable after \
         ensure_msvc_env_for_native — soldr is failing to put the MSVC \
         bin directory on PATH",
    );
    assert!(
        link_resolved
            .to_string_lossy()
            .to_lowercase()
            .contains(r"\vc\tools\msvc\"),
        "link.exe resolved at {} but doesn't look like a VC\\Tools\\MSVC binary",
        link_resolved.display()
    );
    let lib = lib_after.expect("LIB env was not set by ensure_msvc_env_for_native");
    let lib_str = lib.to_string_lossy();
    assert!(
        lib_str.contains(r"\VC\Tools\MSVC\"),
        "LIB env should contain VC\\Tools\\MSVC: {lib_str}"
    );
    assert!(
        lib_str.to_lowercase().contains(r"\ucrt\x64"),
        "LIB env should contain \\ucrt\\x64 (Windows SDK ucrt libs): {lib_str}"
    );
    let inc = include_after.expect("INCLUDE env was not set");
    let inc_str = inc.to_string_lossy();
    assert!(
        inc_str.contains(r"\VC\Tools\MSVC\"),
        "INCLUDE env should contain VC\\Tools\\MSVC: {inc_str}"
    );
    assert!(
        libpath_after.is_some(),
        "LIBPATH env was not set by ensure_msvc_env_for_native"
    );
}
