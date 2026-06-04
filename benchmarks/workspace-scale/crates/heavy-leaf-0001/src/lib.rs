//! Deliberately minimal — the workspace-scale benchmark fixture
//! exercises the dep graph in `Cargo.toml`, not the own-crate code.
//! See `../README.md` and soldr#648.

/// Dummy public function so the crate isn't `#[no_implicit_prelude]`
/// detected as completely dead by future linters and so adding a
/// `pub use` re-export from a follow-up doesn't break in-tree consumers.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
