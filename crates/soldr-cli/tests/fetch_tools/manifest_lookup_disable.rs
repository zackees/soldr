//! `SOLDR_MANIFEST_DISABLE` escape-hatch coverage.
//!
//! This category is one Cargo test binary, so its process-environment tests
//! share a lock and RAII restore scope with the URL-override cases.
//!
//! Covers the #856 spec bullet:
//!
//!   * `manifest_disable_env_var_bypasses_lookup` — env var set,
//!     manifest never fetched.

use std::time::Duration;

use crate::manifest_lookup_env::{lock, EnvScope};

#[test]
fn manifest_disable_env_var_bypasses_lookup() {
    let _lock = lock();
    let _env = EnvScope::capture();
    std::env::set_var("SOLDR_MANIFEST_DISABLE", "1");
    // Point at a URL we never bind. If the disable env var weren't
    // honored, the fetch would hang up to the 30s budget (or fail
    // after the TCP timeout, which on Windows is several seconds).
    // With disable on, the fetch is skipped entirely and the
    // function returns instantly.
    std::env::set_var(
        "SOLDR_TOOLCHAIN_CATALOGUE_URL",
        "http://127.0.0.1:1/never-bound.json",
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let start = std::time::Instant::now();
    let idx = rt.block_on(soldr_cli::fetch::manifest_lookup::get_or_fetch());
    let elapsed = start.elapsed();

    assert_eq!(
        idx.entries.len(),
        0,
        "disabled manifest must produce an empty index"
    );
    // Generous threshold — the contract is "no network round trip" but tokio
    // runtime spin-up can take a few hundred ms on slow CI runners. The 30s
    // manifest fetch timeout is the regression we're guarding against.
    assert!(
        elapsed < Duration::from_secs(5),
        "disabled manifest must short-circuit without touching the network; took {elapsed:?}"
    );
}
