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
    /// Detect the host triple for the currently-running soldr binary.
    ///
    /// Unlike [`TargetTriple::detect`], this intentionally ignores project
    /// target overrides from `.cargo/config.toml` or `rust-toolchain.toml`.
    /// Host-executed tools fetched by soldr, such as `cargo-zigbuild`, must
    /// match the runner that will execute them, not the Rust target being
    /// built.
    pub fn host() -> Result<Self, SoldrError> {
        Self::from_triple(&compile_time_host_triple()?)
    }

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

        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
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
    use crate::platform::host::facts::HostArch;
    match crate::platform::host::facts::arch() {
        HostArch::X86_64 => Ok(Arch::X86_64),
        HostArch::Aarch64 => Ok(Arch::Aarch64),
        HostArch::Unknown(other) => Err(SoldrError::UnsupportedPlatform(format!(
            "unsupported arch: {other}"
        ))),
    }
}

pub(super) fn compile_time_host_os() -> Result<Os, SoldrError> {
    use crate::platform::host::facts::HostOs;
    // Only the three supported OS trees compile at all (the crate's
    // cfg_select has no fallback), so this can never be anything else.
    match crate::platform::host::facts::os() {
        HostOs::Windows => Ok(Os::Windows),
        HostOs::MacOs => Ok(Os::MacOs),
        HostOs::Linux => Ok(Os::Linux),
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
        Os::Linux => match crate::platform::host::facts::info().libc {
            crate::platform::host::facts::HostLibc::Musl => format!("{arch}-unknown-linux-musl"),
            _ => format!("{arch}-unknown-linux-gnu"),
        },
    };
    Ok(triple)
}

fn compile_time_host_triple() -> Result<String, SoldrError> {
    let arch = match compile_time_arch()? {
        Arch::X86_64 => "x86_64",
        Arch::Aarch64 => "aarch64",
    };
    let triple = match compile_time_host_os()? {
        Os::Windows => format!("{arch}-pc-windows-msvc"),
        Os::MacOs => format!("{arch}-apple-darwin"),
        Os::Linux => {
            let env = match crate::platform::host::facts::libc() {
                crate::platform::host::facts::HostLibc::Musl => "musl",
                _ => "gnu",
            };
            format!("{arch}-unknown-linux-{env}")
        }
    };
    Ok(triple)
}

