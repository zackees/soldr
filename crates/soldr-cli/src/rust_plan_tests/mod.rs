//! Unit tests for [`crate::rust_plan`]. Split across topic files so no
//! single source file blows past the project's 1000-LOC ceiling. Wired in
//! via `#[path = "rust_plan_tests/mod.rs"] mod tests;` at the bottom of
//! `rust_plan.rs`.

mod bundle_walk;
mod manifest;
mod orphan_rmeta;
mod plan_build;
mod warm_restore;
mod wire_compat;
