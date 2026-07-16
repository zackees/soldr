//! `SOLDR_LINKER` override (issue #285).
//!
//! Lets users pick the linker that `soldr cargo ...` injects for the active
//! build target. The choice can come from the `SOLDR_LINKER` env var or the
//! `linker = "..."` field in `~/.soldr/config.toml`; env wins.
//!
//! The selection is resolved into per-target Cargo env vars
//! (`CARGO_TARGET_<TRIPLE>_LINKER` / `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`) that
//! the cargo front door layers onto the spawned cargo process. The
//! existing wrapper cache key already accounts for `CARGO_TARGET_*_LINKER`
//! and `CARGO_TARGET_*_RUSTFLAGS` via the env hash, so no separate
//! invalidation hook is required.

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::str::FromStr;

const PEP517_LINKER_POLICY_ENV: &str = "SOLDR_PEP517_LINKER";
const PEP517_LINKER_FALLBACK_FILE: &str = "pep517-linker-fallback-v1.tsv";

/// User-facing linker choices accepted by `SOLDR_LINKER` and the
/// `linker = "..."` config field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkerChoice {
    /// Do nothing: leave whatever the rust-toolchain default is in place.
    Default,
    /// Use the platform's system linker (`ld` / `ld64` / `link.exe`). On
    /// every supported platform this is the platform default, so it is
    /// also a no-op injection.
    Ld,
    /// Use the [mold](https://github.com/rui314/mold) linker. Linux only.
    Mold,
    /// Use rustup's bundled `rust-lld`. Available on every supported
    /// platform.
    RustLld,
    /// Pick the fastest available linker per platform: mold on Linux if
    /// it is on `PATH`, otherwise rust-lld; rust-lld everywhere else.
    Fast,
}

impl FromStr for LinkerChoice {
    type Err = SoldrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Ok(LinkerChoice::Default);
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "default" => Ok(LinkerChoice::Default),
            "ld" => Ok(LinkerChoice::Ld),
            "mold" => Ok(LinkerChoice::Mold),
            "rust-lld" | "rustlld" | "rust_lld" => Ok(LinkerChoice::RustLld),
            "fast" => Ok(LinkerChoice::Fast),
            other => Err(SoldrError::Other(format!(
                "invalid SOLDR_LINKER value `{other}` (expected one of: default, ld, mold, rust-lld, fast)"
            ))),
        }
    }
}

/// The resolved per-target injection. `None` fields mean "do not set this
/// `CARGO_TARGET_<TRIPLE>_*` env var".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkerInjection {
    pub linker: Option<String>,
    pub rustflags: Option<String>,
}

/// State carried from maturin command preparation to its process runner.
/// `injected_env` contains only values soldr added, so an automatic retry can
/// remove them without clobbering an explicit project/caller linker setting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pep517LinkerState {
    pub automatic_fast: bool,
    pub explicit_fast: bool,
    pub cached_fallback: bool,
    pub cache_key: Option<String>,
    pub candidate: Option<String>,
    pub injected_env: Vec<String>,
}

impl Pep517LinkerState {
    pub fn should_retry(&self) -> bool {
        self.automatic_fast && !self.cached_fallback && !self.injected_env.is_empty()
    }

    pub fn clear_injected_env(&self, command: &mut Command) {
        for key in &self.injected_env {
            command.env_remove(key);
        }
    }
}

impl LinkerInjection {
    fn none() -> Self {
        Self::default()
    }

    fn clang_with_fuse(fuse: &str) -> Self {
        Self {
            linker: Some("clang".to_string()),
            rustflags: Some(format!("-C link-arg=-fuse-ld={fuse}")),
        }
    }

    fn rust_lld_msvc() -> Self {
        Self {
            linker: Some("rust-lld".to_string()),
            rustflags: None,
        }
    }
}

