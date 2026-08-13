//! Linux concrete implementation tree (issue #2493).
//!
//! Host cfg and native APIs are allowed here and in every file under
//! `platform_linux/`. This tree is private: it is reachable only through
//! the `lib.rs` selection site (`platform_imp`) and the neutral facades.
pub(crate) mod executable;
pub(crate) mod fs;
pub(crate) mod host;
