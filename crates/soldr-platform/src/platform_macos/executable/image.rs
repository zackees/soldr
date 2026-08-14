//! macOS running-image identity.

/// Always `None`: Mach-O images carry an `LC_UUID` load command rather than
/// a GNU build ID, and reading it is not implemented. Callers fall back to
/// hashing the executable file (soldr#2549 keeps macOS on the proven hash
/// path until an equivalent native image identity is designed and tested).
pub fn current_build_id() -> Option<Vec<u8>> {
    None
}
