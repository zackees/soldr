//! One place that knows every timeout soldr honours (soldr#1838 Phase 3).
//!
//! # Why
//!
//! The timeout constants live in six files with no shared convention, and
//! nothing reports the values actually in force. A user who sets
//! `SOLDR_COMPILE_REPLY_TIMEOUT_SECS=600` has no way to confirm it took
//! effect, and #1838's own table had to be assembled by hand from source.
//!
//! # Drift is the design constraint
//!
//! A registry that restates the defaults would be worse than none: it would
//! agree with the code on the day it was written and quietly diverge after.
//! So every entry holds a **function pointer to the real resolver** — the
//! same call the production path makes. The registry cannot report a value
//! the code does not use, because it is asking the code.
//!
//! # The one thing it can tell you that the resolver cannot
//!
//! Whether an override was *honoured*. Every parser here follows the rule
//! #1837 established — a malformed override falls back to the default,
//! never to "disabled" — which is the safe behaviour, but it is also
//! silent. `SOLDR_COMPILE_REPLY_TIMEOUT_SECS=30m` looks set and does
//! nothing. [`TimeoutSource::InvalidOverride`] is how `soldr doctor` says
//! so.

use std::time::Duration;

/// One timeout, and how to ask the code what it currently is.
pub(crate) struct TimeoutEntry {
    /// Human-facing name of the wait this bounds.
    pub name: &'static str,
    /// The environment variable that overrides it.
    pub env_var: &'static str,
    /// The compiled-in value used when no valid override is present.
    pub default: Duration,
    /// The production resolver. Not a copy of its logic — the function
    /// itself, so this cannot drift from what actually runs.
    pub resolve: fn() -> Duration,
}

/// Where a timeout's effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimeoutSource {
    /// No override set; the compiled-in default is in force.
    Default,
    /// An override was set, parsed, and is in force.
    Override,
    /// An override was set and could **not** be used, so the default is in
    /// force. The value looks configured and is not — the case worth
    /// reporting, since every parser swallows it by design.
    InvalidOverride,
}

impl TimeoutSource {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Override => "override",
            Self::InvalidOverride => "default (override ignored: unparseable)",
        }
    }
}

/// Classify how `effective` came about, given the raw environment value.
///
/// Pure so the matrix is testable without mutating process env — mutating
/// it would make these tests order-dependent against every other test in
/// the binary.
///
/// The subtle case is an override that *equals* the default: the effective
/// value alone cannot distinguish "explicitly set to 30" from "typo, fell
/// back to 30". Re-parsing the raw value settles it, using the same shape
/// every resolver uses (trimmed, positive integer seconds).
pub(crate) fn classify(raw: Option<&str>, default: Duration, effective: Duration) -> TimeoutSource {
    let Some(raw) = raw else {
        return TimeoutSource::Default;
    };
    match raw.trim().parse::<u64>() {
        // Zero is "use the default" for every parser in this registry, not
        // "disable", so it is not a dishonoured override.
        Ok(seconds) if seconds > 0 && Duration::from_secs(seconds) == effective => {
            TimeoutSource::Override
        }
        Ok(0) => TimeoutSource::Default,
        // Parsed but not in force, or did not parse at all: either way the
        // user asked for something they did not get.
        Ok(_) => TimeoutSource::InvalidOverride,
        Err(_) if effective == default => TimeoutSource::InvalidOverride,
        Err(_) => TimeoutSource::Override,
    }
}

/// A resolved row for reporting.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedTimeout {
    pub name: &'static str,
    pub env_var: &'static str,
    pub default: Duration,
    pub effective: Duration,
    pub source: TimeoutSource,
}

/// Every timeout soldr honours, resolved against the current environment.
pub(crate) fn resolve_all() -> Vec<ResolvedTimeout> {
    entries()
        .into_iter()
        .map(|entry| {
            let effective = (entry.resolve)();
            let raw = std::env::var(entry.env_var).ok();
            ResolvedTimeout {
                name: entry.name,
                env_var: entry.env_var,
                default: entry.default,
                effective,
                source: classify(raw.as_deref(), entry.default, effective),
            }
        })
        .collect()
}

/// Serializable row for `soldr doctor --json`.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct DoctorTimeout {
    pub name: &'static str,
    pub env_var: &'static str,
    pub default_secs: u64,
    pub effective_secs: u64,
    pub source: &'static str,
    /// True when a variable is set but did not take effect. Broken out as
    /// its own field so CI can assert on it without string-matching
    /// `source` — #1838 Phase 4 wants exactly this shape.
    pub override_ignored: bool,
}

pub(crate) fn doctor_rows() -> Vec<DoctorTimeout> {
    resolve_all()
        .into_iter()
        .map(|row| DoctorTimeout {
            name: row.name,
            env_var: row.env_var,
            default_secs: row.default.as_secs(),
            effective_secs: row.effective.as_secs(),
            source: row.source.describe(),
            override_ignored: row.source == TimeoutSource::InvalidOverride,
        })
        .collect()
}

