//! macOS host facts.

/// The host operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    /// Microsoft Windows.
    Windows,
    /// Apple macOS.
    MacOs,
    /// Linux.
    Linux,
}

/// The host CPU architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostArch {
    /// x86-64.
    X86_64,
    /// 64-bit ARM.
    Aarch64,
    /// Anything else; carries the `std::env::consts::ARCH` string.
    Unknown(&'static str),
}

/// The host's C library environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostLibc {
    /// Not applicable (Windows, macOS).
    None,
    /// GNU libc.
    Gnu,
    /// musl.
    Musl,
}

/// The raw compile-host facts `TargetTriple::host()` consumes. The
/// triple construction itself is the caller's policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostInfo {
    /// Host operating system.
    pub os: HostOs,
    /// Host architecture.
    pub arch: HostArch,
    /// Host libc environment.
    pub libc: HostLibc,
}

/// The host operating system (this tree compiles only on macOS).
pub fn os() -> HostOs {
    HostOs::MacOs
}

/// The host architecture.
pub fn arch() -> HostArch {
    if cfg!(target_arch = "x86_64") {
        HostArch::X86_64
    } else if cfg!(target_arch = "aarch64") {
        HostArch::Aarch64
    } else {
        HostArch::Unknown(std::env::consts::ARCH)
    }
}

/// The compile-time libc environment (the `target_env` of this build).
pub fn libc() -> HostLibc {
    HostLibc::None
}

/// The compile-time host triple (the target this binary was built for).
pub fn triple() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-apple-darwin"
    }
}

/// macOS has no MAX_PATH-style ceiling worth projecting against.
pub fn max_path() -> Option<usize> {
    None
}

/// The PATH list separator for this host.
pub fn path_list_separator() -> &'static str {
    ":"
}

/// The OS version string. Soldr's macOS host facts have no version
/// probe — the Windows-specific registry/PowerShell queries live in the
/// Windows tree — so this is always `None` on macOS.
pub fn os_version() -> Option<String> {
    None
}

/// All host facts in one probe.
pub fn info() -> HostInfo {
    HostInfo {
        os: os(),
        arch: arch(),
        libc: libc(),
    }
}
