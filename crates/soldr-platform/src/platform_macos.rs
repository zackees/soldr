//! macOS concrete implementation tree (issue #2493).
//!
//! Host cfg and native APIs are allowed here and in every file under
//! `platform_macos/`. This tree is private: it is reachable only through
//! the `lib.rs` selection site (`platform_imp`) and the neutral facades.
//!
//! Linux and macOS are deliberately separate implementations — there is no
//! `platform_unix` tree. The two may share cfg-free helpers that live in
//! the neutral facade files.
pub(crate) mod executable;
pub(crate) mod fs;
pub(crate) mod host;
