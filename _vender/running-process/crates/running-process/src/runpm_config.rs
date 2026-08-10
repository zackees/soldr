//! TOML config file parsing for the `runpm` PM2-style supervisor CLI.
//!
//! Phase 5 of #222 (issue #428). A single `runpm.toml` may carry any
//! number of `[[app]]` tables — `runpm start --config <path>` iterates
//! them and registers each one with the daemon.
//!
//! Example:
//!
//! ```toml
//! [[app]]
//! name = "web"
//! cmd  = ["node", "server.js"]
//! cwd  = "/srv/web"
//! env  = { NODE_ENV = "production" }
//! autorestart      = true
//! max_restarts     = 10
//! restart_delay_ms = 1000
//! min_uptime_ms    = 2000
//! ```
//!
//! Relative `cwd` values are resolved against the config file's parent
//! directory; absolute paths pass through unchanged. Empty `cmd` arrays
//! and duplicate `name` entries are rejected with a clear error message
//! so a typo in one entry doesn't strand the rest of the batch.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Top-level shape of a `runpm.toml` config file.
#[derive(Deserialize, Debug, Default, Clone)]
pub struct RunpmConfig {
    /// Every `[[app]]` table in the file. Missing entirely is fine —
    /// an empty config is a valid (no-op) batch.
    #[serde(default)]
    pub app: Vec<AppConfig>,
}

/// One `[[app]]` table inside a `runpm.toml` config file.
///
/// Field-for-field shape of [`crate::proto::daemon::ServiceConfig`]
/// minus the daemon-side defaults; the `cwd` field is resolved against
/// the config file's parent dir by [`RunpmConfig::resolve_cwd`].
#[derive(Deserialize, Debug, Clone)]
pub struct AppConfig {
    /// Service name — must be unique within the file.
    pub name: String,
    /// Executable plus arguments. Empty `cmd` is rejected.
    pub cmd: Vec<String>,
    /// Working directory. Relative paths are resolved against the
    /// config file's parent directory by [`RunpmConfig::resolve_cwd`].
    #[serde(default)]
    pub cwd: Option<String>,
    /// Environment variables overlaid on the daemon's environment.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Auto-restart on exit. Defaults to `true` to match PM2 ergonomics.
    #[serde(default = "default_true")]
    pub autorestart: bool,
    /// Maximum restart attempts. `None` => use daemon default (unlimited).
    #[serde(default)]
    pub max_restarts: Option<u32>,
    /// Backoff between restarts (milliseconds). `None` => use daemon
    /// default. Values are capped at `u32::MAX` ms (~49 days) by the
    /// daemon wire format.
    #[serde(default)]
    pub restart_delay_ms: Option<u32>,
    /// Minimum uptime (milliseconds) before the restart counter resets.
    /// Capped at `u32::MAX` ms by the daemon wire format.
    #[serde(default)]
    pub min_uptime_ms: Option<u32>,
    /// Grace period (milliseconds) during `runpm stop` before
    /// SIGKILL/TerminateProcess. Capped at `u32::MAX` ms.
    #[serde(default)]
    pub kill_timeout_ms: Option<u32>,
}

fn default_true() -> bool {
    true
}

/// Errors raised by the runpm TOML config loader.
#[derive(Debug, Error)]
pub enum RunpmConfigError {
    /// The config file could not be read from disk.
    #[error("failed to read runpm config {path}: {source}")]
    Read {
        /// Path the loader tried to read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file was not valid TOML / did not match the expected schema.
    ///
    /// The underlying error is boxed because `toml::de::Error` is large
    /// (~128 bytes); inlining it tripped clippy's `result_large_err`.
    #[error("failed to parse runpm config {path}: {source}")]
    Parse {
        /// Path the loader tried to parse.
        path: PathBuf,
        /// Underlying TOML deserialize error.
        #[source]
        source: Box<toml::de::Error>,
    },
    /// An `[[app]]` table had an empty `cmd` array.
    #[error("app `{name}` has empty cmd in {path}")]
    EmptyCmd {
        /// Path to the offending config file.
        path: PathBuf,
        /// Name of the offending app entry.
        name: String,
    },
    /// Two or more `[[app]]` tables shared the same `name`.
    #[error("duplicate app name `{name}` in {path}")]
    DuplicateName {
        /// Path to the offending config file.
        path: PathBuf,
        /// Name that appeared more than once.
        name: String,
    },
}

impl RunpmConfig {
    /// Load and validate a `runpm.toml` config from disk.
    ///
    /// Returns the parsed config plus the resolved parent directory
    /// (used downstream to resolve relative `cwd` values via
    /// [`RunpmConfig::resolve_cwd`]).
    pub fn load(path: &Path) -> Result<Self, RunpmConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| RunpmConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_str_validated(&text, path)
    }

    /// Parse + validate a config from an in-memory string. The `path`
    /// is used only for error messages and (downstream) for resolving
    /// relative `cwd` entries.
    pub fn from_str_validated(text: &str, path: &Path) -> Result<Self, RunpmConfigError> {
        let parsed: RunpmConfig =
            toml::from_str(text).map_err(|source| RunpmConfigError::Parse {
                path: path.to_path_buf(),
                source: Box::new(source),
            })?;
        parsed.validate(path)?;
        Ok(parsed)
    }

    fn validate(&self, path: &Path) -> Result<(), RunpmConfigError> {
        let mut seen: HashSet<&str> = HashSet::new();
        for app in &self.app {
            if app.cmd.is_empty() {
                return Err(RunpmConfigError::EmptyCmd {
                    path: path.to_path_buf(),
                    name: app.name.clone(),
                });
            }
            if !seen.insert(app.name.as_str()) {
                return Err(RunpmConfigError::DuplicateName {
                    path: path.to_path_buf(),
                    name: app.name.clone(),
                });
            }
        }
        Ok(())
    }

