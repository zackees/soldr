//! macOS user identity and elevation.

/// The Windows admin-token probe has no macOS meaning: the callers that
/// consult it gate the Windows-only optimize path. macOS privilege is
/// conventionally checked with `getuid() == 0`, which is not what the
/// callers ask for, so this answers `false` rather than inventing a
/// different semantic.
pub fn is_elevated() -> bool {
    false
}