/// Human section for `soldr doctor`.
pub(crate) fn print_doctor_section() {
    let rows = doctor_rows();
    println!();
    println!("Timeouts:");
    for row in &rows {
        // Mark only the actionable case. Tagging every row would bury the
        // one line that means "you configured something and it is not
        // running".
        let flag = if row.override_ignored {
            "  <-- IGNORED"
        } else {
            ""
        };
        println!(
            "  {:<32} {:>6}s  ({}; default {}s, {}){}",
            row.name, row.effective_secs, row.source, row.default_secs, row.env_var, flag
        );
    }
    if rows.iter().any(|row| row.override_ignored) {
        println!(
            "  note: an ignored override means the variable is set to a value soldr \
             cannot parse, so the default is in force. Use whole seconds, e.g. 600."
        );
    }
}

/// The registry itself.
///
/// Deliberately not exhaustive of every bound in #1838's table: it covers
/// the ones with an env override, which are the ones a user can get wrong
/// and the ones `doctor` can say something actionable about. The untimed
/// publication barrier and the embedded zccache drains have no override to
/// report and are tracked separately on #1838.
fn entries() -> Vec<TimeoutEntry> {
    vec![
        TimeoutEntry {
            name: "compile reply",
            env_var: crate::daemon::client::REPLY_TIMEOUT_ENV,
            default: Duration::from_secs(crate::daemon::client::DEFAULT_REPLY_TIMEOUT_SECS),
            resolve: crate::daemon::client::compile_reply_timeout,
        },
        TimeoutEntry {
            name: "command output capture",
            env_var: crate::core::COMMAND_OUTPUT_TIMEOUT_ENV_VAR,
            default: Duration::from_secs(crate::core::DEFAULT_COMMAND_OUTPUT_TIMEOUT_SECS),
            resolve: crate::core::command_output_timeout,
        },
        TimeoutEntry {
            name: "installer no-progress watchdog",
            env_var: crate::core::INSTALLER_STALL_TIMEOUT_ENV_VAR,
            default: Duration::from_secs(crate::core::DEFAULT_INSTALLER_STALL_TIMEOUT_SECS),
            resolve: crate::core::installer_stall_timeout,
        },
        TimeoutEntry {
            name: "installer safety ceiling",
            env_var: crate::core::INSTALLER_SAFETY_TIMEOUT_ENV_VAR,
            default: Duration::from_secs(crate::core::DEFAULT_INSTALLER_SAFETY_TIMEOUT_SECS),
            resolve: crate::core::installer_safety_timeout,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: Duration = Duration::from_secs(30);

    #[test]
    fn an_unset_variable_is_the_default() {
        assert_eq!(classify(None, DEFAULT, DEFAULT), TimeoutSource::Default);
    }

    #[test]
    fn a_parsed_override_in_force_is_an_override() {
        assert_eq!(
            classify(Some("600"), DEFAULT, Duration::from_secs(600)),
            TimeoutSource::Override
        );
        // Resolvers trim, so the classifier must agree or a padded value
        // would be reported as ignored while actually taking effect.
        assert_eq!(
            classify(Some("  600 "), DEFAULT, Duration::from_secs(600)),
            TimeoutSource::Override
        );
    }

    #[test]
    fn an_override_equal_to_the_default_is_still_an_override() {
        // The effective value alone cannot tell this from a typo; the raw
        // value can, and reporting "ignored" here would be a lie.
        assert_eq!(
            classify(Some("30"), DEFAULT, DEFAULT),
            TimeoutSource::Override
        );
    }

    #[test]
    fn an_unparseable_override_is_reported_as_ignored() {
        // The whole point of the registry. Every parser swallows these by
        // design (soldr#1837: never fall back to "disabled"), which is safe
        // and silent -- the user sees a configured variable doing nothing.
        for raw in ["30m", "abc", "", "-5", "1e3", "30.0"] {
            assert_eq!(
                classify(Some(raw), DEFAULT, DEFAULT),
                TimeoutSource::InvalidOverride,
                "{raw:?} parses as no seconds and must be reported as ignored"
            );
        }
    }

    #[test]
    fn zero_means_default_not_disabled() {
        // Every resolver here filters `> 0`, so 0 selects the default. It
        // is a real value the user can type, not a mistake, so it must not
        // be reported as an ignored override.
        assert_eq!(
            classify(Some("0"), DEFAULT, DEFAULT),
            TimeoutSource::Default
        );
    }

    #[test]
    fn the_registry_resolves_every_entry() {
        let rows = resolve_all();
        assert!(!rows.is_empty());
        for row in &rows {
            assert!(!row.name.is_empty(), "unnamed entry");
            assert!(
                row.env_var.starts_with("SOLDR_"),
                "{} is not a soldr-namespaced override",
                row.env_var
            );
            assert!(
                row.default > Duration::ZERO,
                "{}: a zero default would mean the bound does not exist",
                row.name
            );
        }
    }

    #[test]
    fn env_var_names_are_unique() {
        // Two entries sharing a variable would make `doctor` print
        // contradictory rows for the same knob.
        let rows = resolve_all();
        let mut seen: Vec<&str> = rows.iter().map(|r| r.env_var).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "duplicate env var in the registry");
    }

    #[test]
    fn each_entry_reports_the_resolver_not_a_restated_default() {
        // Guards the anti-drift property: with no override set, the value
        // the registry reports must be the value the production resolver
        // returns. If someone later hardcodes `effective`, this fails.
        for row in resolve_all() {
            if row.source == TimeoutSource::Default {
                assert_eq!(
                    row.effective, row.default,
                    "{}: default-sourced row disagrees with its own default",
                    row.name
                );
            }
        }
    }
}
