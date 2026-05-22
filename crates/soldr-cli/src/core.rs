use serde::Deserialize;
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
};
use thiserror::Error;

pub const CARGO_HOME_ENV_VAR: &str = "CARGO_HOME";
pub const RUSTUP_HOME_ENV_VAR: &str = "RUSTUP_HOME";
const RUSTUP_TOOLCHAIN_ENV_VAR: &str = "RUSTUP_TOOLCHAIN";

// ---------------------------------------------------------------------------
// Target triple detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Env {
    Gnu,
    Musl,
    Msvc,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetTriple {
    pub arch: Arch,
    pub os: Os,
    pub env: Env,
}

impl TargetTriple {
    /// Detect the active target for the current project context.
    pub fn detect() -> Result<Self, SoldrError> {
        let current_dir = std::env::current_dir().ok();
        Self::detect_from_dir(current_dir.as_deref())
    }

    pub fn detect_in_dir(start_dir: &Path) -> Result<Self, SoldrError> {
        Self::detect_from_dir(Some(start_dir))
    }

    fn detect_from_dir(start_dir: Option<&Path>) -> Result<Self, SoldrError> {
        if let Some(triple) = read_explicit_target_override(start_dir) {
            return Self::from_triple(&triple);
        }

        if cfg!(target_os = "windows") {
            return Ok(Self {
                arch: compile_time_arch()?,
                os: Os::Windows,
                env: Env::Msvc,
            });
        }

        if let Some(triple) = detect_runtime_rustc_triple(start_dir) {
            return Self::from_triple(&triple);
        }

        Self::from_triple(&compile_time_fallback_triple()?)
    }

    pub fn from_triple(triple: &str) -> Result<Self, SoldrError> {
        let triple = triple.trim();
        let arch = if triple.starts_with("x86_64-") {
            Arch::X86_64
        } else if triple.starts_with("aarch64-") {
            Arch::Aarch64
        } else {
            return Err(SoldrError::UnsupportedPlatform(format!(
                "unsupported target arch in triple: {triple}"
            )));
        };

        let (os, env) = if triple.contains("-pc-windows-msvc") {
            (Os::Windows, Env::Msvc)
        } else if triple.contains("-pc-windows-gnu") {
            (Os::Windows, Env::Gnu)
        } else if triple.contains("-unknown-linux-musl") {
            (Os::Linux, Env::Musl)
        } else if triple.contains("-unknown-linux-gnu") {
            (Os::Linux, Env::Gnu)
        } else if triple.contains("-apple-darwin") {
            (Os::MacOs, Env::None)
        } else {
            return Err(SoldrError::UnsupportedPlatform(format!(
                "unsupported target triple: {triple}"
            )));
        };

        Ok(Self { arch, os, env })
    }

    /// Full Rust target triple, e.g. `x86_64-pc-windows-msvc`.
    pub fn triple(&self) -> String {
        let arch = match self.arch {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        };
        match (&self.os, &self.env) {
            (Os::Windows, Env::Msvc) => format!("{arch}-pc-windows-msvc"),
            (Os::Windows, Env::Gnu) => format!("{arch}-pc-windows-gnu"),
            (Os::Linux, Env::Gnu) => format!("{arch}-unknown-linux-gnu"),
            (Os::Linux, Env::Musl) => format!("{arch}-unknown-linux-musl"),
            (Os::MacOs, _) => format!("{arch}-apple-darwin"),
            _ => format!("{arch}-unknown-unknown"),
        }
    }

    pub fn archive_ext(&self) -> &'static str {
        match self.os {
            Os::Windows => "zip",
            _ => "tar.gz",
        }
    }

    pub fn binary_ext(&self) -> &'static str {
        match self.os {
            Os::Windows => ".exe",
            _ => "",
        }
    }
}

impl std::fmt::Display for TargetTriple {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.triple())
    }
}

#[derive(Debug, Deserialize)]
struct RustToolchainFile {
    toolchain: Option<RustToolchainSection>,
    #[serde(default)]
    soldr: Option<SoldrManifestSection>,
}

#[derive(Debug, Deserialize)]
struct RustToolchainSection {
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    components: Option<Vec<String>>,
    #[serde(default)]
    targets: Option<Vec<String>>,
    #[serde(default)]
    profile: Option<String>,
}

/// Top-level `[soldr]` section of `rust-toolchain.toml`. Carries
/// soldr-specific developer-tooling declarations that aren't part of
/// rustup's own schema. Currently surfaces the `[soldr.plugins]` table
/// (see [`PluginSpec`]) which `soldr toolchain prepare` translates into
/// `cargo install` invocations.
#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq)]
pub struct SoldrManifestSection {
    #[serde(default)]
    pub plugins: BTreeMap<String, PluginSpec>,
}

/// One entry in `[soldr.plugins]`. The key is the cargo crate name
/// (e.g. `cargo-nextest`); the value is either a bare version string or
/// a detailed table that mirrors `cargo install`'s relevant flags.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum PluginSpec {
    /// `cargo-nextest = "0.9"` — just a version requirement. The literal
    /// `"*"` is treated as "any version" and skips `--version`.
    Version(String),
    /// `cargo-zigbuild = { version = "0.18", locked = true, ... }`.
    /// Every field is optional; omitted fields mean "don't pass the
    /// corresponding cargo install flag".
    Detailed {
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        locked: Option<bool>,
        #[serde(default)]
        features: Option<Vec<String>>,
        #[serde(default)]
        no_default_features: Option<bool>,
    },
}

/// Parsed view of a project's `rust-toolchain.toml`. All fields are
/// optional so callers can treat a missing file or missing `[toolchain]`
/// section the same as a fully-populated section whose fields happen to
/// be unset. Returned by [`read_rust_toolchain_manifest`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RustToolchainManifest {
    pub channel: Option<String>,
    pub components: Option<Vec<String>>,
    pub targets: Option<Vec<String>>,
    pub profile: Option<String>,
    /// Parsed `[soldr]` section. `None` when the file omits it
    /// entirely so callers can short-circuit cleanly.
    pub soldr: Option<SoldrManifestSection>,
}

