use serde::Deserialize;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};
use thiserror::Error;

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
}

#[derive(Debug, Deserialize)]
struct RustToolchainSection {
    targets: Option<Vec<String>>,
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
    let text = std::fs::read_to_string(path).ok()?;
    let toolchain: RustToolchainFile = toml::from_str(&text).ok()?;
    let supported = toolchain
        .toolchain?
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

fn detect_runtime_rustc_triple(start_dir: Option<&Path>) -> Option<String> {
    let rustc = resolve_runtime_rustc(start_dir)?;
    let mut command = std::process::Command::new(rustc);
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
    let mut rustup = std::process::Command::new("rustup");
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
    pub cache: PathBuf,
    pub config_file: PathBuf,
}

impl SoldrPaths {
    pub fn new() -> Result<Self, SoldrError> {
        let root = soldr_root_from_env_var(std::env::var_os(SOLDR_CACHE_DIR_ENV_VAR).as_deref())
            .unwrap_or_else(|| home_dir().map(|home| home.join(".soldr")))?;
        Ok(Self::with_root(root))
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self {
            bin: root.join("bin"),
            cache: root.join("cache"),
            config_file: root.join("config.toml"),
            root,
        }
    }

    pub fn ensure_dirs(&self) -> Result<(), SoldrError> {
        std::fs::create_dir_all(&self.bin)?;
        std::fs::create_dir_all(&self.cache)?;
        Ok(())
    }
}

fn soldr_root_from_env_var(value: Option<&OsStr>) -> Option<Result<PathBuf, SoldrError>> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    Some(Ok(PathBuf::from(value)))
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
    use tempfile::tempdir;

    #[test]
    fn test_version() {
        assert_eq!(version(), "0.2.0-beta");
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
        let paths = SoldrPaths::new().unwrap();
        assert!(paths.root.ends_with(".soldr"));
        assert!(paths.bin.ends_with("bin"));
        assert!(paths.cache.ends_with("cache"));
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
}
