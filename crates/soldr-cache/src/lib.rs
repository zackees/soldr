//! zccache integration surface for soldr.
//!
//! soldr owns the build UX and cache policy, while zccache provides the actual
//! compiler-cache engine and daemon.

use serde::Deserialize;
use soldr_core::SoldrPaths;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// Environment variable used to carry cache enable/disable state from the
/// front-door cargo command into child processes.
pub const CACHE_ENABLED_ENV_VAR: &str = "SOLDR_CACHE_ENABLED";

/// Encoded environment value for an enabled cache invocation.
pub const CACHE_ENABLED_VALUE: &str = "1";

/// Encoded environment value for a disabled cache invocation.
pub const CACHE_DISABLED_VALUE: &str = "0";

/// Per-build session identifier recognized by zccache.
pub const ZCCACHE_SESSION_ID_ENV_VAR: &str = "ZCCACHE_SESSION_ID";

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

pub fn zccache_dir(paths: &SoldrPaths) -> PathBuf {
    paths.cache.join("zccache")
}

pub fn parse_zccache_session_id(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(response) = serde_json::from_str::<SessionStartResponse>(trimmed) {
        if !response.session_id.trim().is_empty() {
            return Some(response.session_id);
        }
    }

    for line in trimmed.lines() {
        let line = line.trim();
        for prefix in [
            "ZCCACHE_SESSION_ID=",
            "export ZCCACHE_SESSION_ID=",
            "$env:ZCCACHE_SESSION_ID=",
        ] {
            if let Some(value) = line.strip_prefix(prefix) {
                let value = value.trim().trim_matches('"').trim_matches('\'');
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }

    None
}

pub fn session_journal_path(zccache_dir: &Path) -> PathBuf {
    zccache_dir.join("logs").join("last-session.jsonl")
}

#[derive(Debug, Deserialize)]
struct SessionStartResponse {
    session_id: String,
}

#[cfg(test)]
mod tests {
    use super::{
        cache_enabled_env_value, cache_enabled_from_env_var, parse_zccache_session_id,
        session_journal_path, zccache_dir, CACHE_DISABLED_VALUE, CACHE_ENABLED_VALUE,
    };
    use soldr_core::SoldrPaths;
    use std::{ffi::OsStr, path::Path};

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

    #[test]
    fn zccache_dir_lives_under_soldr_cache_root() {
        let paths = SoldrPaths::with_root(Path::new("C:\\soldr-root").to_path_buf());
        assert_eq!(zccache_dir(&paths), paths.root.join("cache").join("zccache"));
    }

    #[test]
    fn parses_json_session_start_output() {
        let session_id = parse_zccache_session_id(
            r#"{"session_id":"08f063c0-5f01-4c92-aec1-3f304d9224d0","started_at":1776141813}"#,
        );
        assert_eq!(
            session_id.as_deref(),
            Some("08f063c0-5f01-4c92-aec1-3f304d9224d0")
        );
    }

    #[test]
    fn parses_shell_style_session_start_output() {
        let session_id = parse_zccache_session_id(
            "export ZCCACHE_SESSION_ID=08f063c0-5f01-4c92-aec1-3f304d9224d0",
        );
        assert_eq!(
            session_id.as_deref(),
            Some("08f063c0-5f01-4c92-aec1-3f304d9224d0")
        );
    }

    #[test]
    fn session_journal_path_uses_logs_directory() {
        let path = session_journal_path(Path::new("C:\\soldr-root\\cache\\zccache"));
        assert_eq!(path, Path::new("C:\\soldr-root\\cache\\zccache").join("logs").join("last-session.jsonl"));
    }
}
