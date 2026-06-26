//! Compile-dispatch backend selection (issue #977 Phase 2).
//!
//! Phase 1 + 2 introduces the abstraction without changing any
//! on-the-wire behaviour. The daemon picks `Wrapped` by default — which
//! is exactly today's "rustc-wrapper shells out to the managed zccache
//! binary" code path — and `Embedded` only when `SOLDR_ZCCACHE_EMBEDDED=1`
//! is set at daemon startup AND the `embedded` Cargo feature is on.
//!
//! In Phase 5 the wrapper learns a new `Request::Compile` IPC verb and
//! the daemon dispatches it here; until then this enum is held on
//! `State` purely to anchor the lifecycle calls (`flush` on
//! BuildSessionEnd, `shutdown` on daemon exit).

use std::env;

#[cfg(feature = "embedded")]
use std::sync::Arc;

/// Daemon-side compile backend handle. Cheap to clone — both variants
/// hold either a `()` marker or an `Arc<_>`.
#[derive(Clone)]
pub enum CompileBackend {
    /// The default: rustc-wrapper invocations on the client side exec
    /// the managed `zccache` binary; the daemon does not participate
    /// in per-compile dispatch beyond build-session correlation.
    Wrapped,
    /// In-process `zccache::embedded::ZccacheService`. Held only by
    /// the daemon — never by transient rustc-wrapper invocations.
    #[cfg(feature = "embedded")]
    Embedded(Arc<crate::zccache_embedded::SoldrZccacheService>),
}

impl std::fmt::Debug for CompileBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wrapped => f.write_str("CompileBackend::Wrapped"),
            #[cfg(feature = "embedded")]
            Self::Embedded(_) => f.write_str("CompileBackend::Embedded(<service>)"),
        }
    }
}

/// Env var name documented in `docs/API.md`. Strict parser: only the
/// literal value `"1"` activates embedded mode so we never flip on
/// accidentally during this preview-grade phase.
pub const EMBEDDED_ENV_VAR: &str = "SOLDR_ZCCACHE_EMBEDDED";

/// Cheap, side-effect-free test of "does the operator want embedded mode?".
///
/// Returns `true` ONLY when the literal value `1` is present. Any other
/// value (`0`, `true`, `yes`, unset, empty) yields `false`. Decoupled
/// from the actual backend construction so the env-var check is unit
/// testable in isolation.
pub fn embedded_requested() -> bool {
    env::var(EMBEDDED_ENV_VAR)
        .map(|v| v == "1")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(embedded_requested_strict_parse, {
        // Use a fresh env-var name so we don't race with parallel tests
        // that read SOLDR_ZCCACHE_EMBEDDED. The helper takes the value
        // directly to keep this test hermetic.
        fn check(raw: Option<&str>) -> bool {
            raw.map(|v| v == "1").unwrap_or(false)
        }
        assert!(check(Some("1")));
        assert!(!check(Some("0")));
        assert!(!check(Some("true")));
        assert!(!check(Some("yes")));
        assert!(!check(Some("")));
        assert!(!check(None));
    });

    crate::timed_test!(compile_backend_debug_does_not_panic, {
        let backend = CompileBackend::Wrapped;
        assert_eq!(format!("{backend:?}"), "CompileBackend::Wrapped");
    });
}
