// Direct concrete-tree references outside soldr-platform are denied even
// in private code.

mod inner {
    fn uses_tree() {
        let _ = crate::platform_win::fs::identity::file_identity(std::path::Path::new("x")); //~ ERROR host-platform selection outside the soldr-platform boundary
        let _ = crate::platform_linux::fs::identity::file_identity(std::path::Path::new("x")); //~ ERROR host-platform selection outside the soldr-platform boundary
        let _ = crate::platform_macos::fs::identity::file_identity(std::path::Path::new("x")); //~ ERROR host-platform selection outside the soldr-platform boundary
        let _ = crate::platform_imp::fs::identity::file_identity(std::path::Path::new("x")); //~ ERROR host-platform selection outside the soldr-platform boundary
    }
}
