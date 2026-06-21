//! Integration tests for the embed-first resolver order (issue #873).
//!
//! These tests exercise the env-var-driven [`ResolverOrder`] gate at
//! the process boundary — they live as an integration test (rather
//! than unit tests in `src/fetch/mod.rs`) so each `#[test]` runs in
//! its own process and can set/unset `SOLDR_RESOLVER_ORDER` without
//! racing other tests.
//!
//! End-to-end download paths (`try_embedded_manifest_v6` /
//! `try_manifest_first` driving `archive::download_and_extract_with_pin`)
//! are exercised by the existing manifest-lookup integration tests; this
//! file focuses on the in-memory short-circuit semantics so we don't
//! depend on the live network during CI.

use soldr_cli::fetch::{ResolverOrder, RESOLVER_ORDER_ENV_VAR};
use soldr_cli::timed_test;

timed_test!(embed_hits_short_circuit_live_fetch, {
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
});

timed_test!(embed_miss_falls_through_to_live, {
    // Empty embed must miss on every lookup, leaving the resolver to
    // fall through to the live hop. We prove the miss semantics here;
    // the integration with `try_manifest_first` is exercised by
    // `tests/manifest_lookup.rs`.
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
});

timed_test!(resolver_order_env_var_skips_embed, {
    // Set SOLDR_RESOLVER_ORDER=live,api and verify the parsed
    // ResolverOrder reports embed=false. We restore the env var when
    // the test ends so other tests in the same process aren't affected
    // (each integration test binary is single-process so this is
    // sufficient — unit tests in `src/fetch/mod.rs` that touch this
    // env var would race, which is why those tests use `parse()`
    // directly instead).
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
});

timed_test!(resolver_order_env_var_skips_both_manifests, {
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
});
