//! Integration tests for the embed-first resolver order (issue #873).
//!
//! These tests exercise the env-var-driven [`ResolverOrder`] gate at
//! the process boundary.
//!
//! soldr#1211 — cargo test runs every `#[test]` in the same binary in
//! the SAME process (with `--test-threads=NUM_CPUS` by default). The
//! two env-var tests below race on `SOLDR_RESOLVER_ORDER` because
//! `std::env::set_var` mutates a process-global; without serialization
//! one test's `remove_var` restore can wipe the other test's just-set
//! value, causing `assert!(!order.try_embed)` to fail against the
//! default `ResolverOrder`. `ENV_LOCK` below serializes them.
//!
//! End-to-end download paths (`try_embedded_manifest_v6` /
//! `try_manifest_first` driving `archive::download_and_extract_with_pin`)
//! are exercised by the existing manifest-lookup integration tests; this
//! file focuses on the in-memory short-circuit semantics so we don't
//! depend on the live network during CI.

use std::sync::Mutex;

use soldr_cli::fetch::{ResolverOrder, RESOLVER_ORDER_ENV_VAR};

/// Serializes tests that mutate the `SOLDR_RESOLVER_ORDER` env var.
/// Pure-Rust tests (no env access) skip this and remain parallel.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn embed_hits_short_circuit_live_fetch() {
    // Populate the embed lookup with a synthetic v6 manifest and prove
    // a positive hit comes back without ever consulting the live fetch.
    // The lookup is pure (no I/O), so any hit at all proves the live
    // path is skipped — see `embed_miss_falls_through_to_live` for the
    // mirror-image test.
    use soldr_cli::fetch::ManifestV6;
    let body = r#"{
        "schema_version": 6,
        "tools": {
            "acme/widget": {
                "x86_64-pc-windows-msvc": {
                    "latest": "1.0.0",
                    "1.0.0": {
                        "href": "https://example.invalid/widget-1.0.0-x86_64-pc-windows-msvc.zip",
                        "sha256": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
                    }
                }
            }
        }
    }"#;
    let manifest = ManifestV6::from_json(body).expect("synthetic v6 parses");
    let hit = manifest
        .lookup("acme", "widget", "x86_64-pc-windows-msvc", None)
        .expect("must hit the synthetic leaf");
    assert_eq!(hit.version, "1.0.0");
    assert_eq!(
        hit.asset.sha256,
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
    );
    // Pin override path — supplying the same version as a pin must
    // produce the same hit. This is the exact code path the embed-first
    // resolver hop calls.
    let pinned = manifest
        .lookup("acme", "widget", "x86_64-pc-windows-msvc", Some("1.0.0"))
        .expect("pin must hit");
    assert_eq!(pinned.version, "1.0.0");
}

#[test]
fn embed_miss_falls_through_to_live() {
    // Empty embed must miss on every lookup, leaving the resolver to
    // fall through to the live hop. We prove the miss semantics here;
    // the integration with `try_manifest_first` is exercised by
    // `tests/fetch_tools/manifest_lookup.rs`.
    use soldr_cli::fetch::ManifestV6;
    let empty = ManifestV6::from_json(r#"{"schema_version":6,"tools":{}}"#).unwrap();
    assert!(empty.is_empty());
    assert!(empty
        .lookup("acme", "widget", "x86_64-pc-windows-msvc", None)
        .is_none());
    // ResolverOrder default → all three hops fire, so a miss in embed
    // doesn't prevent live or api from running.
    let order = ResolverOrder::all();
    assert!(
        order.try_live,
        "live hop must remain enabled after embed miss"
    );
    assert!(
        order.try_api,
        "api hop must remain enabled after embed miss"
    );
}

#[test]
fn resolver_order_env_var_skips_embed() {
    // soldr#1211 — serialize with the sibling env-var test.
    let _guard = ENV_LOCK.lock().expect("env lock poisoned");
    // Set SOLDR_RESOLVER_ORDER=live,api and verify the parsed
    // ResolverOrder reports embed=false. Restore on the way out so
    // the sibling test (below) sees a clean env when its guard drops.
    let prior = std::env::var(RESOLVER_ORDER_ENV_VAR).ok();
    // SAFETY: env mutation; reset on the way out.
    std::env::set_var(RESOLVER_ORDER_ENV_VAR, "live,api");
    let order = ResolverOrder::from_env();
    assert!(!order.try_embed, "embed hop must be disabled by `live,api`");
    assert!(order.try_live);
    assert!(order.try_api);
    // Restore.
    match prior {
        Some(v) => std::env::set_var(RESOLVER_ORDER_ENV_VAR, v),
        None => std::env::remove_var(RESOLVER_ORDER_ENV_VAR),
    }
}

#[test]
fn resolver_order_env_var_skips_both_manifests() {
    // soldr#1211 — serialize with the sibling env-var test.
    let _guard = ENV_LOCK.lock().expect("env lock poisoned");
    let prior = std::env::var(RESOLVER_ORDER_ENV_VAR).ok();
    std::env::set_var(RESOLVER_ORDER_ENV_VAR, "api");
    let order = ResolverOrder::from_env();
    assert!(!order.try_embed);
    assert!(!order.try_live);
    assert!(order.try_api);
    match prior {
        Some(v) => std::env::set_var(RESOLVER_ORDER_ENV_VAR, v),
        None => std::env::remove_var(RESOLVER_ORDER_ENV_VAR),
    }
}
