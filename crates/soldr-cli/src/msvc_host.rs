//! Windows MSVC host-toolchain auto-discovery (issue #1079).
//!
//! Synthesizes the env vars (`LIB`, `INCLUDE`, `PATH`, `LIBPATH`) that
//! `vcvars64.bat` sets up, by probing the host's Visual Studio install
//! via `vswhere.exe` and the Windows 10/11 SDK via filesystem
//! enumeration. Lets soldr-managed `cargo build` / `cargo test`
//! invocations succeed from a plain PowerShell, eliminating the
//! downstream `$env:LIB` workaround documented in issue #1079.
//!
//! ## Detect-then-download (soldr#1079, soldr#2292)
//!
//! This module covers both halves now. **Detect-host** probes the
//! host's VS install via `vswhere.exe` and validates the discovered
//! toolset (see [`is_compatible_vc_tools_version`]) before trusting
//! it. **Download-fallback** (soldr#2292) kicks in only when the host
//! probe comes back empty or the discovered toolset fails that
//! compatibility check: it materializes a soldr-managed MSVC bundle
//! from the soldr-toolchain catalogue (see
//! `soldr_fetch::fetch::msvc_toolset`) and synthesizes the same env
//! from the bundle root. Probe-first, download-only-when-missing —
//! the common case (a compatible host VS install) never touches the
//! network.
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
//!   validate compatibility (major/minor + real `cl.exe` on disk).
//!   Compatible → probe SDK → write `LIB`/`INCLUDE`/`PATH`/`LIBPATH`
//!   from the host install. Missing or incompatible → materialize the
//!   soldr-managed MSVC bundle and synthesize the same env vars from
//!   its root instead (SDK is still probed from the host either way —
//!   SDK fallback is out of scope for soldr#2292). If the download
//!   also fails, the error names both the host-probe outcome and the
//!   download failure, plus the `SOLDR_MSVC_DISCOVERY=off` escape
//!   hatch.

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

/// Where a [`MsvcHostLayout`]'s MSVC tools came from. Changes how
/// [`MsvcHostLayout::synthesize_env_x64`] locates the tools directory
/// under `vs_install` — see that method for the two shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsvcToolsSource {
    /// `vs_install` is a Visual Studio install root (e.g. `...\2022\
    /// Community`); the tools directory is `VC\Tools\MSVC\<version>\`
    /// underneath it.
    Host,
    /// `vs_install` IS the tools directory already — the root of a
    /// soldr-managed MSVC bundle extracted from the soldr-toolchain
    /// catalogue (soldr#2292), whose layout mirrors a real
    /// `VC\Tools\MSVC\<version>\` directory directly (`bin\Hostx64\
    /// x64\cl.exe`, `include\...`, `lib\x64\...`).
    ManagedBundle,
}

/// Resolved MSVC + Windows SDK layout. Constructed either by
/// [`discover_msvc_layout`] / [`probe_host_toolset`] from the host
/// filesystem, or from a materialized soldr-managed MSVC bundle
/// (soldr#2292); transformed into [`MsvcHostEnv`] via
/// [`MsvcHostLayout::synthesize_env_x64`] — no I/O in the synthesis
/// step so the pure transformation is unit-testable without a real VS
/// install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsvcHostLayout {
    pub vs_install: PathBuf,
    pub vc_tools_version: String,
    pub sdk_root: PathBuf,
    pub sdk_version: String,
    pub source: MsvcToolsSource,
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
    /// soldr#2292: neither the host probe nor the managed-bundle
    /// download produced a usable MSVC toolset. Names both failures
    /// so the user isn't left guessing which side broke, plus the
    /// opt-out escape hatch for unusual setups this heuristic trips
    /// on.
    #[error(
        "no usable MSVC toolset: host probe failed ({host_reason}); managed MSVC bundle \
         download also failed ({download_error}). Set SOLDR_MSVC_DISCOVERY=off to skip \
         soldr's automatic MSVC discovery and configure LIB/INCLUDE/PATH yourself \
         (e.g. from a Developer Command Prompt)."
    )]
    HostAndDownloadFailed {
        host_reason: String,
        download_error: String,
    },
}

