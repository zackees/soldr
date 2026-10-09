//! Broker debug switches, defined once (soldr#3609).
//!
//! Owned switches, so they are read through [`crate::core::flag`]: `=0` and
//! `=false` leave them off. `broker_server` and `broker_launcher` both read
//! [`broker_debug_enabled`] instead of spelling the variable themselves.

/// Verbose broker lifecycle tracing.
pub(crate) const BROKER_DEBUG_ENV_VAR: &str = "SOLDR_BROKER_DEBUG";

/// Test hook (debug builds only): refuse the broker handle handoff.
#[cfg(debug_assertions)]
pub(crate) const TEST_DISABLE_HANDOFF_ENV_VAR: &str = "SOLDR_TEST_BROKER_DISABLE_HANDOFF";

/// Is [`BROKER_DEBUG_ENV_VAR`] on? The one reader.
pub(crate) fn broker_debug_enabled() -> bool {
    crate::core::flag(BROKER_DEBUG_ENV_VAR)
}
