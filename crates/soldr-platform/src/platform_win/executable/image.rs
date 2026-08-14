//! Windows running-image identity.

/// Always `None`: PE images carry a CodeView GUID rather than a GNU build
/// ID, and reading it is not implemented. Callers fall back to hashing the
/// executable file (soldr#2549 keeps Windows on the proven hash path until
/// an equivalent native image identity is designed and tested).
pub fn current_build_id() -> Option<Vec<u8>> {
    None
}