// ---------------------------------------------------------------------------
// soldr#2292: host-vs-download compatibility decision
// ---------------------------------------------------------------------------

/// Minimum `VCToolsVersion` major component soldr accepts as a
/// compatible host toolset. MSVC toolset versions ship as
/// `MAJOR.MINOR.PATCH` (e.g. `14.44.35207`, read verbatim from
/// `Microsoft.VCToolsVersion.default.txt`). Major `14` spans every
/// v14x ABI generation (VS2015 through VS2022); [`MSVC_COMPAT_MIN_MINOR`]
/// narrows that down to the actual floor soldr accepts.
pub const MSVC_COMPAT_MAJOR: u32 = 14;

/// Minimum `VCToolsVersion` minor component soldr accepts, paired
/// with [`MSVC_COMPAT_MAJOR`]. `30` is the first VS2022 minor series
/// (the v143 toolset ABI); later VS2022 updates (17.10+) ship `40+`
/// (v144). Anything below `30` is VS2019 or older (v142 and earlier) —
/// soldr treats that as incompatible rather than probe-and-accept,
/// because soldr's managed MSVC bundle (`MANAGED_MSVC_VERSION` in
/// `soldr_fetch::fetch::msvc_toolset`, currently `14.44.35207`) is a
/// v143/v144-family toolset, and mixing import libs / linker behavior
/// across major toolset generations is a known source of subtle
/// ODR/ABI breakage. See soldr#2292.
pub const MSVC_COMPAT_MIN_MINOR: u32 = 30;

/// Parse a `VCToolsVersion` string (`MAJOR.MINOR.PATCH`, e.g.
/// `14.44.35207`) into its numeric components. Returns `None` for
/// anything that doesn't parse as at least `MAJOR.MINOR`.
fn parse_vc_tools_version(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.trim().split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// "cl.exe indicates a compatible expected toolset" (soldr#2292):
/// `true` iff `major == 14 && minor >= 30` — see
/// [`MSVC_COMPAT_MAJOR`] / [`MSVC_COMPAT_MIN_MINOR`] for the
/// reasoning. An unparseable version string is treated as
/// incompatible (fail closed — prefer a download over trusting a
/// toolset version string soldr doesn't understand).
pub fn is_compatible_vc_tools_version(version: &str) -> bool {
    match parse_vc_tools_version(version) {
        Some((major, minor)) => major == MSVC_COMPAT_MAJOR && minor >= MSVC_COMPAT_MIN_MINOR,
        None => false,
    }
}

/// Path to `cl.exe` for a given VS install root + `VCToolsVersion`,
/// following the fixed `VC\Tools\MSVC\<version>\bin\Hostx64\x64\`
/// layout VS ships. Does not check existence — callers probe with
/// `.is_file()` (see [`probe_host_toolset`]); this is a pure path
/// builder so it stays unit-testable without a VS install.
pub fn hostx64_cl_exe_path(vs_install: &Path, vc_tools_version: &str) -> PathBuf {
    vs_install
        .join("VC")
        .join("Tools")
        .join("MSVC")
        .join(vc_tools_version)
        .join("bin")
        .join("Hostx64")
        .join("x64")
        .join("cl.exe")
}

/// Outcome of probing the host for a Visual Studio C++ toolset,
/// decoupled from the Windows SDK probe (soldr#2292's compatibility
/// decision doesn't need the SDK — only [`is_compatible_vc_tools_version`]
/// and cl.exe's on-disk presence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostProbeOutcome {
    /// vswhere resolved a VS install with a readable `VCToolsVersion`.
    /// `cl_exe_exists` is a real filesystem probe, not an assumption —
    /// see soldr#2292 requirement 2.
    Found {
        vs_install: PathBuf,
        vc_tools_version: String,
        cl_exe_exists: bool,
    },
    /// No usable host toolset. Carries a human-readable reason (the
    /// `Display` of whichever [`MsvcDetectionError`] the probe hit).
    NotFound(String),
}