/// Read `rust-toolchain.toml` from `workspace_root` (non-recursive — the
/// caller is expected to already point at the directory containing the
/// manifest, mirroring how cargo resolves the file). A missing file is
/// not an error; an empty `RustToolchainManifest` is returned so callers
/// can branch on `manifest.channel.is_none()` without juggling IO error
/// kinds. Malformed TOML or unreadable files surface as
/// [`SoldrError::Other`].
pub fn read_rust_toolchain_manifest(
    workspace_root: &Path,
) -> Result<RustToolchainManifest, SoldrError> {
    let path = workspace_root.join("rust-toolchain.toml");
    if !path.exists() {
        return Ok(RustToolchainManifest::default());
    }
    let text = std::fs::read_to_string(&path).map_err(|err| {
        SoldrError::Other(format!(
            "failed to read rust-toolchain.toml at {}: {err}",
            path.display()
        ))
    })?;
    let parsed: RustToolchainFile = toml::from_str(&text).map_err(|err| {
        SoldrError::Other(format!(
            "failed to parse rust-toolchain.toml at {}: {err}",
            path.display()
        ))
    })?;
    let soldr = parsed.soldr;
    let Some(section) = parsed.toolchain else {
        return Ok(RustToolchainManifest {
            soldr,
            ..RustToolchainManifest::default()
        });
    };
    Ok(RustToolchainManifest {
        channel: section
            .channel
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        components: section.components,
        targets: section.targets,
        profile: section
            .profile
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        soldr,
    })
}

#[derive(Debug, Deserialize)]
struct CargoConfigFile {
    build: Option<CargoBuildSection>,
}

#[derive(Debug, Deserialize)]
struct CargoBuildSection {
    target: Option<String>,
}

fn read_explicit_target_override(start_dir: Option<&Path>) -> Option<String> {
    find_in_ancestors(start_dir, ".cargo/config.toml")
        .and_then(read_cargo_config_target)
        .or_else(|| {
            find_in_ancestors(start_dir, ".cargo/config").and_then(read_cargo_config_target)
        })
        .or_else(|| {
            find_in_ancestors(start_dir, "rust-toolchain.toml").and_then(read_toolchain_target)
        })
}

fn read_cargo_config_target(path: PathBuf) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let config: CargoConfigFile = toml::from_str(&text).ok()?;
    config.build?.target
}

fn read_toolchain_target(path: PathBuf) -> Option<String> {
    let workspace_root = path.parent()?;
    let manifest = read_rust_toolchain_manifest(workspace_root).ok()?;
    let supported = manifest
        .targets?
        .into_iter()
        .filter(|target| TargetTriple::from_triple(target).is_ok())
        .collect::<Vec<_>>();

    choose_target_override(supported)
}

