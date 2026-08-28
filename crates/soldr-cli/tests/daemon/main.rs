//! Daemon runtime behavior integration tests: lifecycle, single-instance
//! ownership, IPC/ack transport, build-session records, cache maintenance and
//! flush, restart warmth, and stall fault injection.
//!
//! soldr#2934: one linked test binary per category instead of one per source
//! file. Each module below was previously its own top-level test binary, so
//! test IDs are now `<module>::<test_name>`.

#[path = "../common/mod.rs"]
mod common;

mod cli_daemon_builds;
mod cli_daemon_flush_caches;
mod cli_daemon_lifecycle;
mod cli_daemon_single_instance;
mod cli_daemon_target_touch;
mod cli_debug_trace_observer;
mod daemon_ack_best_effort;
mod daemon_cache_maintenance;
mod daemon_restart_warmth;
mod daemon_stall_harness;
mod inherited_stdio_spawns_mark_spoke;
