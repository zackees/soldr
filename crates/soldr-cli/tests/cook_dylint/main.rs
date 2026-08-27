//! `soldr cook` dependency-prebuild and Dylint cook integration tests:
//! the cook CLI surface, the cross-repo cook artifact index served over
//! daemon IPC, and the cargo-front-door hydrate pre-flight.
//!
//! soldr#2934: one linked test binary per category instead of one per source
//! file. Each module below was previously its own top-level test binary, so
//! test IDs are now `<module>::<test_name>`.

#[path = "../common/mod.rs"]
mod common;

mod cli_cook;
mod cli_dylint_cook;
mod cook_hydrate_preflight;
mod cook_writes_index;
mod daemon_cook_index;