fn choose_target_override(targets: Vec<String>) -> Option<String> {
    if targets.len() == 1 {
        return targets.into_iter().next();
    }

    let host_os = compile_time_host_os().ok()?;
    let host_arch = compile_time_arch().ok()?;
    let matching_host = targets
        .into_iter()
        .filter_map(|target| {
            let parsed = TargetTriple::from_triple(&target).ok()?;
            if parsed.os == host_os && parsed.arch == host_arch {
                Some(target)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    if matching_host.len() == 1 {
        matching_host.into_iter().next()
    } else {
        None
    }
}

fn find_in_ancestors(start_dir: Option<&Path>, relative_path: &str) -> Option<PathBuf> {
    let mut current = start_dir?.to_path_buf();
    loop {
        let candidate = current.join(relative_path);
        if candidate.exists() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ImplicitToolchainHomes {
    cargo_home: Option<PathBuf>,
    rustup_home: Option<PathBuf>,
}

impl ImplicitToolchainHomes {
    fn from_env(
        start_dir: Option<&Path>,
        cargo_home_env: Option<&OsStr>,
        rustup_home_env: Option<&OsStr>,
    ) -> Self {
        Self {
            cargo_home: if cargo_home_env.is_none() {
                find_dir_in_ancestors(start_dir, ".cargo")
            } else {
                None
            },
            rustup_home: if rustup_home_env.is_none() {
                find_dir_in_ancestors(start_dir, ".rustup")
            } else {
                None
            },
        }
    }

    fn detect(start_dir: Option<&Path>) -> Self {
        Self::from_env(
            start_dir,
            std::env::var_os(CARGO_HOME_ENV_VAR).as_deref(),
            std::env::var_os(RUSTUP_HOME_ENV_VAR).as_deref(),
        )
    }

    fn apply_to_command(&self, command: &mut Command) {
        if let Some(cargo_home) = &self.cargo_home {
            command.env(CARGO_HOME_ENV_VAR, cargo_home);
        }
        if let Some(rustup_home) = &self.rustup_home {
            command.env(RUSTUP_HOME_ENV_VAR, rustup_home);
        }
    }
}

fn cargo_home_bin_dir(start_dir: Option<&Path>) -> Option<PathBuf> {
    non_empty_env_path(std::env::var_os(CARGO_HOME_ENV_VAR).as_deref())
        .map(|path| path.join("bin"))
        .or_else(|| {
            ImplicitToolchainHomes::detect(start_dir)
                .cargo_home
                .map(|path| path.join("bin"))
        })
}

fn rustup_home_dir(start_dir: Option<&Path>) -> Option<PathBuf> {
    non_empty_env_path(std::env::var_os(RUSTUP_HOME_ENV_VAR).as_deref())
        .or_else(|| ImplicitToolchainHomes::detect(start_dir).rustup_home)
}

fn rustup_toolchain_bin_dir(start_dir: Option<&Path>) -> Option<PathBuf> {
    let toolchains_dir = rustup_home_dir(start_dir)?.join("toolchains");
    let mut candidates = std::fs::read_dir(toolchains_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .map(|path| path.join("bin"))
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();

    if candidates.len() == 1 {
        candidates.pop()
    } else {
        None
    }
}

fn path_bin_dir(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|value| {
        std::env::split_paths(&value).find(|dir| find_executable_in_dir(dir, tool).is_some())
    })
}

fn rustup_toolchain_env_is_explicit(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

fn executable_exists(path: &Path) -> bool {
    path.is_file()
}

#[cfg(windows)]
fn windows_pathexts() -> Vec<String> {
    let pathext = std::env::var_os("PATHEXT")
        .and_then(|value| value.into_string().ok())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
    pathext
        .split(';')
        .map(str::trim)
        .filter(|ext| !ext.is_empty())
        .map(|ext| ext.to_ascii_lowercase())
        .collect()
}

fn find_executable_in_dir(dir: &Path, tool: &str) -> Option<PathBuf> {
    let candidate = dir.join(tool);
    if executable_exists(&candidate) {
        return Some(candidate);
    }

    #[cfg(windows)]
    {
        let ext = candidate
            .extension()
            .and_then(OsStr::to_str)
            .map(|ext| format!(".{}", ext.to_ascii_lowercase()));
        if ext.is_some() {
            return None;
        }

        for suffix in windows_pathexts() {
            let suffixed = dir.join(format!("{tool}{suffix}"));
            if executable_exists(&suffixed) {
                return Some(suffixed);
            }
        }
    }

    None
}

fn find_dir_in_ancestors(start_dir: Option<&Path>, relative_path: &str) -> Option<PathBuf> {
    let mut current = start_dir?.to_path_buf();
    loop {
        let candidate = current.join(relative_path);
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub fn apply_implicit_toolchain_homes(command: &mut Command, start_dir: Option<&Path>) {
    ImplicitToolchainHomes::detect(start_dir).apply_to_command(command);
}

pub fn suppress_windows_console_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    #[cfg(not(windows))]
    let _ = command;
}

pub fn probe_toolchain_binary(tool: &str, start_dir: Option<&Path>) -> Option<PathBuf> {
    if rustup_toolchain_env_is_explicit(std::env::var_os(RUSTUP_TOOLCHAIN_ENV_VAR).as_deref()) {
        return None;
    }

    rustup_toolchain_bin_dir(start_dir)
        .and_then(|dir| find_executable_in_dir(&dir, tool))
        .or_else(|| {
            cargo_home_bin_dir(start_dir).and_then(|dir| find_executable_in_dir(&dir, tool))
        })
        .or_else(|| path_bin_dir(tool).and_then(|dir| find_executable_in_dir(&dir, tool)))
}

fn detect_runtime_rustc_triple(start_dir: Option<&Path>) -> Option<String> {
    let rustc = resolve_runtime_rustc(start_dir)?;
    let mut command = std::process::Command::new(rustc);
    apply_implicit_toolchain_homes(&mut command, start_dir);
    suppress_windows_console_window(&mut command);
    if let Some(start_dir) = start_dir {
        command.current_dir(start_dir);
    }
    let output = command.args(["--print", "target-triple"]).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let triple = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if triple.is_empty() {
        None
    } else {
        Some(triple)
    }
}

fn resolve_runtime_rustc(start_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(rustc) = probe_toolchain_binary("rustc", start_dir) {
        return Some(rustc);
    }

    let mut rustup = std::process::Command::new("rustup");
    apply_implicit_toolchain_homes(&mut rustup, start_dir);
    suppress_windows_console_window(&mut rustup);
    if let Some(start_dir) = start_dir {
        rustup.current_dir(start_dir);
    }
    let rustup_output = rustup.args(["which", "rustc"]).output().ok()?;
    if rustup_output.status.success() {
        let path = String::from_utf8_lossy(&rustup_output.stdout)
            .trim()
            .to_string();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    Some(PathBuf::from("rustc"))
}

fn compile_time_arch() -> Result<Arch, SoldrError> {
    if cfg!(target_arch = "x86_64") {
        Ok(Arch::X86_64)
    } else if cfg!(target_arch = "aarch64") {
        Ok(Arch::Aarch64)
    } else {
        Err(SoldrError::UnsupportedPlatform(format!(
            "unsupported arch: {}",
            std::env::consts::ARCH
        )))
    }
}

fn compile_time_host_os() -> Result<Os, SoldrError> {
    if cfg!(target_os = "windows") {
        Ok(Os::Windows)
    } else if cfg!(target_os = "macos") {
        Ok(Os::MacOs)
    } else if cfg!(target_os = "linux") {
        Ok(Os::Linux)
    } else {
        Err(SoldrError::UnsupportedPlatform(format!(
            "unsupported OS: {}",
            std::env::consts::OS
        )))
    }
}

fn compile_time_fallback_triple() -> Result<String, SoldrError> {
    let arch = match compile_time_arch()? {
        Arch::X86_64 => "x86_64",
        Arch::Aarch64 => "aarch64",
    };
    let triple = match compile_time_host_os()? {
        Os::Windows => format!("{arch}-pc-windows-msvc"),
        Os::MacOs => format!("{arch}-apple-darwin"),
        Os::Linux => format!("{arch}-unknown-linux-gnu"),
    };
    Ok(triple)
}

// ---------------------------------------------------------------------------
// Paths - ~/.soldr/ layout
// ---------------------------------------------------------------------------

pub const SOLDR_CACHE_DIR_ENV_VAR: &str = "SOLDR_CACHE_DIR";

pub struct SoldrPaths {
    pub root: PathBuf,
    pub bin: PathBuf,
    /// Directory holding the pinned-zccache install (issue #426). Lives at
    /// the user's home-anchored `~/.soldr/bin/` independent of
    /// `SOLDR_CACHE_DIR`, so a pin registered against the default cache dir
    /// remains visible to builds that re-root with `SOLDR_CACHE_DIR=/x`.
    /// For synthetic-root construction (`SoldrPaths::with_root`, used by
    /// tests) this collapses into `bin` so test isolation is preserved.
    pub pinned_bin: PathBuf,
    pub cache: PathBuf,
    pub config_file: PathBuf,
}

impl SoldrPaths {
    pub fn new() -> Result<Self, SoldrError> {
        Self::from_root_env_value(std::env::var_os(SOLDR_CACHE_DIR_ENV_VAR).as_deref())
    }

    fn from_root_env_value(value: Option<&OsStr>) -> Result<Self, SoldrError> {
        let env_root = soldr_root_from_env_var(value).transpose()?;
        Self::from_env_root_and_home(env_root, home_dir().map(|h| h.join(".soldr")))
    }

    /// Inner constructor split out so tests can inject explicit `env_root`
    /// + `home_root` values without mutating the process env. Other tests
    /// in this binary read HOME / USERPROFILE too, so global env mutation
    /// in a unit test races with parallel cases.
    fn from_env_root_and_home(
        env_root: Option<PathBuf>,
        home_root: Result<PathBuf, SoldrError>,
    ) -> Result<Self, SoldrError> {
        let root = match (&env_root, &home_root) {
            (Some(env), _) => env.clone(),
            (None, Ok(home)) => home.clone(),
            (None, Err(_)) => return Err(SoldrError::NoHomeDir),
        };
        // The pin must NOT move when SOLDR_CACHE_DIR overrides the rest of
        // the install root (issue #426 — pinned binaries are a machine-level
        // user preference, not per-cache-dir state). When no $HOME / no
        // USERPROFILE is available — e.g. a headless CI sandbox that relies
        // entirely on SOLDR_CACHE_DIR — fall back to the env-rooted bin/ so
        // the pin lives somewhere accessible; that env does lose the
        // cross-SOLDR_CACHE_DIR survival property, but it matches today's
        // behavior so nothing regresses.
        let pinned_bin = match &home_root {
            Ok(home) => home.join("bin"),
            Err(_) => root.join("bin"),
        };
        Ok(Self {
            bin: root.join("bin"),
            pinned_bin,
            cache: root.join("cache"),
            config_file: root.join("config.toml"),
            root,
        })
    }

    pub fn with_root(root: PathBuf) -> Self {
        let bin = root.join("bin");
        Self {
            bin: bin.clone(),
            // Tests use synthetic roots and rely on the pin landing inside
            // the test workspace. Collapse pinned_bin into bin so isolation
            // works; production goes through `from_root_env_value` which
            // anchors pinned_bin at the user's home.
            pinned_bin: bin,
            cache: root.join("cache"),
            config_file: root.join("config.toml"),
            root,
        }
    }

    pub fn ensure_dirs(&self) -> Result<(), SoldrError> {
        std::fs::create_dir_all(&self.bin)?;
        std::fs::create_dir_all(&self.pinned_bin)?;
        std::fs::create_dir_all(&self.cache)?;
        Ok(())
    }

    /// Load the soldr config from `config.toml` if it exists. Missing
    /// or malformed files yield `Default::default()` so callers can
    /// proceed with reasonable defaults.
    pub fn load_config(&self) -> SoldrConfig {
        SoldrConfig::load(&self.config_file)
    }
}

// ---------------------------------------------------------------------------
// Config — `~/.soldr/config.toml`
// ---------------------------------------------------------------------------

/// Top-level soldr configuration. Currently carries the `[gc]` section
/// (issue #234), the `[auto_gc]` section (issue #323) and an optional
/// top-level `linker = "..."` field (issue #285); future sections can
/// be added freely.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SoldrConfig {
    #[serde(default)]
    pub gc: GcConfig,
    /// Automatic GC under disk pressure (issue #323).
    #[serde(default)]
    pub auto_gc: AutoGcConfig,
    /// User-configured linker choice for `soldr cargo ...`. Mirrors the
    /// `SOLDR_LINKER` env var; the env var wins when both are set.
    /// Accepted values: `default`, `ld`, `mold`, `rust-lld`, `fast`.
    #[serde(default)]
    pub linker: Option<String>,
}

/// `gc` section of `config.toml`.
///
/// ```toml
/// [gc]
/// allowlist_roots = ["~/dev", "/work/repos"]
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GcConfig {
    /// Roots that `soldr gc` will consider for reclamation. If unset
    /// (or empty after expansion), callers should fall back to
    /// `~/dev`. `~` is expanded to the user's home directory.
    #[serde(default)]
    pub allowlist_roots: Option<Vec<String>>,
}

/// `auto_gc` section of `config.toml` (issue #323 — automatic GC under
/// disk pressure).
///
/// ```toml
/// [auto_gc]
/// enabled = true            # opt-out; on by default
/// trigger_free_gb = 20      # start GC when free space < this
/// target_free_gb  = 30      # stop GC when free space >= this
/// min_age_secs    = 3600    # never touch files modified in this window
/// ```
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AutoGcConfig {
    /// Whether soldr should automatically reclaim disk space when free
    /// space on a soldr-relevant volume drops below `trigger_free_gb`.
    /// Defaults to `true` (opt-out).
    #[serde(default = "AutoGcConfig::default_enabled")]
    pub enabled: bool,
    /// Auto-GC fires when free space on a soldr-relevant volume drops
    /// below this threshold (in GiB).
    #[serde(default = "AutoGcConfig::default_trigger_free_gb")]
    pub trigger_free_gb: u64,
    /// Auto-GC stops escalating once free space reaches this number of
    /// GiB on the affected volume. Must be >= `trigger_free_gb`.
    #[serde(default = "AutoGcConfig::default_target_free_gb")]
    pub target_free_gb: u64,
    /// Floor applied uniformly to every age-based filter used by
    /// auto-GC. Files / directories modified within this many seconds
    /// of "now" are never touched.
    #[serde(default = "AutoGcConfig::default_min_age_secs")]
    pub min_age_secs: u64,
}

impl AutoGcConfig {
    pub const fn default_enabled() -> bool {
        true
    }
    pub const fn default_trigger_free_gb() -> u64 {
        20
    }
    pub const fn default_target_free_gb() -> u64 {
        30
    }
    pub const fn default_min_age_secs() -> u64 {
        3600
    }
}

impl Default for AutoGcConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            trigger_free_gb: Self::default_trigger_free_gb(),
            target_free_gb: Self::default_target_free_gb(),
            min_age_secs: Self::default_min_age_secs(),
        }
    }
}

impl SoldrConfig {
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_default()
    }
}

/// Public accessor for the soldr home directory used by the `gc`
/// allowlist default (`~/dev`). Returns `Err` when no home dir can be
/// resolved.
pub fn user_home_dir() -> Result<PathBuf, SoldrError> {
    home_dir()
}

/// Resolve `$CARGO_HOME` if set and non-empty, otherwise `~/.cargo`.
/// Returns `None` when neither resolves cleanly.
pub fn resolve_cargo_home() -> Option<PathBuf> {
    if let Some(path) = non_empty_env_path(std::env::var_os(CARGO_HOME_ENV_VAR).as_deref()) {
        return Some(path);
    }
    home_dir().ok().map(|home| home.join(".cargo"))
}

/// Resolve `$RUSTUP_HOME` if set and non-empty, otherwise `~/.rustup`.
/// Returns `None` when neither resolves cleanly.
pub fn resolve_rustup_home() -> Option<PathBuf> {
    if let Some(path) = non_empty_env_path(std::env::var_os(RUSTUP_HOME_ENV_VAR).as_deref()) {
        return Some(path);
    }
    home_dir().ok().map(|home| home.join(".rustup"))
}

/// Expand `~` and `~/...` strings to absolute paths under the user's
/// home directory. Other inputs pass through unchanged.
pub fn expand_user_home(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~") {
        if let Ok(home) = home_dir() {
            let trimmed = rest.trim_start_matches(['/', '\\']);
            if trimmed.is_empty() {
                return home;
            }
            return home.join(trimmed);
        }
    }
    PathBuf::from(input)
}

fn soldr_root_from_env_var(value: Option<&OsStr>) -> Option<Result<PathBuf, SoldrError>> {
    non_empty_env_path(value).map(Ok)
}

fn non_empty_env_path(value: Option<&OsStr>) -> Option<PathBuf> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

fn home_dir() -> Result<PathBuf, SoldrError> {
    #[cfg(windows)]
    {
        if let Ok(p) = std::env::var("USERPROFILE") {
            return Ok(PathBuf::from(p));
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(p) = std::env::var("HOME") {
            return Ok(PathBuf::from(p));
        }
    }
    Err(SoldrError::NoHomeDir)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum SoldrError {
    #[error("unsupported platform: {0}")]
    UnsupportedPlatform(String),
    #[error("no home directory found")]
    NoHomeDir,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("network error: {0}")]
    Network(String),
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("archive error: {0}")]
    Archive(String),
    #[error("{0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, fs, sync::Mutex};
    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn fake_script_path(dir: &Path, name: &str) -> PathBuf {
        #[cfg(windows)]
        {
            dir.join(format!("{name}.bat"))
        }
        #[cfg(not(windows))]
        {
            dir.join(name)
        }
    }

    fn write_fake_script(path: &Path, script: &str) {
        fs::write(path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
    }

    #[cfg(windows)]
    fn fake_rustc_script(triple: &str) -> String {
        format!(
            "@echo off\r\n\
             if \"%~1\"==\"--print\" if \"%~2\"==\"target-triple\" (\r\n\
             echo {triple}\r\n\
             exit /b 0\r\n\
             )\r\n\
             echo unexpected rustc args %* 1>&2\r\n\
             exit /b 1\r\n"
        )
    }

    #[cfg(not(windows))]
    fn fake_rustc_script(triple: &str) -> String {
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"--print\" ] && [ \"$2\" = \"target-triple\" ]; then\n\
                 printf '%s\\n' '{triple}'\n\
                 exit 0\n\
             fi\n\
             echo \"unexpected rustc args: $*\" >&2\n\
             exit 1\n"
        )
    }

    #[cfg(windows)]
    fn fake_failing_rustup_script(log_path: &Path) -> String {
        format!(
            "@echo off\r\n\
             echo rustup %*>>\"{}\"\r\n\
             echo rustup should not have been invoked 1>&2\r\n\
             exit /b 1\r\n",
            log_path.display()
        )
    }

    #[cfg(not(windows))]
    fn fake_failing_rustup_script(log_path: &Path) -> String {
        format!(
            "#!/bin/sh\n\
             echo \"rustup $*\" >> \"{}\"\n\
             echo \"rustup should not have been invoked\" >&2\n\
             exit 1\n",
            log_path.display()
        )
    }

    fn assert_rustup_not_invoked(log_path: &Path) {
        let log = fs::read_to_string(log_path).unwrap_or_default();
        assert!(
            log.trim().is_empty(),
            "direct tool resolution should bypass rustup entirely: {log}"
        );
    }

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn test_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn test_detect_target() {
        let t = TargetTriple::detect().unwrap();
        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        {
            assert_eq!(t.os, Os::Windows);
            assert_eq!(t.env, Env::Msvc);
            assert_eq!(t.arch, Arch::X86_64);
            assert_eq!(t.triple(), "x86_64-pc-windows-msvc");
        }
        #[cfg(target_os = "macos")]
        assert_eq!(t.os, Os::MacOs);
        #[cfg(target_os = "linux")]
        assert_eq!(t.os, Os::Linux);
        let _ = t.triple();
    }

    #[test]
    fn test_triple_strings() {
        let t = TargetTriple {
            arch: Arch::X86_64,
            os: Os::Windows,
            env: Env::Msvc,
        };
        assert_eq!(t.triple(), "x86_64-pc-windows-msvc");
        assert_eq!(t.archive_ext(), "zip");
        assert_eq!(t.binary_ext(), ".exe");

        let t = TargetTriple {
            arch: Arch::Aarch64,
            os: Os::MacOs,
            env: Env::None,
        };
        assert_eq!(t.triple(), "aarch64-apple-darwin");
        assert_eq!(t.archive_ext(), "tar.gz");
        assert_eq!(t.binary_ext(), "");
    }

    #[test]
    fn test_paths() {
        let paths = SoldrPaths::from_root_env_value(None).unwrap();
        assert!(paths.root.ends_with(".soldr"));
        assert!(paths.bin.ends_with("bin"));
        assert!(paths.cache.ends_with("cache"));
    }

    #[test]
    fn pinned_bin_survives_soldr_cache_dir_override() {
        // Issue #426: SOLDR_CACHE_DIR overrides the install root (and bin/
        // and cache/ along with it), but the pin must keep living at the
        // home-anchored ~/.soldr/bin/ so a pin registered against the
        // default cache dir stays visible to builds that re-root the cache.
        //
        // We exercise `from_env_root_and_home` directly so the test
        // doesn't have to mutate HOME / USERPROFILE — that would race with
        // every other case in this binary that reads them.
        let home_root = PathBuf::from("/synthetic-home/.soldr");
        let env_root = PathBuf::from("/synthetic-cache-dir");
        let paths =
            SoldrPaths::from_env_root_and_home(Some(env_root.clone()), Ok(home_root.clone()))
                .unwrap();

        assert_eq!(paths.root, env_root, "root follows SOLDR_CACHE_DIR");
        assert_eq!(
            paths.bin,
            env_root.join("bin"),
            "bin/ follows SOLDR_CACHE_DIR (managed binaries stay per-cache-dir)"
        );
        assert_eq!(
            paths.pinned_bin,
            home_root.join("bin"),
            "pinned_bin stays at $HOME/.soldr/bin/ regardless of SOLDR_CACHE_DIR"
        );
        assert_ne!(
            paths.bin, paths.pinned_bin,
            "the whole point of the fix: bin and pinned_bin diverge under env override"
        );
    }

    #[test]
    fn pinned_bin_equals_bin_when_no_cache_dir_override() {
        // No SOLDR_CACHE_DIR → root falls back to home → bin == pinned_bin.
        // No behavior change in the dominant case.
        let home_root = PathBuf::from("/synthetic-home/.soldr");
        let paths = SoldrPaths::from_env_root_and_home(None, Ok(home_root.clone())).unwrap();
        assert_eq!(paths.root, home_root);
        assert_eq!(paths.bin, home_root.join("bin"));
        assert_eq!(paths.pinned_bin, paths.bin);
    }

    #[test]
    fn pinned_bin_falls_back_to_env_root_when_no_home_available() {
        // Headless sandbox case: SOLDR_CACHE_DIR set but no $HOME /
        // USERPROFILE. pinned_bin must NOT panic — it falls back to the
        // env-rooted bin/ (loses cross-SOLDR_CACHE_DIR survival, but
        // matches today's behavior so nothing regresses).
        let env_root = PathBuf::from("/sandbox/.soldr");
        let paths =
            SoldrPaths::from_env_root_and_home(Some(env_root.clone()), Err(SoldrError::NoHomeDir))
                .unwrap();
        assert_eq!(paths.pinned_bin, env_root.join("bin"));
        assert_eq!(paths.pinned_bin, paths.bin);
    }

    #[test]
    fn pinned_bin_collapses_into_bin_with_synthetic_root() {
        // `SoldrPaths::with_root` is the test-construction path used by
        // integration tests. The pin dir must live inside the synthetic
        // root so test workspaces can register pins without escaping to
        // the host's home dir.
        let root = PathBuf::from("/tmp/synthetic-soldr-root");
        let paths = SoldrPaths::with_root(root.clone());
        assert_eq!(paths.bin, root.join("bin"));
        assert_eq!(paths.pinned_bin, paths.bin);
    }

    #[test]
    fn auto_gc_config_default_round_trip() {
        let cfg: SoldrConfig = toml::from_str("[auto_gc]\n").unwrap();
        assert_eq!(cfg.auto_gc, AutoGcConfig::default());
        assert!(cfg.auto_gc.enabled);
        assert_eq!(cfg.auto_gc.trigger_free_gb, 20);
        assert_eq!(cfg.auto_gc.target_free_gb, 30);
        assert_eq!(cfg.auto_gc.min_age_secs, 3600);
    }

    #[test]
    fn auto_gc_config_custom_values_parse() {
        let toml_text = r#"
[auto_gc]
enabled = false
trigger_free_gb = 10
target_free_gb = 50
min_age_secs = 7200
"#;
        let cfg: SoldrConfig = toml::from_str(toml_text).unwrap();
        assert!(!cfg.auto_gc.enabled);
        assert_eq!(cfg.auto_gc.trigger_free_gb, 10);
        assert_eq!(cfg.auto_gc.target_free_gb, 50);
        assert_eq!(cfg.auto_gc.min_age_secs, 7200);
    }

    #[test]
    fn missing_auto_gc_section_uses_defaults() {
        let cfg: SoldrConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.auto_gc, AutoGcConfig::default());
    }

    #[test]
    fn soldr_root_override_uses_env_path() {
        let root = soldr_root_from_env_var(Some(OsStr::new("C:\\temp\\soldr-cache-root")))
            .unwrap()
            .unwrap();
        assert_eq!(root, PathBuf::from("C:\\temp\\soldr-cache-root"));
    }

    #[test]
    fn soldr_root_override_ignores_empty_env() {
        assert!(soldr_root_from_env_var(Some(OsStr::new(""))).is_none());
    }

    #[test]
    fn detects_target_override_from_cargo_config() {
        let dir = tempdir().unwrap();
        let cargo_dir = dir.path().join(".cargo");
        std::fs::create_dir_all(&cargo_dir).unwrap();
        std::fs::write(
            cargo_dir.join("config.toml"),
            "[build]\ntarget = \"x86_64-unknown-linux-musl\"\n",
        )
        .unwrap();

        let target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        assert_eq!(target.triple(), "x86_64-unknown-linux-musl");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn defaults_to_msvc_without_explicit_override() {
        let dir = tempdir().unwrap();
        let target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        assert_eq!(target.triple(), "x86_64-pc-windows-msvc");
    }

    #[test]
    fn detects_gnu_override_from_rust_toolchain_toml() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\ntargets = [\"x86_64-pc-windows-gnu\"]\n",
        )
        .unwrap();

        let target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        assert_eq!(target.triple(), "x86_64-pc-windows-gnu");
    }

    #[test]
    fn detects_msvc_override_from_rust_toolchain_toml() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\ntargets = [\"x86_64-pc-windows-msvc\"]\n",
        )
        .unwrap();

        let target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        assert_eq!(target.triple(), "x86_64-pc-windows-msvc");
    }

    #[test]
    fn detects_macos_override_from_rust_toolchain_toml() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\ntargets = [\"aarch64-apple-darwin\"]\n",
        )
        .unwrap();

        let target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        assert_eq!(target.triple(), "aarch64-apple-darwin");
    }

    #[test]
    fn detects_override_from_parent_directory() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\ntargets = [\"aarch64-apple-darwin\"]\n",
        )
        .unwrap();
        let nested = dir.path().join("nested").join("child");
        std::fs::create_dir_all(&nested).unwrap();

        let target = TargetTriple::detect_in_dir(&nested).unwrap();
        assert_eq!(target.triple(), "aarch64-apple-darwin");
    }

    #[test]
    fn ignores_ambiguous_toolchain_target_list() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\ntargets = [\"x86_64-pc-windows-msvc\", \"aarch64-pc-windows-msvc\"]\n",
        )
        .unwrap();

        let _target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        #[cfg(target_os = "windows")]
        assert_eq!(_target.triple(), "x86_64-pc-windows-msvc");
    }

    #[test]
    fn implicit_toolchain_homes_detect_repo_local_directories_from_ancestors() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".cargo")).unwrap();
        std::fs::create_dir_all(dir.path().join(".rustup")).unwrap();
        let nested = dir.path().join("workspace").join("crate");
        std::fs::create_dir_all(&nested).unwrap();

        let homes = ImplicitToolchainHomes::from_env(Some(nested.as_path()), None, None);
        assert_eq!(homes.cargo_home, Some(dir.path().join(".cargo")));
        assert_eq!(homes.rustup_home, Some(dir.path().join(".rustup")));
    }

    #[test]
    fn implicit_toolchain_homes_only_fill_missing_env_vars() {
        let dir = tempdir().unwrap();
        let repo_cargo_home = dir.path().join(".cargo");
        let repo_rustup_home = dir.path().join(".rustup");
        std::fs::create_dir_all(&repo_cargo_home).unwrap();
        std::fs::create_dir_all(&repo_rustup_home).unwrap();

        let homes = ImplicitToolchainHomes::from_env(
            Some(dir.path()),
            Some(OsStr::new("C:/explicit-cargo-home")),
            None,
        );
        assert_eq!(homes.cargo_home, None);
        assert_eq!(homes.rustup_home, Some(repo_rustup_home));
    }

    #[test]
    fn implicit_toolchain_homes_treat_empty_env_as_explicit() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".cargo")).unwrap();
        std::fs::create_dir_all(dir.path().join(".rustup")).unwrap();

        let homes = ImplicitToolchainHomes::from_env(
            Some(dir.path()),
            Some(OsStr::new("")),
            Some(OsStr::new("")),
        );
        assert_eq!(homes, ImplicitToolchainHomes::default());
    }

    #[test]
    fn explicit_rustup_toolchain_env_disables_direct_probe() {
        assert!(rustup_toolchain_env_is_explicit(Some(OsStr::new("stable"))));
        assert!(!rustup_toolchain_env_is_explicit(Some(OsStr::new(""))));
        assert!(!rustup_toolchain_env_is_explicit(None));
    }

    #[test]
    fn resolve_runtime_rustc_prefers_path_before_rustup() {
        let _env_lock = lock_env();
        let dir = tempdir().unwrap();
        let tool_dir = dir.path().join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        let log_path = dir.path().join("rustup.log");
        let rustc = fake_script_path(&tool_dir, "rustc");
        let rustup = fake_script_path(&tool_dir, "rustup");
        write_fake_script(&rustc, &fake_rustc_script("x86_64-unknown-linux-gnu"));
        write_fake_script(&rustup, &fake_failing_rustup_script(&log_path));

        let _path = EnvVarGuard::set("PATH", std::env::join_paths([&tool_dir]).unwrap());
        let _cargo_home = EnvVarGuard::remove(CARGO_HOME_ENV_VAR);
        let _rustup_home = EnvVarGuard::remove(RUSTUP_HOME_ENV_VAR);
        let _rustup_toolchain = EnvVarGuard::remove(RUSTUP_TOOLCHAIN_ENV_VAR);

        assert_eq!(resolve_runtime_rustc(None), Some(rustc));
        assert_rustup_not_invoked(&log_path);
    }

    #[test]
    fn suppress_windows_console_window_preserves_piped_output() {
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd");
            command.args(["/C", "echo soldr-no-window"]);
            command
        };

        #[cfg(not(windows))]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "printf soldr-no-window"]);
            command
        };

        suppress_windows_console_window(&mut command);
        let output = command.output().expect("failed to run child command");
        assert!(
            output.status.success(),
            "child command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("soldr-no-window"),
            "missing expected stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn resolve_runtime_rustc_prefers_explicit_rustup_home_toolchain_before_rustup() {
        let _env_lock = lock_env();
        let dir = tempdir().unwrap();
        let explicit_rustup_home = dir.path().join("explicit-rustup-home");
        let rustc = fake_script_path(
            &explicit_rustup_home
                .join("toolchains")
                .join("stable-test")
                .join("bin"),
            "rustc",
        );
        fs::create_dir_all(rustc.parent().unwrap()).unwrap();
        write_fake_script(&rustc, &fake_rustc_script("aarch64-apple-darwin"));

        let tool_dir = dir.path().join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        let log_path = dir.path().join("rustup.log");
        let rustup = fake_script_path(&tool_dir, "rustup");
        write_fake_script(&rustup, &fake_failing_rustup_script(&log_path));

        let _path = EnvVarGuard::set("PATH", OsStr::new(""));
        let _cargo_home = EnvVarGuard::remove(CARGO_HOME_ENV_VAR);
        let _rustup_home = EnvVarGuard::set(RUSTUP_HOME_ENV_VAR, &explicit_rustup_home);
        let _rustup_toolchain = EnvVarGuard::remove(RUSTUP_TOOLCHAIN_ENV_VAR);

        assert_eq!(resolve_runtime_rustc(None), Some(rustc));
        assert_rustup_not_invoked(&log_path);
    }

    #[test]
    fn resolve_runtime_rustc_prefers_repo_local_rustup_home_toolchain_before_rustup() {
        let _env_lock = lock_env();
        let dir = tempdir().unwrap();
        let nested = dir.path().join("workspace").join("crate");
        fs::create_dir_all(&nested).unwrap();

        let rustc = fake_script_path(
            &dir.path()
                .join(".rustup")
                .join("toolchains")
                .join("stable-test")
                .join("bin"),
            "rustc",
        );
        fs::create_dir_all(rustc.parent().unwrap()).unwrap();
        write_fake_script(&rustc, &fake_rustc_script("x86_64-pc-windows-msvc"));

        let _path = EnvVarGuard::set("PATH", OsStr::new(""));
        let _cargo_home = EnvVarGuard::remove(CARGO_HOME_ENV_VAR);
        let _rustup_home = EnvVarGuard::remove(RUSTUP_HOME_ENV_VAR);
        let _rustup_toolchain = EnvVarGuard::remove(RUSTUP_TOOLCHAIN_ENV_VAR);

        assert_eq!(resolve_runtime_rustc(Some(&nested)), Some(rustc));
    }

    #[test]
    fn resolve_runtime_rustc_prefers_repo_local_rustup_home_before_explicit_cargo_home_shim() {
        let _env_lock = lock_env();
        let dir = tempdir().unwrap();
        let nested = dir.path().join("workspace").join("crate");
        fs::create_dir_all(&nested).unwrap();

        let repo_local_rustc = fake_script_path(
            &dir.path()
                .join(".rustup")
                .join("toolchains")
                .join("stable-test")
                .join("bin"),
            "rustc",
        );
        fs::create_dir_all(repo_local_rustc.parent().unwrap()).unwrap();
        write_fake_script(
            &repo_local_rustc,
            &fake_rustc_script("x86_64-pc-windows-msvc"),
        );

        let explicit_cargo_home = dir.path().join("explicit-cargo-home");
        let shim_rustc = fake_script_path(&explicit_cargo_home.join("bin"), "rustc");
        fs::create_dir_all(shim_rustc.parent().unwrap()).unwrap();
        write_fake_script(&shim_rustc, &fake_rustc_script("x86_64-unknown-linux-gnu"));

        let tool_dir = dir.path().join("tools");
        fs::create_dir_all(&tool_dir).unwrap();
        let log_path = dir.path().join("rustup.log");
        let rustup = fake_script_path(&tool_dir, "rustup");
        write_fake_script(&rustup, &fake_failing_rustup_script(&log_path));

        let _path = EnvVarGuard::set("PATH", std::env::join_paths([&tool_dir]).unwrap());
        let _cargo_home = EnvVarGuard::set(CARGO_HOME_ENV_VAR, &explicit_cargo_home);
        let _rustup_home = EnvVarGuard::remove(RUSTUP_HOME_ENV_VAR);
        let _rustup_toolchain = EnvVarGuard::remove(RUSTUP_TOOLCHAIN_ENV_VAR);

        assert_eq!(resolve_runtime_rustc(Some(&nested)), Some(repo_local_rustc));
        assert_rustup_not_invoked(&log_path);
    }

    #[test]
    fn rust_toolchain_manifest_parses_full_section() {
        let dir = tempdir().unwrap();
        let manifest_path = dir.path().join("rust-toolchain.toml");
        fs::write(
            &manifest_path,
            "[toolchain]\n\
             channel = \"1.94.1\"\n\
             components = [\"clippy\", \"rustfmt\"]\n\
             targets = [\"x86_64-unknown-linux-musl\", \"aarch64-apple-darwin\"]\n\
             profile = \"minimal\"\n",
        )
        .unwrap();

        let manifest = read_rust_toolchain_manifest(dir.path()).unwrap();
        assert_eq!(manifest.channel.as_deref(), Some("1.94.1"));
        assert_eq!(
            manifest.components.as_deref(),
            Some(&["clippy".to_string(), "rustfmt".to_string()][..])
        );
        assert_eq!(
            manifest.targets.as_deref(),
            Some(
                &[
                    "x86_64-unknown-linux-musl".to_string(),
                    "aarch64-apple-darwin".to_string()
                ][..]
            )
        );
        assert_eq!(manifest.profile.as_deref(), Some("minimal"));
    }

    #[test]
    fn rust_toolchain_manifest_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let manifest = read_rust_toolchain_manifest(dir.path()).unwrap();
        assert_eq!(manifest, RustToolchainManifest::default());
        assert!(manifest.channel.is_none());
        assert!(manifest.components.is_none());
        assert!(manifest.targets.is_none());
        assert!(manifest.profile.is_none());
        assert!(manifest.soldr.is_none());
    }

    #[test]
    fn manifest_parses_soldr_plugins_section() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("rust-toolchain.toml"),
            "[toolchain]\n\
             channel = \"1.94.1\"\n\
             \n\
             [soldr.plugins]\n\
             cargo-nextest = \"0.9\"\n\
             cargo-zigbuild = { version = \"0.18\", locked = true }\n\
             cargo-deny = \"*\"\n",
        )
        .unwrap();

        let manifest = read_rust_toolchain_manifest(dir.path()).unwrap();
        let soldr = manifest.soldr.expect("expected [soldr] section to parse");
        assert_eq!(soldr.plugins.len(), 3);
        match soldr
            .plugins
            .get("cargo-nextest")
            .expect("cargo-nextest missing")
        {
            PluginSpec::Version(value) => assert_eq!(value, "0.9"),
            other => panic!("cargo-nextest should parse as Version(\"0.9\"), got {other:?}"),
        }
        match soldr
            .plugins
            .get("cargo-zigbuild")
            .expect("cargo-zigbuild missing")
        {
            PluginSpec::Detailed {
                version,
                locked,
                features,
                no_default_features,
            } => {
                assert_eq!(version.as_deref(), Some("0.18"));
                assert_eq!(*locked, Some(true));
                assert!(features.is_none());
                assert!(no_default_features.is_none());
            }
            other => panic!("cargo-zigbuild should parse as Detailed, got {other:?}"),
        }
        match soldr.plugins.get("cargo-deny").expect("cargo-deny missing") {
            PluginSpec::Version(value) => assert_eq!(value, "*"),
            other => panic!("cargo-deny should parse as Version(\"*\"), got {other:?}"),
        }
    }
}
