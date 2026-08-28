//! The `SOLDR_MANIFEST_DISABLE` escape hatch.
//!
//! Covers the #856 spec bullet:
//!
//!   * `manifest_disable_env_var_bypasses_lookup` — env var set, catalogue
//!     never fetched.
//!
//! soldr#2951: this file no longer needs — and no longer gets — a test binary
//! of its own. The catalogue cache is keyed on an immutable snapshot of the
//! resolved configuration, and the mutations below take
//! `common::catalogue_env::CatalogueEnvGuard`, which serialises the catalogue
//! variables against every other test in this binary and restores their prior
//! values on drop.

use std::time::Duration;

use crate::common::catalogue_env::CatalogueEnvGuard;
use soldr_cli::fetch::manifest_lookup::{
    get_or_fetch, MANIFEST_DISABLE_ENV_VAR, TOOLCHAIN_CATALOGUE_URL_ENV_VAR,
};

#[test]
fn manifest_disable_env_var_bypasses_lookup() {
    let env = CatalogueEnvGuard::acquire();
    env.set(MANIFEST_DISABLE_ENV_VAR, "1");
    // Point at a URL we never bind. If the disable env var weren't
    // honored, the fetch would hang up to the 30s budget (or fail
    // after the TCP timeout, which on Windows is several seconds).
    // With disable on, the fetch is skipped entirely and the
    // function returns instantly.
    env.set(
        TOOLCHAIN_CATALOGUE_URL_ENV_VAR,
        "http://127.0.0.1:1/never-bound.json",
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let start = std::time::Instant::now();
    let idx = rt.block_on(get_or_fetch());
    let elapsed = start.elapsed();

    assert_eq!(
        idx.entries.len(),
        0,
        "disabled manifest must produce an empty index"
    );
    // Generous threshold — the contract is "no network round trip"
    // but tokio runtime spin-up and cache contention can take a few
    // hundred ms on slow CI runners. The 30s manifest fetch timeout
    // is the regression we're guarding against.
    assert!(
        elapsed < Duration::from_secs(5),
        "disabled manifest must short-circuit without touching the network; took {elapsed:?}"
    );
}