/// Resolve a `LinkerChoice` from (in order): the env var if set, the
/// config string if set, otherwise `Default`.
pub fn from_env_and_config(
    env: Option<&OsStr>,
    config: Option<&str>,
) -> Result<LinkerChoice, SoldrError> {
    if let Some(env) = env {
        let env = env
            .to_str()
            .ok_or_else(|| SoldrError::Other("SOLDR_LINKER is not valid UTF-8".to_string()))?;
        return LinkerChoice::from_str(env);
    }
    if let Some(config) = config {
        return LinkerChoice::from_str(config);
    }
    Ok(LinkerChoice::Default)
}

fn target_kind(target: &str) -> TargetKind {
    if target.contains("-windows-msvc") {
        TargetKind::WindowsMsvc
    } else if target.contains("-windows-gnu") {
        TargetKind::WindowsGnu
    } else if target.contains("-apple-") || target.contains("-darwin") {
        TargetKind::Apple
    } else if target.contains("-linux-") {
        TargetKind::Linux
    } else {
        TargetKind::Other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Linux,
    Apple,
    WindowsMsvc,
    WindowsGnu,
    Other,
}

/// Resolve a `LinkerChoice` plus an active target triple into the env
/// var injection that should be layered onto the cargo subprocess.
///
/// Errors when the choice is unsupported on the target (e.g.
/// `SOLDR_LINKER=mold` on macOS or Windows).
pub fn resolve_for_target(
    choice: LinkerChoice,
    target: &str,
) -> Result<LinkerInjection, SoldrError> {
    let mold_present = || mold_on_path();
    resolve_for_target_with_probe(choice, target, &mold_present)
}

/// Same as `resolve_for_target` but with the mold-on-PATH probe injected
/// so tests can exercise both branches of `fast` on Linux.
pub fn resolve_for_target_with_probe(
    choice: LinkerChoice,
    target: &str,
    mold_present: &dyn Fn() -> bool,
) -> Result<LinkerInjection, SoldrError> {
    let kind = target_kind(target);
    match choice {
        LinkerChoice::Default | LinkerChoice::Ld => Ok(LinkerInjection::none()),
        LinkerChoice::Mold => match kind {
            TargetKind::Linux => Ok(LinkerInjection::clang_with_fuse("mold")),
            _ => Err(SoldrError::Other(format!(
                "mold is not supported on `{target}`; use 'fast' for a portable fallback"
            ))),
        },
        LinkerChoice::RustLld => match kind {
            TargetKind::WindowsMsvc => Ok(LinkerInjection::rust_lld_msvc()),
            // Apple clang only accepts `-fuse-ld=lld` when the toolchain has
            // wired up a `ld64.lld` shim, and stock macOS toolchains do not.
            // Injecting `-fuse-ld=lld` breaks even `cc-rs` build-script
            // compilations (issue #509). Fall back to the platform default
            // linker silently on Apple targets.
            TargetKind::Apple => Ok(LinkerInjection::none()),
            TargetKind::Linux | TargetKind::WindowsGnu | TargetKind::Other => {
                Ok(LinkerInjection::clang_with_fuse("lld"))
            }
        },
        LinkerChoice::Fast => match kind {
            TargetKind::Linux => {
                if mold_present() {
                    Ok(LinkerInjection::clang_with_fuse("mold"))
                } else {
                    Ok(LinkerInjection::clang_with_fuse("lld"))
                }
            }
            TargetKind::WindowsMsvc => Ok(LinkerInjection::rust_lld_msvc()),
            // See the `RustLld` arm above — `-fuse-ld=lld` is not valid on
            // Apple clang and silently dropping to the platform default
            // keeps `SOLDR_LINKER=fast` portable across hosts (issue #509).
            TargetKind::Apple => Ok(LinkerInjection::none()),
            TargetKind::WindowsGnu | TargetKind::Other => {
                Ok(LinkerInjection::clang_with_fuse("lld"))
            }
        },
    }
}

/// Apply the automatic fast-linker policy used by the PEP backend. Direct
/// `soldr cargo` remains governed by `SOLDR_LINKER` / config.toml; the Python
/// backend opts into this policy with `SOLDR_PEP517_LINKER=auto`.
pub fn apply_pep517_override(
    command: &mut Command,
    target: &str,
    paths: &SoldrPaths,
) -> Result<Pep517LinkerState, SoldrError> {
    let config = paths.load_config();
    let explicit_env = std::env::var_os(crate::LINKER_ENV_VAR);
    let explicit_config = config.linker.as_deref();
    let project_target_linker_configured = project_target_config_value(target, "linker").is_some()
        || project_target_config_value(target, "rustflags").is_some();
    let automatic_fast = explicit_env.is_none()
        && explicit_config.is_none()
        && !project_target_linker_configured
        && std::env::var(PEP517_LINKER_POLICY_ENV)
            .ok()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("auto"));
    let choice = if automatic_fast {
        LinkerChoice::Fast
    } else {
        from_env_and_config(explicit_env.as_deref(), explicit_config)?
    };
    let injection = resolve_for_target(choice, target)?;
    let prefix = cargo_target_env_prefix(target);
    let linker_key = format!("CARGO_TARGET_{prefix}_LINKER");
    let rustflags_key = format!("CARGO_TARGET_{prefix}_RUSTFLAGS");
    let mut injected_env = Vec::new();

    let cache_key = if automatic_fast && injection != LinkerInjection::default() {
        pep517_fallback_key(target, &injection)
    } else {
        None
    };
    let cached_fallback = cache_key
        .as_deref()
        .is_some_and(|key| fallback_cache_contains(paths, key));

    if !cached_fallback {
        if let Some(linker) = injection.linker.as_deref() {
            if !effective_command_env_is_non_empty(command, &linker_key) {
                command.env(&linker_key, linker);
                injected_env.push(linker_key.clone());
            }
        }
        if let Some(rustflags) = injection.rustflags.as_deref() {
            if !effective_command_env_is_non_empty(command, &rustflags_key) {
                command.env(&rustflags_key, rustflags);
                injected_env.push(rustflags_key.clone());
            }
        }
    }

    let explicit_fast = !automatic_fast && choice == LinkerChoice::Fast;
    Ok(Pep517LinkerState {
        automatic_fast,
        explicit_fast,
        cached_fallback,
        cache_key,
        candidate: if injection == LinkerInjection::default() {
            None
        } else {
            Some(linker_description(&injection))
        },
        injected_env,
    })
}

