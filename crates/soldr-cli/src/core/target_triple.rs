//! Target-triple types and host-detection plumbing.
//!
//! Holds the `Arch`/`Os`/`Env` triplet, the public `TargetTriple` value
//! type with its parsing/formatting impls, and the compile-time host
//! probes (`compile_time_arch`, `compile_time_host_os`,
//! `compile_time_fallback_triple`). The runtime detection entry point
//! `TargetTriple::detect_from_dir` reaches across into
//! `super::toolchain_resolve` for explicit-override discovery and
//! runtime rustc probing.

use std::path::Path;

use super::toolchain_resolve::{detect_runtime_rustc_triple, read_explicit_target_override};
use super::SoldrError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // `MacOs` ends with `Os` by design — it's the canonical Rust spelling.
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

pub(super) fn compile_time_arch() -> Result<Arch, SoldrError> {
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

pub(super) fn compile_time_host_os() -> Result<Os, SoldrError> {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[cfg(target_os = "windows")]
    #[test]
    fn defaults_to_msvc_without_explicit_override() {
        let dir = tempfile::tempdir().unwrap();
        let target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        // Runtime arch: Windows runners come in x86_64 AND aarch64 on
        // GitHub Actions. Both are valid host triples — the test must
        // expect the runner's actual arch, not a hardcoded x86_64.
        let expected_arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else {
            "aarch64"
        };
        assert_eq!(target.triple(), format!("{expected_arch}-pc-windows-msvc"));
    }
}