/// Reason the resolution fell through to the soldr-managed MSVC
/// bundle download. `Display` is the log-line / error-message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadReason {
    /// No host VS install (or no VCToolsVersion) was found.
    HostMissing(String),
    /// A host VS install was found, but its `VCToolsVersion` fails
    /// [`is_compatible_vc_tools_version`].
    HostIncompatible { version: String },
    /// A host VS install was found with a compatible-looking version
    /// string, but `cl.exe` isn't actually at the expected path —
    /// treated as "missing", not "compatible" (soldr#2292 requirement 2).
    ClExeMissing { version: String },
}

impl std::fmt::Display for DownloadReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadReason::HostMissing(reason) => {
                write!(f, "no host MSVC toolset found ({reason})")
            }
            DownloadReason::HostIncompatible { version } => write!(
                f,
                "host MSVC toolset {version} is incompatible (need {major}.{minor}+)",
                major = MSVC_COMPAT_MAJOR,
                minor = MSVC_COMPAT_MIN_MINOR
            ),
            DownloadReason::ClExeMissing { version } => write!(
                f,
                "host MSVC toolset {version} resolved but cl.exe is missing on disk"
            ),
        }
    }
}

/// The three-way outcome of soldr#2292's probe-first decision:
/// already satisfied, use the host toolset as-is, or fall back to the
/// soldr-managed download. Pure function of `already_in_env` +
/// [`HostProbeOutcome`] — no I/O, fully unit-testable, and runs on
/// every platform (the inputs are already-resolved values, not live
/// probes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MsvcResolution {
    /// Caller is already in a working MSVC env (Developer Prompt or
    /// equivalent) — no-op, don't probe or download anything.
    AlreadyInEnv,
    /// Host probe found a compatible toolset with a real `cl.exe` —
    /// use it, no download.
    UseHost,
    /// Host probe missing, incompatible, or missing `cl.exe` — fall
    /// back to the soldr-managed bundle.
    Download(DownloadReason),
}

/// The pure decision function itself. Probe-first: only reaches
/// `Download` when the host probe didn't produce a directly-usable
/// toolset. See module docs for the full ordering contract.
pub fn decide_msvc_resolution(already_in_env: bool, host: &HostProbeOutcome) -> MsvcResolution {
    if already_in_env {
        return MsvcResolution::AlreadyInEnv;
    }
    match host {
        HostProbeOutcome::Found {
            vc_tools_version,
            cl_exe_exists,
            ..
        } => {
            if !*cl_exe_exists {
                MsvcResolution::Download(DownloadReason::ClExeMissing {
                    version: vc_tools_version.clone(),
                })
            } else if is_compatible_vc_tools_version(vc_tools_version) {
                MsvcResolution::UseHost
            } else {
                MsvcResolution::Download(DownloadReason::HostIncompatible {
                    version: vc_tools_version.clone(),
                })
            }
        }
        HostProbeOutcome::NotFound(reason) => {
            MsvcResolution::Download(DownloadReason::HostMissing(reason.clone()))
        }
    }
}

impl MsvcHostLayout {
    /// The MSVC tools directory (contains `bin\`, `include\`, `lib\`),
    /// resolved according to [`MsvcToolsSource`]. No filesystem
    /// access.
    fn tools_dir(&self) -> PathBuf {
        match self.source {
            MsvcToolsSource::Host => self
                .vs_install
                .join("VC")
                .join("Tools")
                .join("MSVC")
                .join(&self.vc_tools_version),
            MsvcToolsSource::ManagedBundle => self.vs_install.clone(),
        }
    }