fn effective_command_env_is_non_empty(command: &Command, key: &str) -> bool {
    if let Some(value) = command
        .get_envs()
        .find(|(candidate, _)| *candidate == OsStr::new(key))
        .map(|(_, value)| value)
    {
        return value.is_some_and(|value| !value.is_empty());
    }
    std::env::var_os(key).is_some_and(|value| !value.is_empty())
}

fn linker_description(injection: &LinkerInjection) -> String {
    match (&injection.linker, &injection.rustflags) {
        (Some(linker), Some(flags)) => format!("{linker} ({flags})"),
        (Some(linker), None) => linker.clone(),
        (None, Some(flags)) => flags.clone(),
        (None, None) => "platform default".to_string(),
    }
}

fn project_root(start: &Path) -> PathBuf {
    for directory in start.ancestors() {
        if directory.join("Cargo.toml").is_file() {
            return directory.to_path_buf();
        }
    }
    start.to_path_buf()
}

fn project_target_config_value(target: &str, key: &str) -> Option<String> {
    let current = std::env::current_dir().ok()?;
    let root = project_root(&current);
    for relative in [".cargo/config.toml", ".cargo/config"] {
        let path = root.join(relative);
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(value) = contents.parse::<toml::Value>() else {
            continue;
        };
        let configured = value
            .get("target")
            .and_then(|targets| targets.get(target))
            .and_then(|target_config| target_config.get(key));
        if let Some(value) = configured {
            let text = match value {
                toml::Value::String(value) => value.clone(),
                toml::Value::Array(values) => values
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => value.to_string(),
            };
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn pep517_fallback_key(target: &str, injection: &LinkerInjection) -> Option<String> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| OsStr::new("rustc").to_os_string());
    let rustc_identity = Command::new(&rustc).arg("-vV").output().ok()?;
    if !rustc_identity.status.success() {
        return None;
    }

    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"schema", b"pep517-linker-fallback-v1");
    hash_field(&mut hasher, b"target", target.as_bytes());
    hash_field(
        &mut hasher,
        b"injection",
        linker_description(injection).as_bytes(),
    );
    hash_field(
        &mut hasher,
        b"linker-identity",
        &linker_candidate_identity(injection),
    );
    hash_field(&mut hasher, b"rustc", &rustc_identity.stdout);
    for name in [
        "CARGO_BUILD_TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "PATH",
        "CC",
        "CXX",
        "AR",
        "CMAKE",
        "SDKROOT",
        "MACOSX_DEPLOYMENT_TARGET",
        "VCToolsInstallDir",
        "WindowsSdkDir",
        "LIB",
        "SOLDR_PEP517_PROFILE",
        PEP517_LINKER_POLICY_ENV,
    ] {
        hash_field(
            &mut hasher,
            name.as_bytes(),
            std::env::var_os(name)
                .as_deref()
                .unwrap_or_else(|| OsStr::new(""))
                .to_string_lossy()
                .as_bytes(),
        );
    }

    let current = std::env::current_dir().ok()?;
    let root = project_root(&current);
    for relative in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        ".cargo/config.toml",
        ".cargo/config",
    ] {
        let path = root.join(relative);
        if let Ok(bytes) = std::fs::read(&path) {
            hash_field(&mut hasher, relative.as_bytes(), &bytes);
        }
    }
    Some(hex::encode(hasher.finalize()))
}

