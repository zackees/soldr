//! Windows MSVC host-toolchain auto-discovery (issue #1079).
//!
//! Synthesizes the env vars (`LIB`, `INCLUDE`, `PATH`, `LIBPATH`) that
//! `vcvars64.bat` sets up, by probing the host's Visual Studio install
//! via `vswhere.exe` and the Windows 10/11 SDK via filesystem
//! enumeration. Lets soldr-managed `cargo build` / `cargo test`
//! invocations succeed from a plain PowerShell, eliminating the
//! downstream `$env:LIB` workaround documented in issue #1079.
//!
//! ## Detect-then-download (issue #1079 comment thread)
//!
//! This module covers the **detect-host** half. The download-fallback
//! half (synthesize the same env from a soldr-managed MSVC catalogue
//! when no host VS is installed) is tracked in #1079 as a follow-up
//! and is not implemented here — the immediate user-blocking symptom
//! is satisfied by host detection alone, which is the 99% case on
//! developer machines.
//!
//! ## Behavior contract
//!
//! - On non-Windows hosts: every function is a no-op.
//! - On Windows with `SOLDR_MSVC_DISCOVERY=off` (or `0` / `false`):
//!   no-op. Lets users force-disable the auto-injection.
//! - On Windows where `LIB` is already set AND `link.exe` is already
//!   on `PATH` (caller is in a Developer Command Prompt or has run
//!   vcvars manually): no-op. soldr respects an existing env rather
//!   than clobbering it.
//! - Otherwise: probe vswhere → resolve VS install + tools version →
//!   probe SDK → write `LIB`/`INCLUDE`/`PATH`/`LIBPATH` onto the
//!   current process env.

use std::path::{Path, PathBuf};

/// Env var that opts out of MSVC host discovery. Accepts `off`, `0`,
/// or `false`. Any other value (including unset) keeps discovery on.
pub const SOLDR_MSVC_DISCOVERY_ENV_VAR: &str = "SOLDR_MSVC_DISCOVERY";

/// Override path to `vswhere.exe`. Useful in tests and for unusual
/// installs. When unset, [`vswhere_path`] uses the canonical
/// `%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe`
/// location that ships with every VS 2017+ install.
pub const SOLDR_VSWHERE_ENV_VAR: &str = "SOLDR_VSWHERE";

/// Override path to the Windows SDK root (e.g. `C:\Program Files (x86)\Windows Kits\10`).
/// When unset, [`probe_windows_sdk`] enumerates the standard locations.
pub const SOLDR_WINDOWS_SDK_ROOT_ENV_VAR: &str = "SOLDR_WINDOWS_SDK_ROOT";

/// The four env vars [`ensure_msvc_env_for_native`] writes when it
/// successfully detects a host MSVC install. `PATH` is the only
/// existing-value-respected one (we *prepend* tool dirs); the others
/// are full replacements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsvcHostEnv {
    pub lib: String,
    pub include: String,
    /// Semicolon-joined list of directories to **prepend** to `PATH`.
    pub path_prepend: String,
    pub libpath: String,
}

/// Resolved MSVC + Windows SDK layout. Constructed by
/// [`discover_msvc_layout`] from the host filesystem; transformed
/// into [`MsvcHostEnv`] via [`MsvcHostLayout::synthesize_env_x64`] —
/// no I/O in the synthesis step so the pure transformation is unit-
/// testable without a real VS install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsvcHostLayout {
    pub vs_install: PathBuf,
    pub vc_tools_version: String,
    pub sdk_root: PathBuf,
    pub sdk_version: String,
}

