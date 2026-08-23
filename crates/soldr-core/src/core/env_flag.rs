//! The one definition of "is this environment variable on" (soldr#2740).
//!
//! # Why this module exists
//!
//! soldr had five hand-rolled truthy parsers plus two inline spellings,
//! mutually disagreeing, and the disagreement landed on trust boundaries:
//!
//! * `SOLDR_USE_SYSTEM_CMAKE=false` **enabled** the flag, routing around the
//!   pinned, sha256-verified SDK to an unpinned system tool. That parser's
//!   falsy set was `{empty, "0"}` -- it never excluded the word `false`.
//! * `ZCCACHE_DISABLE=off` **disabled** the cache. That parser excluded `0`
//!   and `false` but not `no`/`off`, so it disagreed with itself on a
//!   variable whose name is the word "disable".
//!
//! Each parser looked correct in isolation. The defect existed only *between*
//! them, which is why it survived review for so long.
//!
//! # Two populations, two rules
//!
//! One rule cannot serve both, and picking either one globally reintroduces a
//! bug. The split is by *who owns the variable's value space*:
//!
//! | | Rule | Unknown value | For |
//! |---|---|---|---|
//! | [`flag`] | allowlist | **off** | `SOLDR_*`, `ZCCACHE_*` -- ours |
//! | [`foreign_flag`] | denylist | **on** | `RUSTC_BOOTSTRAP`, `GITHUB_ACTIONS`, `NO_COLOR` |
//!
//! **Owned switches must default unknown to off.** `ZCCACHE_DISABLE=maybe`
//! silently turning the cache off is the failure mode, and it is worse for
//! anything named `NO_*` or `*_DISABLE`, where an unrecognised value must
//! never mean "disable". `SOLDR_NO_BOOTSTRAP` is the sharpest case: unknown
//! reading as "on" would leave a user with no toolchain.
//!
//! **Foreign variables must default unknown to on**, because we do not get to
//! define their value space. `RUSTC_BOOTSTRAP`'s real convention is `1` *or a
//! crate name* (`RUSTC_BOOTSTRAP=serde`); an allowlist would read every
//! crate-name form as unset. This is also why "reject unrecognised values
//! loudly" was considered and rejected -- it would hard-fail soldr on a
//! correct thing for a user to set.
//!
//! # Choosing
//!
//! Per *variable*, at its call site, and deliberately. If you are adding a
//! `SOLDR_*` switch, you want [`flag`]. Reach for [`foreign_flag`] only when
//! the variable is defined by something outside this project.

/// Spellings that turn an owned switch on. Nothing else does.
const OWNED_ON: &[&str] = &["1", "true", "yes", "on"];

/// Spellings that explicitly turn something off.
///
/// The empty string is deliberately NOT here. `VAR=` means *absent*, not
/// *explicitly off* -- `SOLDR_MSVC_DISCOVERY=` must not read as "the user
/// opted out of discovery". Callers that do want empty to count as off say
/// so themselves, via [`foreign_flag_value`].
const EXPLICIT_OFF: &[&str] = &["0", "false", "no", "off"];

/// Is `value` an "on" spelling for a soldr-owned switch?
///
/// Pure, so the contract is unit-testable without touching the process
/// environment. Trimmed and case-insensitive; anything unrecognised is off.
#[must_use]
pub fn flag_value(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    OWNED_ON.contains(&value.as_str())
}

/// Is `value` an explicit "off" spelling?
///
/// The third intent, for a soldr-owned switch that defaults **on** and is
/// only turned off explicitly -- `cache_enabled_from_env_var` is the case:
/// absent means enabled, so [`flag_value`] would invert it and disable the
/// cache on an unrecognised value.
///
/// Three intents, two sets, one definition each:
///
/// | Variable shape | Read as |
/// |---|---|
/// | owned, default off | [`flag`] |
/// | owned, default on | `!is_off_value(..)` |
/// | foreign | [`foreign_flag`] |
#[must_use]
pub fn is_off_value(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    EXPLICIT_OFF.contains(&value.as_str())
}

