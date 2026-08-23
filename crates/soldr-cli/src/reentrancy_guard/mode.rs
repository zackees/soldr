//! Enforcement-mode parsing for the re-entrancy guard (soldr#2739).
//!
//! Split out of `reentrancy_guard.rs` to stay under the 1000-line
//! production ceiling; the parsing is a self-contained concern.

use super::{GUARD_MODE_ENFORCING, GUARD_MODE_OFF};

/// Resolved enforcement mode.
///
/// Deliberately not routed through `soldr_core::core::env_flag` (soldr#2740):
/// that accessor answers a boolean question, and this one is tri-state --
/// enforce, off, or *invalid*. Collapsing the third case into either boolean
/// is precisely the failure this flip is meant to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardMode {
    Enforcing,
    Off,
}

impl GuardMode {
    /// `None` (unset) enforces. So does an empty/whitespace value: `VAR=`
    /// means the operator did not express a preference, which is the default,
    /// and must not read as an opt-out.
    pub fn from_env_value(value: Option<&str>) -> Result<Self, String> {
        let Some(raw) = value else {
            return Ok(Self::Enforcing);
        };
        let normalized = raw.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Ok(Self::Enforcing);
        }
        if normalized == GUARD_MODE_OFF {
            return Ok(Self::Off);
        }
        if GUARD_MODE_ENFORCING.contains(&normalized.as_str()) {
            return Ok(Self::Enforcing);
        }
        Err(raw.to_string())
    }

    pub fn is_enforcing(self) -> bool {
        matches!(self, Self::Enforcing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_guard_variable_enforces() {
        assert_eq!(GuardMode::from_env_value(None), Ok(GuardMode::Enforcing));
    }

    #[test]
    fn only_off_disables_enforcement() {
        assert_eq!(
            GuardMode::from_env_value(Some("off")),
            Ok(GuardMode::Off),
            "`off` is the documented hatch"
        );
        for spelling in ["OFF", " off ", "Off"] {
            assert_eq!(
                GuardMode::from_env_value(Some(spelling)),
                Ok(GuardMode::Off),
                "{spelling} should normalize to the hatch"
            );
        }
    }

    #[test]
    fn the_historical_strict_spelling_still_enforces() {
        // soldr#2566 put `strict` in eleven workflows and ci/perf_local.py.
        // Those exports are redundant after the flip, never wrong.
        for spelling in ["strict", "STRICT", " strict ", "on"] {
            assert_eq!(
                GuardMode::from_env_value(Some(spelling)),
                Ok(GuardMode::Enforcing),
                "{spelling} must keep enforcing"
            );
        }
    }

    #[test]
    fn an_empty_value_is_absent_not_an_opt_out() {
        for spelling in ["", "   "] {
            assert_eq!(
                GuardMode::from_env_value(Some(spelling)),
                Ok(GuardMode::Enforcing),
                "{spelling:?} expresses no preference, so the default applies"
            );
        }
    }

    #[test]
    fn an_unrecognised_value_is_an_error_not_a_fallback() {
        // The whole point of the flip: a typo must not disable the guard.
        for spelling in ["strck", "false", "0", "1", "yes", "disabled", "no"] {
            assert_eq!(
                GuardMode::from_env_value(Some(spelling)),
                Err(spelling.to_string()),
                "{spelling} must be rejected rather than guessed at"
            );
        }
    }

    #[test]
    fn falsey_spellings_are_not_silent_opt_outs() {
        // soldr#2740 found `SOLDR_USE_SYSTEM_CMAKE=false` *enabling* a switch.
        // Here `false` is an error, so it can never mean the opposite of what
        // it looks like.
        assert!(GuardMode::from_env_value(Some("false")).is_err());
        assert!(GuardMode::from_env_value(Some("0")).is_err());
    }
}
