//! Unit coverage split from `msvc_host.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

//! Unit tests for the issue #1079 MSVC host discovery.
//!
//! Pure-function tests (no real VS / SDK install required) run on
//! every platform. The `discover_msvc_layout` end-to-end probe is
//! runtime-gated to Windows hosts and skips gracefully when vswhere
//! isn't present (CI without VS), so the same source compiles +
//! tests on Linux CI lanes too.
//!
//! The acceptance test for the issue is in
//! `tests/toolchain_env/msvc_host_discovery_windows.rs` (an
//! integration test, so nextest runs its env-mutation in a process
//! of its own and it cannot race other tests).

use super::*;

// Runtime-gated to Windows hosts: the assertions look for
// backslash-joined path segments (`VC\Tools\MSVC\...`) which only
// appear when
// `PathBuf::display()` runs on Windows. The function under test
// (`synthesize_env_x64`) uses `PathBuf::display()` and so produces
// platform-native separators — on Linux that's forward slashes.
// Issue #1105 surfaced this by running the suite under Docker
// Linux for the first time. Fixing `synthesize_env_x64` itself to
// always emit backslashes is a separate concern; for now we keep
// the existing Windows-only coverage and ensure the test no longer
// panics on Linux CI / Linux Docker runs.
#[test]
fn synthesize_env_x64_builds_canonical_paths() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let layout = MsvcHostLayout {
        vs_install: PathBuf::from(r"C:\Program Files (x86)\Microsoft Visual Studio\2019\Community"),
        vc_tools_version: "14.29.30133".into(),
        sdk_root: PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10"),
        sdk_version: "10.0.22621.0".into(),
        source: MsvcToolsSource::Host,
    };
    let env = layout.synthesize_env_x64();

    assert!(
        env.lib.contains(r"VC\Tools\MSVC\14.29.30133\lib\x64"),
        "lib should contain canonical MSVC x64 libs path: {}",
        env.lib
    );
    assert!(
        env.lib
            .contains(r"Windows Kits\10\Lib\10.0.22621.0\ucrt\x64"),
        "lib should contain ucrt x64 path: {}",
        env.lib
    );
    assert!(
        env.include.contains(r"VC\Tools\MSVC\14.29.30133\include"),
        "include should contain MSVC include path: {}",
        env.include
    );
    assert!(
        env.path_prepend
            .contains(r"VC\Tools\MSVC\14.29.30133\bin\Hostx64\x64"),
        "path_prepend should contain x64 host tools (link.exe lives here): {}",
        env.path_prepend
    );
}

// soldr#2292: a ManagedBundle-sourced layout treats `vs_install` as
// the tools dir directly — no `VC\Tools\MSVC\<version>` nesting.
#[test]
fn synthesize_env_x64_managed_bundle_uses_root_directly() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let layout = MsvcHostLayout {
        vs_install: PathBuf::from(
            r"C:\Users\me\.soldr-dev\bin\syslib\msvc\14.44.35207\windows-x64\package",
        ),
        vc_tools_version: "14.44.35207".into(),
        sdk_root: PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10"),
        sdk_version: "10.0.22621.0".into(),
        source: MsvcToolsSource::ManagedBundle,
    };
    let env = layout.synthesize_env_x64();

    assert!(
        env.lib.contains(r"package\lib\x64"),
        "bundle LIB should hang the lib dir directly off the bundle root: {}",
        env.lib
    );
    assert!(
        !env.lib.contains(r"VC\Tools\MSVC"),
        "bundle LIB must NOT insert a VC\\Tools\\MSVC\\<version> segment: {}",
        env.lib
    );
    assert!(
        env.path_prepend.contains(r"package\bin\Hostx64\x64"),
        "bundle path_prepend should point at bin\\Hostx64\\x64 directly off root: {}",
        env.path_prepend
    );
}

// ---- soldr#2292: compatibility check -----------------------------

#[test]
fn is_compatible_vc_tools_version_accepts_v143_and_v144() {
    // This machine's actual host toolset (must stay the
    // no-download hot path).
    assert!(is_compatible_vc_tools_version("14.44.35207"));
    // Lower bound of the accepted minor series.
    assert!(is_compatible_vc_tools_version("14.30.30705"));
    // A later v144 update.
    assert!(is_compatible_vc_tools_version("14.41.34120"));
}

#[test]
fn is_compatible_vc_tools_version_rejects_pre_vs2022_and_junk() {
    // VS2019 (v142) — below the accepted minor floor.
    assert!(!is_compatible_vc_tools_version("14.29.30133"));
    // Wrong major entirely.
    assert!(!is_compatible_vc_tools_version("13.99.99999"));
    assert!(!is_compatible_vc_tools_version("15.0.0"));
    // Unparseable — fail closed.
    assert!(!is_compatible_vc_tools_version(""));
    assert!(!is_compatible_vc_tools_version("not-a-version"));
    assert!(!is_compatible_vc_tools_version("14"));
}

