//! Unit tests for [`crate::rust_plan`]. Split across topic files so no
//! single source file blows past the project's 1000-LOC ceiling. Wired in
//! via `#[path = "rust_plan_tests/mod.rs"] mod tests;` at the bottom of
//! `rust_plan.rs`.

mod bundle_walk;
mod local_roundtrip;
mod manifest;
mod orphan_rmeta;
mod plan_build;
mod plan_content_identity;
mod prep_memo;
mod prepopulated_target;
mod restore_gc_protection;
mod warm_restore;
mod wire_compat;