#[derive(Debug, thiserror::Error)]
pub enum MsvcDetectionError {
    #[error("vswhere.exe not found at expected location: {0}")]
    VswhereNotFound(PathBuf),
    #[error("vswhere.exe returned non-zero: {0}")]
    VswhereFailed(String),
    #[error("vswhere.exe found no Visual Studio install with the VC++ x86/x64 tools component")]
    NoVsInstall,
    #[error("MSVC tools version file missing or empty: {0}")]
    NoToolsVersion(PathBuf),
    #[error("Windows 10/11 SDK install not found in any standard location")]
    NoSdkRoot,
    #[error("Windows SDK at {0} has no usable version directory under Include/")]
    NoSdkVersion(PathBuf),
    #[error("io error during MSVC discovery: {0}")]
    Io(#[from] std::io::Error),
}

impl MsvcHostLayout {
    /// Pure transformation from a discovered layout to the env vars
    /// `vcvars64.bat` writes for an x64 host targeting x64. No
    /// filesystem access — fully unit-testable.
    pub fn synthesize_env_x64(&self) -> MsvcHostEnv {
        let msvc = self
            .vs_install
            .join("VC")
            .join("Tools")
            .join("MSVC")
            .join(&self.vc_tools_version);
        let sdk_inc = self.sdk_root.join("Include").join(&self.sdk_version);
        let sdk_lib = self.sdk_root.join("Lib").join(&self.sdk_version);

        let lib = vec![
            msvc.join("lib").join("x64"),
            msvc.join("ATLMFC").join("lib").join("x64"),
            sdk_lib.join("um").join("x64"),
            sdk_lib.join("ucrt").join("x64"),
        ];
        let include = vec![
            msvc.join("include"),
            msvc.join("ATLMFC").join("include"),
            sdk_inc.join("ucrt"),
            sdk_inc.join("shared"),
            sdk_inc.join("um"),
            sdk_inc.join("winrt"),
            sdk_inc.join("cppwinrt"),
        ];
        let path_prepend = vec![
            msvc.join("bin").join("Hostx64").join("x64"),
            self.sdk_root.join("bin").join(&self.sdk_version).join("x64"),
            msvc.join("bin").join("Hostx64").join("x86"),
        ];
        let libpath = vec![
            msvc.join("lib").join("x64"),
            msvc.join("ATLMFC").join("lib").join("x64"),
        ];

        MsvcHostEnv {
            lib: join_semicolons(&lib),
            include: join_semicolons(&include),
            path_prepend: join_semicolons(&path_prepend),
            libpath: join_semicolons(&libpath),
        }
    }
}

fn join_semicolons(parts: &[PathBuf]) -> String {
    parts
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(";")
}

/// Returns `true` when the env was injected, `false` when discovery
/// was skipped (non-Windows, opted out, already in a vcvars env, or
/// target is not MSVC). Returns `Err` only when discovery was
/// attempted and failed in a way the caller should know about — most
/// commonly "no VS install with VC++ tools".
pub fn ensure_msvc_env_for_native(target_triple: &str) -> Result<bool, MsvcDetectionError> {
    if !cfg!(target_os = "windows") {
        return Ok(false);
    }
    if !target_triple.ends_with("-pc-windows-msvc") {
        return Ok(false);
    }
    if opted_out() {
        return Ok(false);
    }
    if already_in_msvc_env() {
        return Ok(false);
    }
    let layout = discover_msvc_layout()?;
    let env = layout.synthesize_env_x64();
    apply_to_process(&env);
    Ok(true)
}

/// Returns `true` if `SOLDR_MSVC_DISCOVERY` is set to `off`, `0`, or
/// `false`. Anything else (including missing) keeps discovery on.
pub fn opted_out() -> bool {
    match std::env::var(SOLDR_MSVC_DISCOVERY_ENV_VAR) {
        Ok(v) => {
            let lower = v.trim().to_lowercase();
            matches!(lower.as_str(), "off" | "0" | "false" | "no")
        }
        Err(_) => false,
    }
}

/// Heuristic for "user already has a working MSVC env loaded".
/// Conservative on purpose: we only skip when BOTH `LIB` is set
/// AND `link.exe` is findable on PATH. A half-loaded env (LIB without
/// link.exe, or vice versa) still triggers discovery so we end up in
/// a consistent state.
pub fn already_in_msvc_env() -> bool {
    let lib_set = std::env::var_os("LIB")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let link_on_path = which_on_path("link.exe").is_some();
    lib_set && link_on_path
}

/// Look up an executable on the current process's `PATH`. Returns
/// the first match, or `None`. Kept dep-free (no `which` crate)
/// because we only need bare-name lookup with `.is_file()`.
pub fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// End-to-end host probe: vswhere → VS install → tools version →
/// Windows SDK root + version. Returns the resolved layout or a
/// structured error for the missing step.
pub fn discover_msvc_layout() -> Result<MsvcHostLayout, MsvcDetectionError> {
    let vswhere = vswhere_path();
    if !vswhere.is_file() {
        return Err(MsvcDetectionError::VswhereNotFound(vswhere));
    }
    let install = run_vswhere_install_path(&vswhere)?;
    let vc_tools_version = read_vc_tools_version(&install)?;
    let (sdk_root, sdk_version) = probe_windows_sdk()?;
    Ok(MsvcHostLayout {
        vs_install: install,
        vc_tools_version,
        sdk_root,
        sdk_version,
    })
}

/// Resolves the location of `vswhere.exe`. Honors
/// [`SOLDR_VSWHERE_ENV_VAR`] for overrides; otherwise points at the
/// canonical install path that ships with every VS 2017+ installer.
pub fn vswhere_path() -> PathBuf {
    if let Some(p) = std::env::var_os(SOLDR_VSWHERE_ENV_VAR) {
        return PathBuf::from(p);
    }
    let pf86 = std::env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)"));
    pf86.join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe")
}

