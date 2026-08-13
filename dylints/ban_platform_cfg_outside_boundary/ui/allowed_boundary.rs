// The boundary's inside: the selection site and the concrete trees are
// allowed host cfg and concrete-tree references.

#[cfg(target_os = "linux")]
mod selected { pub fn f() {} }

fn neutral() -> u8 {
    crate::platform_imp::fs::identity::file_identity(std::path::Path::new("x")).map(|_| 1).unwrap_or(0)
}