    /// Pure transformation from a discovered layout to the env vars
    /// `vcvars64.bat` writes for an x64 host targeting x64. No
    /// filesystem access — fully unit-testable.
    pub fn synthesize_env_x64(&self) -> MsvcHostEnv {
        let msvc = self.tools_dir();
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
            self.sdk_root
                .join("bin")
                .join(&self.sdk_version)
                .join("x64"),
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
/// commonly "no VS install with VC++ tools" AND the soldr-managed
/// bundle download also failed (soldr#2292).
///
/// Async because the soldr#2292 download-fallback path needs it; the
/// hot path (host probe finds a compatible toolset, or the caller is
/// already in a Developer Prompt) never awaits anything network-bound.
pub async fn ensure_msvc_env_for_native(target_triple: &str) -> Result<bool, MsvcDetectionError> {
    ensure_msvc_env_for_native_in(target_triple, &std::env::current_dir().unwrap_or_default()).await
}

/// Same as [`ensure_msvc_env_for_native`] but with the project root
/// passed in explicitly so tests can exercise the `.cargo/config.toml`
/// linker-pin probe without `chdir`-ing the process. Issue #1105.
pub async fn ensure_msvc_env_for_native_in(
    target_triple: &str,
    project_dir: &Path,
) -> Result<bool, MsvcDetectionError> {
    if !cfg!(target_os = "windows") {
        return Ok(false);
    }
    if !target_triple.ends_with("-pc-windows-msvc") {
        return Ok(false);
    }
    if opted_out() {
        return Ok(false);
    }
    // soldr#1105: when the project pins `linker = "rust-lld.exe"` in
    // .cargo/config.toml, `link.exe` is irrelevant — rust-lld is the
    // linker and it relies SOLELY on `LIB` to find the import libs.
    // The conservative `already_in_msvc_env()` check (LIB && link.exe)
    // therefore misses this scenario: a Developer Command Prompt
    // sets both, but a plain PowerShell with a half-loaded env from
    // a previous failed attempt may have LIB but no link.exe — in
    // which case we should defer to the user's LIB regardless of
    // link.exe. For rust-lld-pinned projects, defer iff LIB is set
    // (link.exe presence does not matter).
    let pins_rust_lld = project_pins_rust_lld_for_msvc(project_dir);
    let lib_already_set = std::env::var_os("LIB")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let should_skip = if pins_rust_lld {
        lib_already_set
    } else {
        already_in_msvc_env()
    };
    if should_skip {
        // Developer-Prompt / already-configured path: identical to
        // pre-soldr#2292 behavior — no probe, no download, zero-diff.
        return Ok(false);
    }

    // soldr#2292 probe-first decision: probe the host, decide, and
    // only download when the host probe didn't produce a directly
    // usable toolset.
    let host_probe = probe_host_toolset();
    let layout = match decide_msvc_resolution(false, &host_probe) {
        MsvcResolution::AlreadyInEnv => {
            // Unreachable in practice: `already_in_env` is hard-coded
            // `false` here because the real "already configured" case
            // is handled by the `should_skip` early return above,
            // before any host probe runs.
            return Ok(false);
        }
        MsvcResolution::UseHost => {
            let HostProbeOutcome::Found {
                vs_install,
                vc_tools_version,
                ..
            } = host_probe
            else {
                unreachable!("MsvcResolution::UseHost implies HostProbeOutcome::Found");
            };
            let (sdk_root, sdk_version) = probe_windows_sdk()?;
            MsvcHostLayout {
                vs_install,
                vc_tools_version,
                sdk_root,
                sdk_version,
                source: MsvcToolsSource::Host,
            }
        }
        MsvcResolution::Download(reason) => {
            eprintln!(
                "soldr: {reason} — materializing the soldr-managed MSVC \
                 {version} bundle instead",
                version = crate::fetch::msvc_toolset::MANAGED_MSVC_VERSION,
            );
            let bundle_root = download_managed_msvc_bundle()
                .await
                .map_err(|download_err| MsvcDetectionError::HostAndDownloadFailed {
                    host_reason: reason.to_string(),
                    download_error: download_err,
                })?;
            let cl_exe = crate::fetch::msvc_toolset::cl_exe(&bundle_root);
            if !cl_exe.is_file() {
                return Err(MsvcDetectionError::HostAndDownloadFailed {
                    host_reason: reason.to_string(),
                    download_error: format!(
                        "managed MSVC bundle at {} has no {} (bundle layout drift?)",
                        bundle_root.display(),
                        cl_exe.display()
                    ),
                });
            }
            let (sdk_root, sdk_version) = probe_windows_sdk()?;
            MsvcHostLayout {
                vs_install: bundle_root,
                vc_tools_version: crate::fetch::msvc_toolset::MANAGED_MSVC_VERSION.to_string(),
                sdk_root,
                sdk_version,
                source: MsvcToolsSource::ManagedBundle,
            }
        }
    };
    let env = layout.synthesize_env_x64();
    apply_to_process(&env);
    Ok(true)
}

/// Probe the host for a Visual Studio C++ toolset, decoupled from the
/// Windows SDK probe (soldr#2292's compatibility decision only needs
/// [`is_compatible_vc_tools_version`] plus cl.exe's on-disk presence —
/// SDK resolution is unchanged / out of scope). Never fails: every
/// probe step that would error instead folds into
/// [`HostProbeOutcome::NotFound`] with the underlying reason, because
/// "no host toolset" is an expected, handled outcome here, not an
/// exceptional one.
pub fn probe_host_toolset() -> HostProbeOutcome {
    let vswhere = vswhere_path();
    if !vswhere.is_file() {
        return HostProbeOutcome::NotFound(
            MsvcDetectionError::VswhereNotFound(vswhere).to_string(),
        );
    }
    let install = match run_vswhere_install_path(&vswhere) {
        Ok(p) => p,
        Err(e) => return HostProbeOutcome::NotFound(e.to_string()),
    };
    let vc_tools_version = match read_vc_tools_version(&install) {
        Ok(v) => v,
        Err(e) => return HostProbeOutcome::NotFound(e.to_string()),
    };
    let cl_exe_exists = hostx64_cl_exe_path(&install, &vc_tools_version).is_file();
    HostProbeOutcome::Found {
        vs_install: install,
        vc_tools_version,
        cl_exe_exists,
    }
}

/// Materialize the soldr-managed MSVC bundle (soldr#2292) for the
/// current host and return its root. Thin wrapper over
/// `soldr_fetch::fetch::msvc_toolset::ensure_msvc_bundle` that resolves
/// `SoldrPaths` and stringifies the error, since [`MsvcDetectionError`]
/// carries download failures as `String` (it lives in soldr-cli, which
/// depends on soldr-fetch's `SoldrError` only transitively here).
async fn download_managed_msvc_bundle() -> Result<PathBuf, String> {
    let paths = crate::core::SoldrPaths::new().map_err(|e| e.to_string())?;
    let host = crate::fetch::cmake_tools::current_host_triple();
    crate::fetch::msvc_toolset::ensure_msvc_bundle(&paths, host)
        .await
        .map_err(|e| e.to_string())
}

/// Walk from `cwd` up to the filesystem root looking for a cargo
/// config file (`.cargo/config.toml`, falling back to the legacy
/// `.cargo/config` if no `.toml` form is present at the same level).
/// Returns `true` as soon as any of them contains a `linker` field
/// pointing at `rust-lld` for a windows-msvc target. Issue #1105 —
/// the rust-lld pin is the smoking gun that the project needs
/// soldr's MSVC SDK env injection even when other heuristics would
/// short-circuit.
pub fn project_pins_rust_lld_for_msvc(cwd: &Path) -> bool {
    for ancestor in cwd.ancestors() {
        let cargo_dir = ancestor.join(".cargo");
        for cfg in [cargo_dir.join("config.toml"), cargo_dir.join("config")] {
            if cfg.is_file() {
                if let Ok(contents) = std::fs::read_to_string(&cfg) {
                    if config_pins_rust_lld(&contents) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Pure-function helper for [`project_pins_rust_lld_for_msvc`].
/// Parses a cargo config TOML string and returns `true` when any
/// `[target.<X>]` table whose key mentions `windows` has a `linker`
/// field that resolves to `rust-lld` (case-insensitive, with or
/// without the `.exe` suffix, with or without a leading path).
///
/// Lives in `msvc_host` (not a generic cargo-config helper) because
/// it intentionally only fires for the windows-msvc case — that is
/// the only place rust-lld needs `LIB` injection.
pub fn config_pins_rust_lld(toml_str: &str) -> bool {
    let value: toml::Value = match toml_str.parse() {
        Ok(v) => v,
        Err(_) => return false,
    };
    let target = match value.as_table().and_then(|t| t.get("target")) {
        Some(toml::Value::Table(t)) => t,
        _ => return false,
    };
    for (key, sub) in target {
        // Filter to target keys that could plausibly cover windows-msvc.
        // Accept literal triples (`x86_64-pc-windows-msvc`,
        // `aarch64-pc-windows-msvc`), `cfg(windows)` predicates, and
        // anything else containing `windows` (defensive — cargo config
        // accepts a variety of cfg-strings).
        let key_lc = key.to_ascii_lowercase();
        if !(key_lc.contains("windows") || key_lc.contains("msvc")) {
            continue;
        }
        let linker = sub
            .as_table()
            .and_then(|t| t.get("linker"))
            .and_then(|v| v.as_str());
        let Some(linker) = linker else { continue };
        if linker_is_rust_lld(linker) {
            return true;
        }
    }
    false
}

fn linker_is_rust_lld(linker: &str) -> bool {
    let trimmed = linker.trim().trim_matches('"');
    let lower = trimmed.to_ascii_lowercase();
    if lower == "rust-lld" || lower == "rust-lld.exe" {
        return true;
    }
    // Accept paths ending in rust-lld / rust-lld.exe. Normalize both
    // separators because the project's `.cargo/config.toml` may use
    // Windows backslashes even when the test (or a Linux Docker
    // harness, per CLAUDE.md issue #1105 rule) is running on Linux —
    // `std::path::Path::file_name` only recognizes `/` on Linux.
    let normalized = lower.replace('\\', "/");
    let bare = normalized.rsplit('/').next().unwrap_or("");
    bare == "rust-lld" || bare == "rust-lld.exe"
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
        source: MsvcToolsSource::Host,
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

    // cfg-gated to Windows: the assertions look for backslash-joined
    // path segments (`VC\Tools\MSVC\...`) which only appear when
    // `PathBuf::display()` runs on Windows. The function under test
    // (`synthesize_env_x64`) uses `PathBuf::display()` and so produces
    // platform-native separators — on Linux that's forward slashes.
    // Issue #1105 surfaced this by running the suite under Docker
    // Linux for the first time. Fixing `synthesize_env_x64` itself to
    // always emit backslashes is a separate concern; for now we keep
    // the existing Windows-only coverage and ensure the test no longer
    // panics on Linux CI / Linux Docker runs.
    #[cfg(target_os = "windows")]
    timed_test!(
        synthesize_env_x64_builds_canonical_paths,
        Duration::from_secs(5),
        {
            let layout = MsvcHostLayout {
                vs_install: PathBuf::from(
                    r"C:\Program Files (x86)\Microsoft Visual Studio\2019\Community",
                ),
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
    );

    // soldr#2292: a ManagedBundle-sourced layout treats `vs_install` as
    // the tools dir directly — no `VC\Tools\MSVC\<version>` nesting.
    #[cfg(target_os = "windows")]
    timed_test!(
        synthesize_env_x64_managed_bundle_uses_root_directly,
        Duration::from_secs(5),
        {
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
    );

    // ---- soldr#2292: compatibility check -----------------------------

    timed_test!(
        is_compatible_vc_tools_version_accepts_v143_and_v144,
        Duration::from_secs(5),
        {
            // This machine's actual host toolset (must stay the
            // no-download hot path).
            assert!(is_compatible_vc_tools_version("14.44.35207"));
            // Lower bound of the accepted minor series.
            assert!(is_compatible_vc_tools_version("14.30.30705"));
            // A later v144 update.
            assert!(is_compatible_vc_tools_version("14.41.34120"));
        }
    );

    timed_test!(
        is_compatible_vc_tools_version_rejects_pre_vs2022_and_junk,
        Duration::from_secs(5),
        {
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
    );

    timed_test!(hostx64_cl_exe_path_layout, Duration::from_secs(5), {
        let p = hostx64_cl_exe_path(Path::new(r"C:\VS"), "14.44.35207");
        assert!(
            p.ends_with(r"bin\Hostx64\x64\cl.exe") || p.ends_with("bin/Hostx64/x64/cl.exe"),
            "{}",
            p.display()
        );
        assert!(p.to_string_lossy().contains("14.44.35207"));
    });

    // ---- soldr#2292: decide_msvc_resolution pure decision function ---

    timed_test!(
        decide_msvc_resolution_already_in_env_short_circuits,
        Duration::from_secs(5),
        {
            let host = HostProbeOutcome::NotFound("irrelevant".into());
            assert_eq!(
                decide_msvc_resolution(true, &host),
                MsvcResolution::AlreadyInEnv
            );
        }
    );

    timed_test!(
        decide_msvc_resolution_uses_host_when_compatible_and_cl_exe_present,
        Duration::from_secs(5),
        {
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
    );

    timed_test!(
        decide_msvc_resolution_downloads_when_host_missing,
        Duration::from_secs(5),
        {
            let host = HostProbeOutcome::NotFound("no VS install".into());
            let resolution = decide_msvc_resolution(false, &host);
            assert!(matches!(
                resolution,
                MsvcResolution::Download(DownloadReason::HostMissing(_))
            ));
        }
    );

    timed_test!(
        decide_msvc_resolution_downloads_when_host_incompatible,
        Duration::from_secs(5),
        {
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
    );

    timed_test!(
        decide_msvc_resolution_downloads_when_cl_exe_missing_even_if_version_ok,
        Duration::from_secs(5),
        {
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
    );

    // soldr#2292: the real-world path an incompatible/no-VS host actually
    // hits today — the public MSVC bundle catalogue publication is on
    // hold (licensing), so `download_managed_msvc_bundle` fails too, and
    // `MsvcDetectionError::HostAndDownloadFailed` is what the user sees.
    // The message MUST name both failures plus the opt-out escape hatch —
    // "host probe failed, then download also failed, good luck" with no
    // actionable detail is exactly what soldr#2292 was filed to fix.
    timed_test!(
        host_and_download_failed_message_names_both_failures_and_escape_hatch,
        Duration::from_secs(5),
        {
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
    );

    timed_test!(
        vswhere_path_honors_override_env_var,
        Duration::from_secs(5),
        {
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
    );

    timed_test!(
        opted_out_recognizes_off_zero_false,
        Duration::from_secs(5),
        {
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
    );

    timed_test!(
        pick_highest_sdk_version_skips_partial_installs,
        Duration::from_secs(10),
        {
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
    );

    timed_test!(
        pick_highest_sdk_version_errors_on_empty_install,
        Duration::from_secs(10),
        {
            let tmp = tempfile::tempdir().expect("tempdir");
            // Only Include/ exists, but no version subdirs with um+ucrt.
            std::fs::create_dir_all(tmp.path().join("Include")).unwrap();
            std::fs::create_dir_all(tmp.path().join("Include").join("10.0.0.0").join("um"))
                .unwrap();
            // missing ucrt → should not qualify
            let err = pick_highest_sdk_version(tmp.path()).expect_err("should fail");
            assert!(matches!(err, MsvcDetectionError::NoSdkVersion(_)));
        }
    );

    timed_test!(
        ensure_msvc_env_for_native_is_noop_on_non_msvc_target,
        Duration::from_secs(5),
        {
            // This test runs on every platform and proves the early-out
            // for non-MSVC targets. Even on Windows, asking for a
            // non-MSVC target must skip discovery.
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            let applied = rt
                .block_on(ensure_msvc_env_for_native("x86_64-unknown-linux-gnu"))
                .expect("noop should not error");
            assert!(!applied, "linux-gnu target must not trigger MSVC discovery");
        }
    );

    // ---- soldr#1105: .cargo/config.toml rust-lld linker probe -----

    timed_test!(
        config_pins_rust_lld_detects_bare_rust_lld_exe,
        Duration::from_secs(5),
        {
            let toml = r#"
[target.x86_64-pc-windows-msvc]
linker = "rust-lld.exe"
"#;
            assert!(
                config_pins_rust_lld(toml),
                "should match bare rust-lld.exe linker pin"
            );
        }
    );

    timed_test!(
        config_pins_rust_lld_detects_path_to_rust_lld,
        Duration::from_secs(5),
        {
            let toml = r#"
[target.aarch64-pc-windows-msvc]
linker = "C:/Users/me/.rustup/toolchains/stable/bin/rust-lld.exe"
"#;
            assert!(
                config_pins_rust_lld(toml),
                "should match absolute path to rust-lld.exe"
            );
        }
    );

    // Backslash-path variant — TOML uses `\\` escapes so the actual
    // string the parser sees is `C:\Users\...\rust-lld.exe`. This
    // case must work even when the test runs on Linux (Path::file_name
    // doesn't recognize `\` on Linux); the implementation normalizes
    // separators before splitting.
    timed_test!(
        config_pins_rust_lld_detects_backslash_path_on_any_host,
        Duration::from_secs(5),
        {
            let toml = "
[target.x86_64-pc-windows-msvc]
linker = \"C:\\\\Users\\\\me\\\\.rustup\\\\toolchains\\\\stable\\\\bin\\\\rust-lld.exe\"
";
            assert!(
                config_pins_rust_lld(toml),
                "should match Windows-backslash path even on Linux hosts"
            );
        }
    );

    timed_test!(
        config_pins_rust_lld_detects_no_exe_suffix,
        Duration::from_secs(5),
        {
            let toml = r#"
[target.x86_64-pc-windows-msvc]
linker = "rust-lld"
"#;
            assert!(
                config_pins_rust_lld(toml),
                "should match rust-lld without .exe"
            );
        }
    );

    timed_test!(
        config_pins_rust_lld_ignores_link_exe,
        Duration::from_secs(5),
        {
            let toml = r#"
[target.x86_64-pc-windows-msvc]
linker = "link.exe"
"#;
            assert!(
                !config_pins_rust_lld(toml),
                "should NOT match the default link.exe — soldr#1105 only cares about rust-lld pins"
            );
        }
    );

    timed_test!(
        config_pins_rust_lld_ignores_non_windows_targets,
        Duration::from_secs(5),
        {
            let toml = r#"
[target.x86_64-unknown-linux-gnu]
linker = "rust-lld"
"#;
            assert!(
                !config_pins_rust_lld(toml),
                "rust-lld pinned for a linux target must not trigger MSVC env injection"
            );
        }
    );

    timed_test!(
        config_pins_rust_lld_handles_malformed_toml_silently,
        Duration::from_secs(5),
        {
            let toml = "not = valid = toml = at = all";
            assert!(
                !config_pins_rust_lld(toml),
                "malformed cargo config must be a soft-skip, not a panic — soldr#1105"
            );
        }
    );

    timed_test!(
        project_pins_rust_lld_walks_ancestors,
        Duration::from_secs(10),
        {
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
    );

    timed_test!(
        project_pins_rust_lld_no_config_is_false,
        Duration::from_secs(5),
        {
            let tmp = tempfile::tempdir().expect("tempdir");
            assert!(
                !project_pins_rust_lld_for_msvc(tmp.path()),
                "empty directory tree must return false"
            );
        }
    );

    #[cfg(not(target_os = "windows"))]
    timed_test!(
        ensure_msvc_env_for_native_is_noop_on_non_windows_host,
        Duration::from_secs(5),
        {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            let applied = rt
                .block_on(ensure_msvc_env_for_native("x86_64-pc-windows-msvc"))
                .expect("noop should not error");
            assert!(!applied, "non-windows host must not attempt MSVC discovery");
        }
    );

    // -------------------------------------------------------------------
    // Windows-only end-to-end probe. Skips gracefully on hosts without
    // a VS install (CI lanes without VC++ workload).
    // -------------------------------------------------------------------
    #[cfg(target_os = "windows")]
    timed_test!(
        discover_msvc_layout_on_developer_machine_finds_real_link_exe,
        Duration::from_secs(30),
        {
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
    );
}