#[test]
fn hostx64_cl_exe_path_layout() {
    let p = hostx64_cl_exe_path(Path::new(r"C:\VS"), "14.44.35207");
    assert!(
        p.ends_with(r"bin\Hostx64\x64\cl.exe") || p.ends_with("bin/Hostx64/x64/cl.exe"),
        "{}",
        p.display()
    );
    assert!(p.to_string_lossy().contains("14.44.35207"));
}

// ---- soldr#2292: decide_msvc_resolution pure decision function ---

#[test]
fn decide_msvc_resolution_already_in_env_short_circuits() {
    let host = HostProbeOutcome::NotFound("irrelevant".into());
    assert_eq!(
        decide_msvc_resolution(true, &host),
        MsvcResolution::AlreadyInEnv
    );
}

#[test]
fn decide_msvc_resolution_uses_host_when_compatible_and_cl_exe_present() {
    let host = HostProbeOutcome::Found {
        vs_install: PathBuf::from(r"C:\VS"),
        vc_tools_version: "14.44.35207".into(),
        cl_exe_exists: true,
    };
    assert_eq!(
        decide_msvc_resolution(false, &host),
        MsvcResolution::UseHost,
        "compatible host toolset with a real cl.exe must be the no-download hot path"
    );
}

#[test]
fn decide_msvc_resolution_downloads_when_host_missing() {
    let host = HostProbeOutcome::NotFound("no VS install".into());
    let resolution = decide_msvc_resolution(false, &host);
    assert!(matches!(
        resolution,
        MsvcResolution::Download(DownloadReason::HostMissing(_))
    ));
}

#[test]
fn decide_msvc_resolution_downloads_when_host_incompatible() {
    let host = HostProbeOutcome::Found {
        vs_install: PathBuf::from(r"C:\VS"),
        vc_tools_version: "14.29.30133".into(),
        cl_exe_exists: true,
    };
    let resolution = decide_msvc_resolution(false, &host);
    assert!(
        matches!(
            resolution,
            MsvcResolution::Download(DownloadReason::HostIncompatible { .. })
        ),
        "a compatible-looking but too-old host toolset must fall back to download: \
                 {resolution:?}"
    );
}

#[test]
fn decide_msvc_resolution_downloads_when_cl_exe_missing_even_if_version_ok() {
    // A "compatible" version string but no cl.exe on disk must
    // be treated as missing, not compatible (soldr#2292
    // requirement 2 — don't trust the layout blindly).
    let host = HostProbeOutcome::Found {
        vs_install: PathBuf::from(r"C:\VS"),
        vc_tools_version: "14.44.35207".into(),
        cl_exe_exists: false,
    };
    let resolution = decide_msvc_resolution(false, &host);
    assert!(
        matches!(
            resolution,
            MsvcResolution::Download(DownloadReason::ClExeMissing { .. })
        ),
        "missing cl.exe must trigger download even with a compatible version string: \
                 {resolution:?}"
    );
}

// soldr#2292: the real-world path an incompatible/no-VS host actually
// hits today — the public MSVC bundle catalogue publication is on
// hold (licensing), so `download_managed_msvc_bundle` fails too, and
// `MsvcDetectionError::HostAndDownloadFailed` is what the user sees.
// The message MUST name both failures plus the opt-out escape hatch —
// "host probe failed, then download also failed, good luck" with no
// actionable detail is exactly what soldr#2292 was filed to fix.
#[test]
fn host_and_download_failed_message_names_both_failures_and_escape_hatch() {
    let err = MsvcDetectionError::HostAndDownloadFailed {
        host_reason: DownloadReason::HostIncompatible {
            version: "14.29.30133".into(),
        }
        .to_string(),
        download_error: "syslib bundle for msvc/14.44.35207/windows-x64 not yet \
                                  ingested into the soldr-toolchain catalogue"
            .to_string(),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("14.29.30133"),
        "must name the incompatible host version: {msg}"
    );
    assert!(
        msg.contains("not yet ingested"),
        "must surface the download failure detail verbatim: {msg}"
    );
    assert!(
        msg.contains("SOLDR_MSVC_DISCOVERY=off"),
        "must name the opt-out escape hatch: {msg}"
    );
}

