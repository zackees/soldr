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
        .map(|value| super::flag_value(&value))
        .unwrap_or(false)
}

/// Internal marker: this process's **stdout** carries a machine-readable
/// payload, so nothing else may be written to it.
///
/// soldr#2892: distinct from [`QUIET_DIAGNOSTICS_ENV_VAR`], and the
/// difference is the point.
///
/// `soldr env --json` suppresses *everything* because its callers commonly
/// merge stdout and stderr — a heartbeat on stderr would corrupt the parse
/// just as surely as installer output on stdout. `soldr toolchain ensure
/// --json` is used the other way round: the target-run lane redirects stdout
/// to a file and reads stderr as the job log, where rustup's progress and the
/// installer heartbeat are exactly what stop someone killing a multi-minute
/// first-time install.
///
/// So this marker moves child stdout to stderr rather than discarding it.
/// The payload stays parseable and nothing is lost.
///
/// Suppression wins when both are set: a caller that asked for silence
/// should not start receiving relocated output because it also asked for a
/// clean payload.
pub const PAYLOAD_STDOUT_ENV_VAR: &str = "SOLDR_INTERNAL_PAYLOAD_STDOUT";

/// Must child stdout be kept off this process's stdout?
pub fn stdout_carries_payload() -> bool {
    std::env::var(PAYLOAD_STDOUT_ENV_VAR)
        .map(|value| super::flag_value(&value))
        .unwrap_or(false)
}
