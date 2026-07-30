//! Source-policy guard for the Windows daemon-console regression (soldr#2039).
//!
//! Both managed daemon entrypoints must route their pre-server relocation
//! through `reexec_from_runtime_root_for_daemon_entry`, which honors the
//! running-process daemon marker (detach with no console when the process was
//! launched through the managed daemon boundary; keep the terminal only for a
//! genuine user-invoked foreground run). The popup-producing revision hardcoded
//! `reexec_from_runtime_root(false)` in `soldr_main`, sending a managed
//! `via_self` daemon (`soldr daemon start --foreground`, spawned detached) down
//! the `show_console = true` foreground path and popping a visible
//! `soldr-daemon` console on Windows.
//!
//! This test fails against that revision and passes with the fix, without
//! spawning any process or mutating global environment (cf. soldr#1663). Source
//! files are resolved through the runtime workspace root (`common::crate_root`),
//! not the compile-time crate-manifest env, so the test survives archival to a
//! target-run host (cf. `test_archived_source_tests_use_only_runtime_workspace_
//! resolution`).

mod common;

fn read_crate_src(rel: &str) -> String {
    let path = common::crate_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

soldr_cli::timed_test!(daemon_entrypoints_use_marker_aware_relocation, {
    let soldr_main = read_crate_src("src/soldr_main.rs");
    let daemon_entry = read_crate_src("src/daemon_entry.rs");

    // The raw-literal bypass that popped a console (soldr#2039) must not return
    // to either managed daemon entrypoint. Only the shared helper is allowed to
    // consult the marker and forward the resulting bool to
    // `reexec_from_runtime_root`.
    for (name, src) in [
        ("soldr_main.rs", &soldr_main),
        ("daemon_entry.rs", &daemon_entry),
    ] {
        assert!(
            !src.contains("reexec_from_runtime_root(false)")
                && !src.contains("reexec_from_runtime_root(true)"),
            "{name} calls reexec_from_runtime_root with a hardcoded literal; managed daemon \
             entrypoints must use reexec_from_runtime_root_for_daemon_entry so the running-process \
             daemon marker decides console/detach (soldr#2039)"
        );
    }

    // Both entrypoints must route through the marker-aware helper.
    assert!(
        soldr_main.contains("reexec_from_runtime_root_for_daemon_entry"),
        "soldr_main.rs must relocate the foreground daemon start through \
         reexec_from_runtime_root_for_daemon_entry (soldr#2039)"
    );
    assert!(
        daemon_entry.contains("reexec_from_runtime_root_for_daemon_entry"),
        "daemon_entry.rs must relocate through reexec_from_runtime_root_for_daemon_entry (soldr#2039)"
    );
});