#[test]
fn vswhere_path_honors_override_env_var() {
    // Mutate env: ok because we read-then-restore. Run serially
    // would be ideal but a single set/clear pair is atomic enough
    // for cargo test's default parallelism — and no other tests
    // touch SOLDR_VSWHERE.
    let prior = std::env::var_os(SOLDR_VSWHERE_ENV_VAR);
    std::env::set_var(SOLDR_VSWHERE_ENV_VAR, r"D:\custom\vswhere.exe");
    let p = vswhere_path();
    match prior {
        Some(v) => std::env::set_var(SOLDR_VSWHERE_ENV_VAR, v),
        None => std::env::remove_var(SOLDR_VSWHERE_ENV_VAR),
    }
    assert_eq!(p, PathBuf::from(r"D:\custom\vswhere.exe"));
}

#[test]
fn opted_out_recognizes_off_zero_false() {
    let prior = std::env::var_os(SOLDR_MSVC_DISCOVERY_ENV_VAR);
    for v in ["off", "OFF", "0", "false", "FALSE", "no", "  off  "] {
        std::env::set_var(SOLDR_MSVC_DISCOVERY_ENV_VAR, v);
        assert!(opted_out(), "value `{v}` should opt out");
    }
    for v in ["on", "1", "true", "auto", ""] {
        std::env::set_var(SOLDR_MSVC_DISCOVERY_ENV_VAR, v);
        assert!(!opted_out(), "value `{v}` should NOT opt out");
    }
    match prior {
        Some(v) => std::env::set_var(SOLDR_MSVC_DISCOVERY_ENV_VAR, v),
        None => std::env::remove_var(SOLDR_MSVC_DISCOVERY_ENV_VAR),
    }
}

#[test]
fn pick_highest_sdk_version_skips_partial_installs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let inc = tmp.path().join("Include");
    // 10.0.19041.0 — half-installed, missing ucrt/
    std::fs::create_dir_all(inc.join("10.0.19041.0").join("um")).unwrap();
    // 10.0.22621.0 — fully installed
    std::fs::create_dir_all(inc.join("10.0.22621.0").join("um")).unwrap();
    std::fs::create_dir_all(inc.join("10.0.22621.0").join("ucrt")).unwrap();
    // 10.0.20348.0 — also fully installed but older
    std::fs::create_dir_all(inc.join("10.0.20348.0").join("um")).unwrap();
    std::fs::create_dir_all(inc.join("10.0.20348.0").join("ucrt")).unwrap();

    let picked = pick_highest_sdk_version(tmp.path()).expect("should find a version");
    assert_eq!(
        picked, "10.0.22621.0",
        "should pick the highest fully-installed version, skipping the partial 10.0.19041.0"
    );
}

#[test]
fn pick_highest_sdk_version_errors_on_empty_install() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Only Include/ exists, but no version subdirs with um+ucrt.
    std::fs::create_dir_all(tmp.path().join("Include")).unwrap();
    std::fs::create_dir_all(tmp.path().join("Include").join("10.0.0.0").join("um")).unwrap();
    // missing ucrt → should not qualify
    let err = pick_highest_sdk_version(tmp.path()).expect_err("should fail");
    assert!(matches!(err, MsvcDetectionError::NoSdkVersion(_)));
}

#[test]
fn ensure_msvc_env_for_native_is_noop_on_non_msvc_target() {
    // This test runs on every platform and proves the early-out
    // for non-MSVC targets. Even on Windows, asking for a
    // non-MSVC target must skip discovery.
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let applied = rt
        .block_on(ensure_msvc_env_for_native("x86_64-unknown-linux-gnu"))
        .expect("noop should not error");
    assert!(!applied, "linux-gnu target must not trigger MSVC discovery");
}

// ---- soldr#1105: .cargo/config.toml rust-lld linker probe -----

#[test]
fn config_pins_rust_lld_detects_bare_rust_lld_exe() {
    let toml = r#"
[target.x86_64-pc-windows-msvc]
linker = "rust-lld.exe"
"#;
    assert!(
        config_pins_rust_lld(toml),
        "should match bare rust-lld.exe linker pin"
    );
}

#[test]
fn config_pins_rust_lld_detects_path_to_rust_lld() {
    let toml = r#"
[target.aarch64-pc-windows-msvc]
linker = "C:/Users/me/.rustup/toolchains/stable/bin/rust-lld.exe"
"#;
    assert!(
        config_pins_rust_lld(toml),
        "should match absolute path to rust-lld.exe"
    );
}

// Backslash-path variant — TOML uses `\\` escapes so the actual
// string the parser sees is `C:\Users\...\rust-lld.exe`. This
// case must work even when the test runs on Linux (Path::file_name
// doesn't recognize `\` on Linux); the implementation normalizes
// separators before splitting.
#[test]
fn config_pins_rust_lld_detects_backslash_path_on_any_host() {
    let toml = "
[target.x86_64-pc-windows-msvc]
linker = \"C:\\\\Users\\\\me\\\\.rustup\\\\toolchains\\\\stable\\\\bin\\\\rust-lld.exe\"
";
    assert!(
        config_pins_rust_lld(toml),
        "should match Windows-backslash path even on Linux hosts"
    );
}

