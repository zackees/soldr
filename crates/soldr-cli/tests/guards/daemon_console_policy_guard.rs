//! Source-policy guard for broker-only daemon creation (soldr#2427).
//!
//! The requesting Soldr process registers the exact source image and cache
//! root in a service definition. Only `broker_launcher` may place and spawn the
//! long-lived daemon. CLI, wrapper, and daemon entrypoints must never recreate
//! the former client-owned spawn or direct-rustc fallback paths.

use crate::common;

fn read_source_with_includes(path: &std::path::Path) -> String {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let mut expanded = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(relative) = trimmed
            .strip_prefix("include!(\"")
            .and_then(|rest| rest.strip_suffix("\");"))
        {
            expanded.push_str(&read_source_with_includes(
                &path
                    .parent()
                    .expect("source file has a parent")
                    .join(relative),
            ));
        } else {
            expanded.push_str(line);
            expanded.push('\n');
        }
    }
    expanded
}

fn read_crate_src(rel: &str) -> String {
    let path = common::crate_root().join(rel);
    read_source_with_includes(&path)
}

#[test]
fn only_broker_launcher_places_and_spawns_daemon() {
    let launcher = read_crate_src("src/broker_launcher.rs");
    assert!(
        launcher.contains("ensure_daemon_relocated"),
        "broker_launcher must own daemon image placement"
    );
    assert!(
        launcher.contains("spawn_daemon_with_stdio_and_env_policy"),
        "broker_launcher must own detached daemon creation"
    );

    for rel in [
        "src/compile_dispatch.rs",
        "src/daemon_entry.rs",
        "src/multicall.rs",
        "src/soldr_main.rs",
        "src/wrapper.rs",
    ] {
        let source = read_crate_src(rel);
        assert!(
            !source.contains("try_spawn_detached"),
            "{rel} must not create soldr-daemon; route through the broker"
        );
        assert!(
            !source.contains("reexec_from_runtime_root"),
            "{rel} must not relocate/re-exec soldr-daemon; the broker places it"
        );
    }

    for rel in ["src/multicall.rs", "src/wrapper.rs"] {
        let source = read_crate_src(rel);
        assert!(
            !source.contains("should_fall_back_to_direct_rustc"),
            "{rel} must hard-fail broker/daemon infrastructure errors"
        );
    }

    let compile_dispatch = read_crate_src("src/compile_dispatch.rs");
    for forbidden in [
        "client::compile_streaming",
        "dispatch_compile_with_sock",
        "resolved_spawn_retry_budget",
        "append_compile_daemon_fallback_event",
    ] {
        assert!(
            !compile_dispatch.contains(forbidden),
            "compile_dispatch must have no direct daemon acquisition API: {forbidden}"
        );
    }
}