fn linker_candidate_identity(injection: &LinkerInjection) -> Vec<u8> {
    let mut candidates = Vec::new();
    if let Some(linker) = injection.linker.as_deref() {
        candidates.push(linker.to_string());
    }
    if let Some(rustflags) = injection.rustflags.as_deref() {
        for candidate in ["mold", "ld.lld"] {
            if rustflags.contains(&format!("-fuse-ld={candidate}")) {
                candidates.push(candidate.to_string());
            }
        }
    }

    let mut identity = Vec::new();
    for candidate in candidates {
        identity.extend_from_slice(candidate.as_bytes());
        identity.push(0);
        let mut command = Command::new(&candidate);
        command.arg("--version");
        suppress_windows_console_window(&mut command);
        match command.output() {
            Ok(output) => {
                identity.extend_from_slice(&output.stdout);
                identity.extend_from_slice(&output.stderr);
                identity.extend_from_slice(&output.status.code().unwrap_or(-1).to_le_bytes());
            }
            Err(err) => identity.extend_from_slice(err.to_string().as_bytes()),
        }
        identity.push(0xff);
    }
    identity
}

fn hash_field(hasher: &mut Sha256, name: &[u8], value: &[u8]) {
    hasher.update((name.len() as u64).to_le_bytes());
    hasher.update(name);
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn fallback_cache_path(paths: &SoldrPaths) -> PathBuf {
    paths.cache.join(PEP517_LINKER_FALLBACK_FILE)
}

fn fallback_cache_contains(paths: &SoldrPaths, key: &str) -> bool {
    std::fs::read_to_string(fallback_cache_path(paths))
        .map(|contents| contents.lines().any(|line| line.trim() == key))
        .unwrap_or(false)
}

pub fn record_pep517_fallback(paths: &SoldrPaths, key: Option<&str>) -> Result<(), SoldrError> {
    let Some(key) = key else { return Ok(()) };
    paths.ensure_dirs()?;
    let path = fallback_cache_path(paths);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)?;
    file.lock_exclusive()?;
    file.seek(SeekFrom::Start(0))?;
    let mut existing = String::new();
    file.read_to_string(&mut existing)?;
    if !existing.lines().any(|line| line.trim() == key) {
        file.seek(SeekFrom::End(0))?;
        writeln!(file, "{key}")?;
    }
    file.unlock()?;
    Ok(())
}