fn run_vswhere_install_path(vswhere: &Path) -> Result<PathBuf, MsvcDetectionError> {
    let out = std::process::Command::new(vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
            "-format",
            "value",
            "-utf8",
        ])
        .output()?;
    if !out.status.success() {
        return Err(MsvcDetectionError::VswhereFailed(format!(
            "status={:?} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return Err(MsvcDetectionError::NoVsInstall);
    }
    Ok(PathBuf::from(path))
}

fn read_vc_tools_version(install: &Path) -> Result<String, MsvcDetectionError> {
    let f = install
        .join("VC")
        .join("Auxiliary")
        .join("Build")
        .join("Microsoft.VCToolsVersion.default.txt");
    let s =
        std::fs::read_to_string(&f).map_err(|_| MsvcDetectionError::NoToolsVersion(f.clone()))?;
    let v = s.trim().to_string();
    if v.is_empty() {
        return Err(MsvcDetectionError::NoToolsVersion(f));
    }
    Ok(v)
}

fn probe_windows_sdk() -> Result<(PathBuf, String), MsvcDetectionError> {
    if let Some(root_override) = std::env::var_os(SOLDR_WINDOWS_SDK_ROOT_ENV_VAR) {
        let root = PathBuf::from(root_override);
        let version = pick_highest_sdk_version(&root)?;
        return Ok((root, version));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for env_name in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(pf) = std::env::var_os(env_name) {
            candidates.push(PathBuf::from(pf).join("Windows Kits").join("10"));
        }
    }
    for root in candidates {
        if root.is_dir() {
            let version = pick_highest_sdk_version(&root)?;
            return Ok((root, version));
        }
    }
    Err(MsvcDetectionError::NoSdkRoot)
}

/// Enumerate `<root>/Include/<version>/` directories and return the
/// lexically-largest one that has the canonical `um/` + `ucrt/`
/// subdirs (filters out half-installed SDKs). Lexical ordering works
/// because Windows SDK versions are `10.0.NNNNN.N` with the build
/// number padded to a fixed width.
pub fn pick_highest_sdk_version(root: &Path) -> Result<String, MsvcDetectionError> {
    let include = root.join("Include");
    let mut versions = Vec::new();
    if include.is_dir() {
        for entry in std::fs::read_dir(&include)? {
            let entry = entry?;
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("10.") {
                continue;
            }
            let p = entry.path();
            if p.join("um").is_dir() && p.join("ucrt").is_dir() {
                versions.push(name);
            }
        }
    }
    if versions.is_empty() {
        return Err(MsvcDetectionError::NoSdkVersion(include));
    }
    versions.sort();
    Ok(versions.pop().expect("non-empty"))
}

fn apply_to_process(env: &MsvcHostEnv) {
    std::env::set_var("LIB", &env.lib);
    std::env::set_var("INCLUDE", &env.include);
    std::env::set_var("LIBPATH", &env.libpath);
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = std::ffi::OsString::new();
    new_path.push(&env.path_prepend);
    if !env.path_prepend.ends_with(';') {
        new_path.push(";");
    }
    new_path.push(&existing);
    std::env::set_var("PATH", new_path);
}

#[cfg(test)]
mod tests {
    //! Unit tests for the issue #1079 MSVC host discovery.
    //!
    //! Pure-function tests (no real VS / SDK install required) run on
    //! every platform. The `discover_msvc_layout` end-to-end probe is
    //! gated to `cfg(target_os = "windows")` and skips gracefully when
    //! vswhere isn't present (CI without VS), so the same source
    //! compiles + tests on Linux CI lanes too.
    //!
    //! The acceptance test for the issue is in
    //! `tests/msvc_host_discovery_windows.rs` (separate integration
    //! binary so the env-mutation it does cannot race other tests).

    use super::*;
    use crate::timed_test;
    use std::time::Duration;

    timed_test!(synthesize_env_x64_builds_canonical_paths, Duration::from_secs(5), {
        let layout = MsvcHostLayout {
            vs_install: PathBuf::from(
                r"C:\Program Files (x86)\Microsoft Visual Studio\2019\Community",
            ),
            vc_tools_version: "14.29.30133".into(),
            sdk_root: PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10"),
            sdk_version: "10.0.22621.0".into(),
        };
        let env = layout.synthesize_env_x64();

        assert!(
            env.lib.contains(r"VC\Tools\MSVC\14.29.30133\lib\x64"),
            "lib should contain canonical MSVC x64 libs path: {}",
            env.lib
        );
        assert!(
            env.lib.contains(r"Windows Kits\10\Lib\10.0.22621.0\ucrt\x64"),
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
    });

    timed_test!(vswhere_path_honors_override_env_var, Duration::from_secs(5), {
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
    });

    timed_test!(opted_out_recognizes_off_zero_false, Duration::from_secs(5), {
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
    });

    timed_test!(pick_highest_sdk_version_skips_partial_installs, Duration::from_secs(10), {
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
    });

    timed_test!(pick_highest_sdk_version_errors_on_empty_install, Duration::from_secs(10), {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Only Include/ exists, but no version subdirs with um+ucrt.
        std::fs::create_dir_all(tmp.path().join("Include")).unwrap();
        std::fs::create_dir_all(tmp.path().join("Include").join("10.0.0.0").join("um")).unwrap();
        // missing ucrt → should not qualify
        let err = pick_highest_sdk_version(tmp.path()).expect_err("should fail");
        assert!(matches!(err, MsvcDetectionError::NoSdkVersion(_)));
    });

    timed_test!(ensure_msvc_env_for_native_is_noop_on_non_msvc_target, Duration::from_secs(5), {
        // This test runs on every platform and proves the early-out
        // for non-MSVC targets. Even on Windows, asking for a
        // non-MSVC target must skip discovery.
        let applied = ensure_msvc_env_for_native("x86_64-unknown-linux-gnu")
            .expect("noop should not error");
        assert!(!applied, "linux-gnu target must not trigger MSVC discovery");
    });

    #[cfg(not(target_os = "windows"))]
    timed_test!(ensure_msvc_env_for_native_is_noop_on_non_windows_host, Duration::from_secs(5), {
        let applied = ensure_msvc_env_for_native("x86_64-pc-windows-msvc")
            .expect("noop should not error");
        assert!(
            !applied,
            "non-windows host must not attempt MSVC discovery"
        );
    });

    // -------------------------------------------------------------------
    // Windows-only end-to-end probe. Skips gracefully on hosts without
    // a VS install (CI lanes without VC++ workload).
    // -------------------------------------------------------------------
    #[cfg(target_os = "windows")]
    timed_test!(discover_msvc_layout_on_developer_machine_finds_real_link_exe, Duration::from_secs(30), {
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
    });
}
