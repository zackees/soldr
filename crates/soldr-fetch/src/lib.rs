//! Binary resolution and download.
//!
//! Resolves crate binaries from:
//! 1. Local cache (~/.soldr/bin/)
//! 2. Binstall metadata (Cargo.toml [package.metadata.binstall])
//! 3. GitHub Releases
//! 4. QuickInstall registry
//! 5. Source compilation (fallback)
