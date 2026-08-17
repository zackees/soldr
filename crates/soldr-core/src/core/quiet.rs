//! Process-scoped diagnostic suppression for machine-readable modes
//! (soldr#2304 × soldr#2554).
//!
//! `soldr env --json` (and any future machine-readable verb) materializes
//! the blessed toolchain exactly like `soldr prepare` — but its caller
//! parses this process's output, very possibly with stdout and stderr
//! merged. Progress diagnostics (syslib fetch lines, child installer
//! output) would corrupt that parse, which is the soldr#2554 contract:
//! `--json` suppresses every unsolicited diagnostic.
//!
//! The flag travels as an environment variable so child-facing layers
//! (the installer tee) and other crates observe it without plumbing a
//! parameter through every fetch signature. It is process-internal:
//! never documented as a user surface, set only by machine-readable
//! verbs around their preparation phase.

/// Internal marker: when set truthy, progress diagnostics stay silent.
pub const QUIET_DIAGNOSTICS_ENV_VAR: &str = "SOLDR_INTERNAL_QUIET_DIAGNOSTICS";

/// Should progress diagnostics be suppressed in this process?
pub fn diagnostics_suppressed() -> bool {
    std::env::var(QUIET_DIAGNOSTICS_ENV_VAR)
        .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}
