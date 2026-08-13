//! Windows user identity and elevation.

/// True when the current process holds an administrator token.
///
/// Probes a registry read of `HKLM\SECURITY` — only admin processes can
/// open this key, so a read failure means non-admin.
pub fn is_elevated() -> bool {
    std::process::Command::new("reg")
        .args(["query", r"HKLM\SECURITY"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}
