//! Broker lifecycle, endpoint identity, and process kill-matrix integration tests.
//!
//! soldr#2934: one linked test binary per category instead of one per source
//! file. Each module below was previously its own top-level test binary, so
//! test IDs are now `<module>::<test_name>`.

#[path = "../common/mod.rs"]
mod common;

mod broker_identity_pipe;
mod broker_identity_unix;
mod cli_broker_resurrection;
mod cli_broker_routes;
mod cli_broker_single_instance;
mod cli_broker_status;
mod cli_broker_stop;
mod cli_build_alias_parity;
mod cli_build_fetch_overlap;
mod cli_jobs_routing;
mod cli_kill_matrix;
mod isolated_daemon_shared_copy;
