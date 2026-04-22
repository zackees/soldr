use serde::Deserialize;
use std::{
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
}
