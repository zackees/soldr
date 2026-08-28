//! Repository invariant guards: source-policy lints, manifest/version lockstep,
//! binary layout, and frozen-surface contract checks.
//!
//! soldr#2934: one linked test binary per category instead of one per source
//! file. Each module below was previously its own top-level test binary, so
//! test IDs are now `<module>::<test_name>`.

#[path = "../common/mod.rs"]
mod common;

mod build_session_order_lint;
mod canonical_targets_parity;
mod cli_ci_test;
mod cli_lint;
mod cli_startup_smoke;
mod daemon_console_policy_guard;
mod daemon_state_db_ownership_guard;
mod env_lock_lint;
mod msrv_doc_matches_manifest;
mod multicall_bin_layout;
mod no_panicking_argv_collection;
mod no_standalone_spawn_lint;
mod no_timed_test_guard;
mod phase5_contract;
mod version_lockstep;
