//! Linux user identity and elevation.

/// The Windows admin-token probe has no Linux meaning: the callers that
/// consult it gate the Windows-only optimize path. Unix privilege is
/// conventionally checked with `geteuid() == 0`, which is not what the
/// callers ask for, so this answers `false` rather than inventing a
/// different semantic.
pub fn is_elevated() -> bool {
    false
}