/// Keep linker retry conservative: only output with an explicit linker
/// signal and a failure marker is eligible for the one-shot fallback.
pub fn looks_like_linker_failure(output: &Output) -> bool {
    if output.status.success() {
        return false;
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    looks_like_linker_failure_text(&text)
}

fn looks_like_linker_failure_text(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    let linker_signal = [
        "linking with",
        "linker",
        "link.exe",
        "lld-link",
        "fuse-ld",
        "mold",
        "ld returned",
    ];
    let failure_signal = [
        "failed",
        "error",
        "not found",
        "cannot",
        "could not",
        "exit status",
    ];
    linker_signal.iter().any(|needle| text.contains(needle))
        && failure_signal.iter().any(|needle| text.contains(needle))
}

/// Probe whether `mold` is on `PATH`. Best-effort: any failure (missing
/// binary, non-zero exit, IO error) returns `false`.
fn mold_on_path() -> bool {
    let mut command = std::process::Command::new("mold");
    command.arg("--version");
    suppress_windows_console_window(&mut command);
    match command.output() {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

/// Convert a target triple to the uppercase underscore form Cargo uses
/// for per-target env vars. `x86_64-unknown-linux-gnu` becomes
/// `X86_64_UNKNOWN_LINUX_GNU`, so the corresponding linker env var name
/// is `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER`.
pub fn cargo_target_env_prefix(triple: &str) -> String {
    triple.replace('-', "_").to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINUX: &str = "x86_64-unknown-linux-gnu";
    const LINUX_MUSL: &str = "x86_64-unknown-linux-musl";
    const MAC_X64: &str = "x86_64-apple-darwin";
    const MAC_ARM: &str = "aarch64-apple-darwin";
    const WIN_MSVC: &str = "x86_64-pc-windows-msvc";
    const WIN_GNU: &str = "x86_64-pc-windows-gnu";

    fn always_false() -> bool {
        false
    }

    fn always_true() -> bool {
        true
    }

    #[test]
    fn parses_known_values_case_insensitively() {
        assert_eq!(
            LinkerChoice::from_str("default").unwrap(),
            LinkerChoice::Default
        );
        assert_eq!(LinkerChoice::from_str("LD").unwrap(), LinkerChoice::Ld);
        assert_eq!(LinkerChoice::from_str("Mold").unwrap(), LinkerChoice::Mold);
        assert_eq!(
            LinkerChoice::from_str("rust-lld").unwrap(),
            LinkerChoice::RustLld
        );
        assert_eq!(
            LinkerChoice::from_str("RUST-LLD").unwrap(),
            LinkerChoice::RustLld
        );
        assert_eq!(LinkerChoice::from_str("fast").unwrap(), LinkerChoice::Fast);
    }

    #[test]
    fn empty_parses_as_default() {
        assert_eq!(LinkerChoice::from_str("").unwrap(), LinkerChoice::Default);
        assert_eq!(
            LinkerChoice::from_str("   ").unwrap(),
            LinkerChoice::Default
        );
    }

    #[test]
    fn unknown_value_is_clear_error() {
        let err = LinkerChoice::from_str("gold").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("invalid SOLDR_LINKER value"),
            "unexpected error message: {msg}"
        );
        assert!(msg.contains("gold"), "should echo the bad value: {msg}");
        assert!(
            msg.contains("default") && msg.contains("mold") && msg.contains("rust-lld"),
            "should list valid choices: {msg}"
        );
    }

    #[test]
    fn env_wins_over_config() {
        let choice = from_env_and_config(Some(OsStr::new("mold")), Some("rust-lld")).unwrap();
        assert_eq!(choice, LinkerChoice::Mold);
    }

    #[test]
    fn config_fallback_when_env_unset() {
        let choice = from_env_and_config(None, Some("rust-lld")).unwrap();
        assert_eq!(choice, LinkerChoice::RustLld);
    }

    #[test]
    fn nothing_falls_back_to_default() {
        let choice = from_env_and_config(None, None).unwrap();
        assert_eq!(choice, LinkerChoice::Default);
    }

    #[test]
    fn empty_env_string_falls_back_to_default() {
        let choice = from_env_and_config(Some(OsStr::new("")), Some("mold")).unwrap();
        // Empty env string is treated as "no explicit choice" -> Default.
        assert_eq!(choice, LinkerChoice::Default);
    }

    #[test]
    fn default_and_ld_inject_nothing_on_every_target() {
        for triple in [LINUX, LINUX_MUSL, MAC_X64, MAC_ARM, WIN_MSVC, WIN_GNU] {
            let i = resolve_for_target_with_probe(LinkerChoice::Default, triple, &always_false)
                .unwrap();
            assert_eq!(i, LinkerInjection::default(), "default/{triple}");
            let i = resolve_for_target_with_probe(LinkerChoice::Ld, triple, &always_false).unwrap();
            assert_eq!(i, LinkerInjection::default(), "ld/{triple}");
        }
    }

    #[test]
    fn mold_on_linux_uses_clang_with_fuse_mold() {
        let i = resolve_for_target_with_probe(LinkerChoice::Mold, LINUX, &always_false).unwrap();
        assert_eq!(i.linker.as_deref(), Some("clang"));
        assert_eq!(i.rustflags.as_deref(), Some("-C link-arg=-fuse-ld=mold"));
    }

    #[test]
    fn mold_on_macos_returns_clear_error() {
        let err =
            resolve_for_target_with_probe(LinkerChoice::Mold, MAC_X64, &always_false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("mold is not supported"),
            "unexpected message: {msg}"
        );
        assert!(msg.contains(MAC_X64), "error should name the target: {msg}");
        assert!(msg.contains("fast"), "error should hint at fast: {msg}");
    }

    #[test]
    fn mold_on_windows_returns_clear_error() {
        let err =
            resolve_for_target_with_probe(LinkerChoice::Mold, WIN_MSVC, &always_false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("mold is not supported"),
            "unexpected message: {msg}"
        );
        assert!(msg.contains(WIN_MSVC), "error should name target: {msg}");
    }

    #[test]
    fn rust_lld_on_msvc_uses_rust_lld_directly() {
        let i =
            resolve_for_target_with_probe(LinkerChoice::RustLld, WIN_MSVC, &always_false).unwrap();
        assert_eq!(i.linker.as_deref(), Some("rust-lld"));
        assert!(i.rustflags.is_none());
    }

    #[test]
    fn rust_lld_on_non_msvc_non_apple_uses_clang_with_fuse_lld() {
        for triple in [LINUX, LINUX_MUSL, WIN_GNU] {
            let i = resolve_for_target_with_probe(LinkerChoice::RustLld, triple, &always_false)
                .unwrap();
            assert_eq!(i.linker.as_deref(), Some("clang"), "{triple}");
            assert_eq!(
                i.rustflags.as_deref(),
                Some("-C link-arg=-fuse-ld=lld"),
                "{triple}"
            );
        }
    }

    /// Issue #509: Apple clang rejects `-fuse-ld=lld` (it expects
    /// `ld64.lld`, which stock macOS toolchains do not ship). `RustLld`
    /// on Apple targets must therefore inject nothing and fall back to
    /// the platform default linker. This test is host-agnostic because
    /// `target_kind` is driven purely by the triple string.
    #[test]
    fn rust_lld_on_macos_falls_back_to_platform_default() {
        for triple in [MAC_X64, MAC_ARM] {
            let i = resolve_for_target_with_probe(LinkerChoice::RustLld, triple, &always_false)
                .unwrap();
            assert_eq!(
                i,
                LinkerInjection::default(),
                "rust-lld on `{triple}` must not inject -fuse-ld=lld (issue #509)"
            );
        }
    }

    #[test]
    fn fast_on_linux_prefers_mold_when_present() {
        let i = resolve_for_target_with_probe(LinkerChoice::Fast, LINUX, &always_true).unwrap();
        assert_eq!(i.linker.as_deref(), Some("clang"));
        assert_eq!(i.rustflags.as_deref(), Some("-C link-arg=-fuse-ld=mold"));
    }

    #[test]
    fn fast_on_linux_falls_back_to_rust_lld_when_mold_absent() {
        let i = resolve_for_target_with_probe(LinkerChoice::Fast, LINUX, &always_false).unwrap();
        assert_eq!(i.linker.as_deref(), Some("clang"));
        assert_eq!(i.rustflags.as_deref(), Some("-C link-arg=-fuse-ld=lld"));
    }

    /// Issue #509: `SOLDR_LINKER=fast` on macOS used to inject
    /// `-fuse-ld=lld`, which breaks Apple-clang-driven `cc-rs` build
    /// scripts ("invalid linker name in argument '-fuse-ld=lld'"). The
    /// fast mode must now silently fall back to the platform default
    /// linker on every Apple target, regardless of the host that ran the
    /// resolver — so this test covers the bug whether it executes on
    /// Linux, macOS, or Windows.
    #[test]
    fn fast_on_macos_falls_back_to_platform_default() {
        for triple in [MAC_X64, MAC_ARM] {
            let i =
                resolve_for_target_with_probe(LinkerChoice::Fast, triple, &always_false).unwrap();
            assert_eq!(
                i,
                LinkerInjection::default(),
                "fast on `{triple}` must not inject -fuse-ld=lld (issue #509)"
            );
            // Also exercise the mold-present branch — mold is irrelevant
            // on Apple targets and must not change the outcome.
            let i =
                resolve_for_target_with_probe(LinkerChoice::Fast, triple, &always_true).unwrap();
            assert_eq!(
                i,
                LinkerInjection::default(),
                "fast on `{triple}` must not inject -fuse-ld=lld even when mold is on PATH (issue #509)"
            );
        }
    }

    #[test]
    fn fast_on_windows_msvc_uses_rust_lld_directly() {
        let i = resolve_for_target_with_probe(LinkerChoice::Fast, WIN_MSVC, &always_false).unwrap();
        assert_eq!(i.linker.as_deref(), Some("rust-lld"));
        assert!(i.rustflags.is_none());
    }

    #[test]
    fn linker_failure_classifier_ignores_non_linker_failures() {
        assert!(!looks_like_linker_failure_text(
            "error: failed to parse source file"
        ));
        assert!(looks_like_linker_failure_text(
            "error: linking with `clang` failed: mold not found"
        ));
    }

    #[test]
    fn fallback_record_is_idempotent_and_corruption_tolerant() {
        let root = tempfile::tempdir().expect("temporary soldr root");
        let paths = SoldrPaths::with_root(root.path().to_path_buf());
        record_pep517_fallback(&paths, Some("key-a")).expect("record fallback");
        record_pep517_fallback(&paths, Some("key-a")).expect("record duplicate fallback");
        record_pep517_fallback(&paths, Some("key-b")).expect("record second fallback");
        let contents = std::fs::read_to_string(fallback_cache_path(&paths)).unwrap();
        assert_eq!(contents.lines().collect::<Vec<_>>(), ["key-a", "key-b"]);
        assert!(!fallback_cache_contains(&paths, "key-corrupt"));
    }

    #[test]
    fn cargo_target_env_prefix_uppercases_and_replaces_hyphens() {
        assert_eq!(
            cargo_target_env_prefix("x86_64-unknown-linux-gnu"),
            "X86_64_UNKNOWN_LINUX_GNU"
        );
        assert_eq!(
            cargo_target_env_prefix("aarch64-apple-darwin"),
            "AARCH64_APPLE_DARWIN"
        );
        assert_eq!(
            cargo_target_env_prefix("x86_64-pc-windows-msvc"),
            "X86_64_PC_WINDOWS_MSVC"
        );
    }
}
