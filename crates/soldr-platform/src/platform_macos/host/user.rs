//! macOS user identity and elevation.


/// The current user id.
pub fn uid() -> u32 {
    // SAFETY: getuid has no failure mode.
    unsafe { libc::getuid() }
}

/// The Windows admin-token probe has no macOS meaning: the callers that
/// consult it gate the Windows-only optimize path. macOS privilege is
/// conventionally checked with `getuid() == 0`, which is not what the
/// callers ask for, so this answers `false` rather than inventing a
/// different semantic.
pub fn is_elevated() -> bool {
    false
}

/// UAC elevation is a Windows-only mechanism; the callers gate this
/// path behind the Windows optimize flow, so a macOS call is an
/// internal error rather than an attempt.
pub fn relaunch_elevated(
    _powershell: &std::path::Path,
    _args: &[String],
    _helper_output_path: &std::path::Path,
    _helper_output_env: &str,
) -> Result<i32, String> {
    Err("UAC elevation is not available on this host".to_string())
}
