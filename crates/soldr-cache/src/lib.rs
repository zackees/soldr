//! Compilation caching control plane.
//!
//! The actual artifact cache is not implemented yet, but this crate owns the
//! cache policy surface so the CLI and wrapper agree on whether caching is
//! enabled for a given invocation.

use std::ffi::OsStr;

/// Environment variable used to carry cache enable/disable state from the
/// front-door cargo command into wrapper mode.
pub const CACHE_ENABLED_ENV_VAR: &str = "SOLDR_CACHE_ENABLED";

/// Encoded environment value for an enabled cache invocation.
pub const CACHE_ENABLED_VALUE: &str = "1";

/// Encoded environment value for a disabled cache invocation.
pub const CACHE_DISABLED_VALUE: &str = "0";

pub fn cache_enabled_env_value(enabled: bool) -> &'static str {
    if enabled {
        CACHE_ENABLED_VALUE
    } else {
        CACHE_DISABLED_VALUE
    }
}

pub fn cache_enabled_from_env_var(value: Option<&OsStr>) -> bool {
    match value.and_then(OsStr::to_str) {
        None => true,
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
    }
}

pub fn cache_enabled_in_current_process() -> bool {
    cache_enabled_from_env_var(std::env::var_os(CACHE_ENABLED_ENV_VAR).as_deref())
}

#[cfg(test)]
mod tests {
    use super::{
        cache_enabled_env_value, cache_enabled_from_env_var, CACHE_DISABLED_VALUE,
        CACHE_ENABLED_VALUE,
    };
    use std::ffi::OsStr;

    #[test]
    fn cache_defaults_to_enabled_when_env_is_missing() {
        assert!(cache_enabled_from_env_var(None));
    }

    #[test]
    fn cache_control_parses_common_false_values() {
        for value in ["0", "false", "FALSE", "no", "off", ""] {
            assert!(
                !cache_enabled_from_env_var(Some(OsStr::new(value))),
                "expected {value:?} to disable cache"
            );
        }
    }

    #[test]
    fn cache_control_treats_other_values_as_enabled() {
        for value in ["1", "true", "yes", "unexpected"] {
            assert!(
                cache_enabled_from_env_var(Some(OsStr::new(value))),
                "expected {value:?} to enable cache"
            );
        }
    }

    #[test]
    fn cache_control_serializes_boolean_state() {
        assert_eq!(cache_enabled_env_value(true), CACHE_ENABLED_VALUE);
        assert_eq!(cache_enabled_env_value(false), CACHE_DISABLED_VALUE);
    }
}