#[test]
fn config_pins_rust_lld_detects_no_exe_suffix() {
    let toml = r#"
[target.x86_64-pc-windows-msvc]
linker = "rust-lld"
"#;
    assert!(
        config_pins_rust_lld(toml),
        "should match rust-lld without .exe"
    );
}

#[test]
fn config_pins_rust_lld_ignores_link_exe() {
    let toml = r#"
[target.x86_64-pc-windows-msvc]
linker = "link.exe"
"#;
    assert!(
        !config_pins_rust_lld(toml),
        "should NOT match the default link.exe — soldr#1105 only cares about rust-lld pins"
    );
}

#[test]
fn config_pins_rust_lld_ignores_non_windows_targets() {
    let toml = r#"
[target.x86_64-unknown-linux-gnu]
linker = "rust-lld"
"#;
    assert!(
        !config_pins_rust_lld(toml),
        "rust-lld pinned for a linux target must not trigger MSVC env injection"
    );
}

#[test]
fn config_pins_rust_lld_handles_malformed_toml_silently() {
    let toml = "not = valid = toml = at = all";
    assert!(
        !config_pins_rust_lld(toml),
        "malformed cargo config must be a soft-skip, not a panic — soldr#1105"
    );
}

#[test]
fn project_pins_rust_lld_walks_ancestors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Put .cargo/config.toml at the parent level and CWD two levels deep.
    let parent = tmp.path().join("repo");
    let nested = parent.join("crates").join("inner");
    std::fs::create_dir_all(&nested).unwrap();
    let cargo_dir = parent.join(".cargo");
    std::fs::create_dir_all(&cargo_dir).unwrap();
    std::fs::write(
        cargo_dir.join("config.toml"),
        r#"
[target.x86_64-pc-windows-msvc]
linker = "rust-lld.exe"
"#,
    )
    .unwrap();
    assert!(
        project_pins_rust_lld_for_msvc(&nested),
        "walker should find the parent-level cargo config from a nested CWD"
    );
}

#[test]
fn project_pins_rust_lld_no_config_is_false() {
    let tmp = tempfile::tempdir().expect("tempdir");
    assert!(
        !project_pins_rust_lld_for_msvc(tmp.path()),
        "empty directory tree must return false"
    );
}

#[test]
fn ensure_msvc_env_for_native_is_noop_on_non_windows_host() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let applied = rt
        .block_on(ensure_msvc_env_for_native("x86_64-pc-windows-msvc"))
        .expect("noop should not error");
    assert!(!applied, "non-windows host must not attempt MSVC discovery");
}

// -------------------------------------------------------------------
// Windows-only end-to-end probe. Skips gracefully on hosts without
// a VS install (CI lanes without VC++ workload).
// -------------------------------------------------------------------
#[test]
fn discover_msvc_layout_on_developer_machine_finds_real_link_exe() {
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let v = vswhere_path();
    if !v.is_file() {
        eprintln!(
            "msvc_host: skipping discovery test — vswhere not present at {}",
            v.display()
        );
        return;
    }
    let layout = match discover_msvc_layout() {
        Ok(l) => l,
        Err(MsvcDetectionError::NoVsInstall) => {
            eprintln!("msvc_host: skipping — vswhere installed but no VS with C++ tools");
            return;
        }
        Err(MsvcDetectionError::NoSdkRoot | MsvcDetectionError::NoSdkVersion(_)) => {
            eprintln!("msvc_host: skipping — no Windows 10/11 SDK on host");
            return;
        }
        Err(e) => panic!("unexpected discovery error: {e}"),
    };

    // The whole point of discovery is to produce a layout that
    // resolves to a real link.exe. If this asserts, the layout
    // is wrong and the env we'd inject would not fix the
    // "linker `link.exe` not found" symptom.
    let link_exe = layout
        .vs_install
        .join("VC")
        .join("Tools")
        .join("MSVC")
        .join(&layout.vc_tools_version)
        .join("bin")
        .join("Hostx64")
        .join("x64")
        .join("link.exe");
    assert!(
        link_exe.is_file(),
        "expected discovered layout to point at a real link.exe — got {}",
        link_exe.display()
    );

    let env = layout.synthesize_env_x64();
    assert!(
        env.lib.contains(r"VC\Tools\MSVC"),
        "lib should contain VC\\Tools\\MSVC: {}",
        env.lib
    );
    assert!(
        env.path_prepend.contains(r"VC\Tools\MSVC"),
        "path_prepend should contain VC\\Tools\\MSVC: {}",
        env.path_prepend
    );
}