    /// Resolve an app's `cwd` against the config file's parent directory.
    ///
    /// - `None` => `None`
    /// - absolute path => unchanged
    /// - relative path => joined onto `config_path.parent()`
    ///
    /// If `config_path` has no parent (e.g. a bare filename in the CWD),
    /// relative paths are returned as-is.
    pub fn resolve_cwd(config_path: &Path, raw: &Option<String>) -> Option<String> {
        let raw = raw.as_ref()?;
        if raw.is_empty() {
            return None;
        }
        let candidate = Path::new(raw);
        if candidate.is_absolute() {
            return Some(raw.clone());
        }
        let Some(parent) = config_path.parent() else {
            return Some(raw.clone());
        };
        // An empty parent ("" for bare filenames) is treated as CWD —
        // leaving the relative path unchanged is the right call.
        if parent.as_os_str().is_empty() {
            return Some(raw.clone());
        }
        Some(parent.join(candidate).to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_single_app_config() {
        let text = r#"
            [[app]]
            name = "web"
            cmd  = ["node", "server.js"]
        "#;
        let cfg = RunpmConfig::from_str_validated(text, Path::new("runpm.toml")).expect("parse ok");
        assert_eq!(cfg.app.len(), 1);
        let app = &cfg.app[0];
        assert_eq!(app.name, "web");
        assert_eq!(app.cmd, vec!["node", "server.js"]);
        assert_eq!(app.cwd, None);
        assert!(app.env.is_empty());
        assert!(app.autorestart, "autorestart defaults to true");
        assert_eq!(app.max_restarts, None);
    }

    #[test]
    fn parses_full_config_with_env_and_cwd() {
        let text = r#"
            [[app]]
            name = "web"
            cmd  = ["node", "server.js"]
            cwd  = "/srv/web"
            env  = { NODE_ENV = "production", PORT = "8080" }
            autorestart      = false
            max_restarts     = 10
            restart_delay_ms = 1000
            min_uptime_ms    = 2000
            kill_timeout_ms  = 7500
        "#;
        let cfg = RunpmConfig::from_str_validated(text, Path::new("runpm.toml")).expect("parse ok");
        assert_eq!(cfg.app.len(), 1);
        let app = &cfg.app[0];
        assert_eq!(app.cwd.as_deref(), Some("/srv/web"));
        assert_eq!(
            app.env.get("NODE_ENV").map(String::as_str),
            Some("production")
        );
        assert_eq!(app.env.get("PORT").map(String::as_str), Some("8080"));
        assert!(!app.autorestart);
        assert_eq!(app.max_restarts, Some(10));
        assert_eq!(app.restart_delay_ms, Some(1000));
        assert_eq!(app.min_uptime_ms, Some(2000));
        assert_eq!(app.kill_timeout_ms, Some(7500));
    }

    #[test]
    fn rejects_empty_cmd_with_clear_error() {
        let text = r#"
            [[app]]
            name = "broken"
            cmd  = []
        "#;
        let err = RunpmConfig::from_str_validated(text, Path::new("runpm.toml"))
            .expect_err("empty cmd must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("broken"),
            "error must mention the app name; got: {msg}"
        );
        assert!(
            msg.contains("empty cmd"),
            "error must mention 'empty cmd'; got: {msg}"
        );
    }

    #[test]
    fn rejects_duplicate_app_names() {
        let text = r#"
            [[app]]
            name = "web"
            cmd  = ["a"]

            [[app]]
            name = "web"
            cmd  = ["b"]
        "#;
        let err = RunpmConfig::from_str_validated(text, Path::new("runpm.toml"))
            .expect_err("duplicate names must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate") && msg.contains("web"),
            "error must mention 'duplicate' and the offending name; got: {msg}"
        );
    }

    #[test]
    fn parses_empty_file_as_empty_batch() {
        let cfg = RunpmConfig::from_str_validated("", Path::new("runpm.toml")).expect("parse ok");
        assert!(cfg.app.is_empty());
    }

    #[test]
    fn resolve_cwd_passes_through_absolute_paths() {
        #[cfg(unix)]
        let abs = "/srv/web".to_string();
        #[cfg(windows)]
        let abs = "C:\\srv\\web".to_string();

        let resolved = RunpmConfig::resolve_cwd(Path::new("/tmp/runpm.toml"), &Some(abs.clone()));
        assert_eq!(resolved.as_deref(), Some(abs.as_str()));
    }

    #[test]
    fn resolve_cwd_joins_relative_paths_against_config_parent() {
        let resolved = RunpmConfig::resolve_cwd(
            Path::new("/etc/runpm/runpm.toml"),
            &Some("services/web".to_string()),
        )
        .expect("relative path must resolve");
        // PathBuf joins canonically for the host — just check both halves.
        assert!(
            resolved.contains("etc") && resolved.contains("runpm") && resolved.ends_with("web"),
            "resolved path should contain config parent and original relative tail; got {resolved}",
        );
    }

    #[test]
    fn resolve_cwd_none_returns_none() {
        let resolved = RunpmConfig::resolve_cwd(Path::new("/etc/runpm.toml"), &None);
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_cwd_empty_string_returns_none() {
        let resolved = RunpmConfig::resolve_cwd(Path::new("/etc/runpm.toml"), &Some(String::new()));
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_cwd_with_bare_filename_config_path_keeps_relative() {
        // No parent directory => leave the relative path unchanged so
        // the daemon resolves it against its own CWD.
        let resolved =
            RunpmConfig::resolve_cwd(Path::new("runpm.toml"), &Some("services/web".to_string()));
        assert_eq!(resolved.as_deref(), Some("services/web"));
    }
}