/// Pick the Linux libc to encode in the fallback target triple when no
/// explicit override and no runtime `rustc` are available. Without this
/// detection, soldr defaults to `*-unknown-linux-gnu` and downloads
/// glibc-linked prebuilt assets (cargo-nextest, etc.) that fail to
/// `execve()` on a musl host with the misleading `os error 2`. Issue #806.
///
/// Resolution order:
/// 1. **Compile-time**: if soldr itself was built for musl
///    (`cfg!(target_env = "musl")`), the host has to be musl-compatible
///    or soldr could not have started — pick Musl, skip the probes.
/// 2. **`ldd --version`**: POSIX, available on every glibc host and
///    most musl distributions (Alpine ships it as a busybox shim).
///    Musl's stderr line includes the word `musl`.
/// 3. **Filesystem probe**: `/lib/ld-musl-<arch>.so.1` is the musl
///    dynamic linker's well-known path. Treat it as dispositive only
///    when a glibc dynamic linker is not also present; Ubuntu release
///    runners install `musl-tools` for cross-compile lanes, and that
///    drops a musl linker onto an otherwise glibc host.
/// 4. **Default**: glibc. Most Linux distributions ship glibc — only
///    musl distros need the override.
pub(crate) fn detect_linux_libc() -> Env {
    // The ldd/linker probes and the classification table live in the
    // platform crate's Linux facts (where the host cfg belongs); this
    // keeps the soldr-core `Env` mapping for the triple construction.
    match crate::platform::host::facts::info().libc {
        crate::platform::host::facts::HostLibc::Musl => Env::Musl,
        _ => Env::Gnu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_target() {
        use crate::platform::host::facts::{arch, os, HostArch, HostOs};

        let t = TargetTriple::detect().unwrap();
        match os() {
            HostOs::Windows => {
                if arch() == HostArch::X86_64 {
                    assert_eq!(t.os, Os::Windows);
                    assert_eq!(t.env, Env::Msvc);
                    assert_eq!(t.arch, Arch::X86_64);
                    assert_eq!(t.triple(), "x86_64-pc-windows-msvc");
                }
            }
            HostOs::MacOs => assert_eq!(t.os, Os::MacOs),
            HostOs::Linux => assert_eq!(t.os, Os::Linux),
        }
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
    fn host_ignores_project_target_override() {
        let dir = tempfile::tempdir().unwrap();
        let cargo_dir = dir.path().join(".cargo");
        std::fs::create_dir_all(&cargo_dir).unwrap();
        let override_triple = if crate::platform::host::facts::os()
            == crate::platform::host::facts::HostOs::Windows
        {
            "aarch64-unknown-linux-musl"
        } else {
            "x86_64-pc-windows-msvc"
        };
        std::fs::write(
            cargo_dir.join("config.toml"),
            format!("[build]\ntarget = \"{override_triple}\"\n"),
        )
        .unwrap();

        assert_eq!(
            TargetTriple::detect_in_dir(dir.path()).unwrap().triple(),
            override_triple
        );
        assert_ne!(TargetTriple::host().unwrap().triple(), override_triple);
    }

    #[test]
    fn host_triple_uses_compile_time_target_env() {
        use crate::platform::host::facts::{libc, os, HostLibc, HostOs};

        if os() == HostOs::Linux {
            let host = TargetTriple::host().unwrap();
            match libc() {
                HostLibc::Musl => assert_eq!(host.env, Env::Musl),
                _ => assert_eq!(host.env, Env::Gnu),
            }
        }
    }

    #[test]
    fn defaults_to_msvc_without_explicit_override() {
        if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        // Runtime arch: Windows runners come in x86_64 AND aarch64 on
        // GitHub Actions. Both are valid host triples — the test must
        // expect the runner's actual arch, not a hardcoded x86_64.
        let expected_arch = if std::env::consts::ARCH == "x86_64" {
            "x86_64"
        } else {
            "aarch64"
        };
        assert_eq!(target.triple(), format!("{expected_arch}-pc-windows-msvc"));
    }

    // -----------------------------------------------------------------
    // Linux libc detection for prebuilt asset routing (issue #806).
    //
    // The pure helpers `ldd_output_mentions_musl` are exercised
    // platform-independently; the end-to-end `detect_linux_libc()` /
    // `TargetTriple::detect()` invariants are gated on the host the
    // test binary was built for so only the right CI lane asserts each
    // one. The musl-gated test below is what catches the original bug:
    // on a musl host (Alpine docker harness from
    // `zackees/running-process#514`), soldr's fallback triple must
    // resolve to `*-unknown-linux-musl` so cargo-nextest et al. are
    // downloaded as musl ELFs.
    // -----------------------------------------------------------------

    // The ldd banner helpers and the classification table moved to
    // soldr-platform's Linux facts, where their tests now live beside
    // the implementation.

    /// linux-musl only: when soldr itself is built for musl, the
    /// fallback target triple MUST resolve to `*-unknown-linux-musl`
    /// so the prebuilt-asset matcher in `fetch::github::match_asset`
    /// picks the musl variant of cargo-nextest et al. Without this
    /// the Alpine docker harness from `zackees/running-process#514`
    /// hits the `os error 2` execve failure described in #806.
    #[test]
    fn musl_host_resolves_to_musl_target_triple() {
        use crate::platform::host::facts::{libc, os, HostLibc, HostOs};

        // The original assertion is gated on the binary being built for
        // linux-musl; any other host cannot promise a musl answer.
        if os() != HostOs::Linux || libc() != HostLibc::Musl {
            return;
        }
        // Pure helper: compile-time short-circuit must kick in.
        assert_eq!(detect_linux_libc(), Env::Musl);

        // End-to-end: detection in an empty dir (no explicit override,
        // no .cargo/config, no rust-toolchain.toml). The fallback path
        // is what the Alpine docker repro hits — there's no rustc on
        // PATH yet because the user just downloaded soldr.
        let dir = tempfile::tempdir().unwrap();
        // Wipe RUSTUP_TOOLCHAIN/PATH-driven probes by pointing at the
        // temp dir, which has no .cargo/.rustup so detect_runtime_rustc
        // is short-circuited unless the runner happens to have a global
        // rustc installed. If a global rustc IS present, that's fine —
        // it would itself be a musl rustc on a musl host, so the answer
        // is still musl.
        let target = TargetTriple::detect_in_dir(dir.path()).unwrap();
        assert_eq!(
            target.env,
            Env::Musl,
            "musl host produced non-musl target triple {} — cargo-nextest fetch would download a glibc ELF that fails execve with os error 2 (#806)",
            target.triple(),
        );
        assert!(
            target.triple().ends_with("-unknown-linux-musl"),
            "expected *-unknown-linux-musl, got {}",
            target.triple(),
        );
    }
}