/// Is `value` an "on" spelling for a foreign variable?
///
/// Pure counterpart to [`foreign_flag`]. Trimmed and case-insensitive;
/// anything unrecognised is **on**, because the owning project -- not soldr --
/// defines what its values mean.
#[must_use]
pub fn foreign_flag_value(value: &str) -> bool {
    !value.trim().is_empty() && !is_off_value(value)
}

/// Read a soldr-owned switch. Absent or unrecognised is off.
#[must_use]
pub fn flag(key: &str) -> bool {
    std::env::var(key).is_ok_and(|value| flag_value(&value))
}

/// Read a variable owned by something outside soldr. Absent is off; present
/// with anything but a recognised falsy spelling is on.
#[must_use]
pub fn foreign_flag(key: &str) -> bool {
    std::env::var(key).is_ok_and(|value| foreign_flag_value(&value))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spelling matrix, in one place, because the whole point of this
    /// module is that there is exactly one answer per column.
    #[test]
    fn the_two_rules_agree_on_the_canonical_spellings_and_differ_elsewhere() {
        // Everyone agrees these are on.
        for value in ["1", "true", "TRUE", "Yes", "on", " 1 ", "ON"] {
            assert!(flag_value(value), "owned: {value:?} should be on");
            assert!(foreign_flag_value(value), "foreign: {value:?} should be on");
        }
        // Everyone agrees these are off.
        for value in ["0", "false", "FALSE", "no", "off", "OFF", "", "  "] {
            assert!(!flag_value(value), "owned: {value:?} should be off");
            assert!(
                !foreign_flag_value(value),
                "foreign: {value:?} should be off"
            );
        }
        // The whole reason there are two rules: an unrecognised value.
        for value in ["maybe", "enabled", "serde", "2"] {
            assert!(
                !flag_value(value),
                "owned: {value:?} must be off -- an unrecognised value must \
                 never enable a soldr switch, least of all a NO_*/`*_DISABLE` one"
            );
            assert!(
                foreign_flag_value(value),
                "foreign: {value:?} must be on -- we do not define this \
                 variable's value space (RUSTC_BOOTSTRAP=<crate name>)"
            );
        }
    }

    /// soldr#2740's Tier 1 defect, pinned so it cannot come back: the word
    /// `false` must never enable a switch that routes around the pinned SDK.
    #[test]
    fn the_word_false_never_enables_an_owned_switch() {
        assert!(!flag_value("false"));
        assert!(!flag_value("False"));
        assert!(!flag_value(" false "));
    }

    /// soldr#2740's Tier 2 defect: `off` on a `NO_*` variable must mean
    /// "do not", not "do".
    #[test]
    fn off_and_no_never_enable_an_owned_switch() {
        for value in ["off", "no", "OFF", "No", " off "] {
            assert!(!flag_value(value), "{value:?}");
        }
    }

    /// The motivating foreign case, which rules out an allowlist and rules
    /// out erroring on unrecognised input.
    #[test]
    fn a_rustc_bootstrap_crate_name_reads_as_on() {
        assert!(foreign_flag_value("serde"));
        assert!(foreign_flag_value("my_crate"));
        assert!(foreign_flag_value("1"));
        // ...but an explicit falsy spelling still turns it off.
        assert!(!foreign_flag_value("0"));
    }

    /// Empty is absent, not "explicitly off".
    ///
    /// `SOLDR_MSVC_DISCOVERY=` must not read as "the user opted out of
    /// discovery" -- an opt-out check asks whether they said *off*, and an
    /// empty value says nothing. A caller that does want empty to count as
    /// off uses `foreign_flag_value`, which spells that out.
    #[test]
    fn empty_is_absent_not_explicitly_off() {
        assert!(!is_off_value(""), "empty is not an explicit off");
        assert!(!is_off_value("   "), "blank is not an explicit off");
        for value in ["0", "false", "no", "off", "OFF", " off "] {
            assert!(is_off_value(value), "{value:?} is an explicit off");
        }
        // ...and it still does not enable anything.
        assert!(!flag_value(""));
        assert!(!foreign_flag_value(""));
    }

    #[test]
    fn an_absent_variable_is_off_under_both_rules() {
        let key = "SOLDR_ENV_FLAG_TEST_DEFINITELY_ABSENT_2740";
        assert!(!flag(key));
        assert!(!foreign_flag(key));
    }
}
