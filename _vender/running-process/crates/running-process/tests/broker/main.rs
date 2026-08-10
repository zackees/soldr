//! Integration tests for the v1 broker proto schemas (#228 Phase 0).
//!
//! All gated behind `feature = "client"` because the broker module
//! itself is. With `--no-default-features` this crate compiles to an
//! empty integration test binary.

#![cfg(feature = "client")]

use std::sync::Mutex;

static HELLO_TIMEOUT_ENV_LOCK: Mutex<()> = Mutex::new(());

struct HelloTimeoutOverride(Option<std::ffi::OsString>);

impl HelloTimeoutOverride {
    fn new(timeout_ms: &str) -> Self {
        let previous = std::env::var_os("RUNNING_PROCESS_BROKER_HELLO_TIMEOUT_MS");
        std::env::set_var("RUNNING_PROCESS_BROKER_HELLO_TIMEOUT_MS", timeout_ms);
        Self(previous)
    }
}

impl Drop for HelloTimeoutOverride {
    fn drop(&mut self) {
        match self.0.take() {
            Some(previous) => {
                std::env::set_var("RUNNING_PROCESS_BROKER_HELLO_TIMEOUT_MS", previous)
            }
            None => std::env::remove_var("RUNNING_PROCESS_BROKER_HELLO_TIMEOUT_MS"),
        }
    }
}

mod admin;
mod backend_endpoint_allocator;
mod backend_handle_boot_id;
mod backend_handle_common;
mod backend_handle_dead;
mod backend_handle_probe;
mod backend_handle_recycled;
mod backend_registry;
mod backend_sdk;
#[cfg(feature = "client-async")]
mod backend_sdk_async;
mod broadcast_release_handles;
mod builders;
mod client;
#[cfg(feature = "test-support")]
mod conformance_kit;
mod connection;
mod contrib_templates;
mod docs_escape_hatch;
mod docs_index;
mod doctor;
mod fd_pressure;
mod framing;
mod golden_bytes;
mod handoff_ack_deadline;
mod handoff_adopted_backend;
mod handoff_adoption;
mod handoff_backend_lib;
mod handoff_capability;
mod handoff_cross_os_acceptance;
mod handoff_end_to_end_acceptance;
mod handoff_fallback_perm_denied;
mod handoff_latency;
mod handoff_latency_e2e;
mod handoff_orchestrate;
mod handoff_serve_e2e;
mod handoff_serve_latency;
mod handoff_token_mismatch;
mod handoff_transport;
mod handoff_under_load;
#[cfg(unix)]
mod handoff_unix_e2e_orchestrate;
mod handoff_unix_orchestrate;
#[cfg(windows)]
mod handoff_windows_duplicate_handle;
#[cfg(windows)]
mod handoff_windows_orchestrate;
mod handoff_wire;
mod hello_concurrent;
mod hello_handler;
mod hello_rate_limit;
mod hello_router;
mod hello_service_unknown;
mod hello_skip;
mod hello_version_blocked;
mod idle_coord;
mod instance;
mod instance_isolation;
mod into_backend_io;
mod lifecycle_event_size;
mod manifest_atomic;
mod manifest_boot_id;
mod manifest_corruption;
mod manifest_roundtrip;
mod metrics_names_frozen;
mod names;
mod peer_creds_drop;
mod perf_guard;
mod process_tree_lifecycle;
mod proto_field_numbers;
mod proto_roundtrip;
mod recovery_one_retry;
mod serve;
mod service_def_loader;
mod servicedef_cli;
mod socket_common;
mod spawn_coordinator;
mod spawn_wait;
mod toy_three_party;
mod trace_propagation;
mod verify_pid;
