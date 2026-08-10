//! Security regression tests for currently exposed v1 broker validation.
//!
//! These tests intentionally cover the broker security surfaces that are
//! available without external reviewer or cross-user setup. Deferred runtime
//! surfaces stay documented separately until their broker runtime exists.

#![cfg(feature = "client")]

mod cross_user_release_handles;
mod cve_dbus_2014_3639;
mod cve_dbus_2023_34969;
mod cve_sccache_2023_1521;
mod deferred_runtime_surfaces;
mod dependency_surface;
mod fuzz_campaign_signoff;
mod manifest_tamper_detection;
mod no_network_dependencies;
mod pipe_name_validation;
mod pipe_squatting;
mod security_audit_workflow;
mod security_fuzz_workflow;
mod service_name_validation;
#[path = "../broker/socket_common.rs"]
mod socket_common;
mod unsafe_inventory;
mod wanted_version_shell_injection;
mod wanted_version_traversal;
