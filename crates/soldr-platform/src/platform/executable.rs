//! Host-neutral executable facade.
//!
//! Owns native executable and script suffixes, PATH/PATHEXT candidate
//! generation and lookup, current-executable image discovery and equality,
//! and materializing/replacing a runnable image. Tool registries,
//! release-asset slugs, and binary/archive extensions for a *requested
//! build target* stay outside this namespace — that is target policy, not
//! host mechanics.

pub mod name;
pub mod search;
