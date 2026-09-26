//! zccache cache-hit delivery mode (soldr#3407, zackees/zccache#1683).
//!
//! zccache reads `ZCCACHE_MODE` (`AUTO` | `LINK` | `COPY` | `REFLINK`) per
//! request from the environment the rustc wrapper forwards. soldr's job is
//! only to decide which value that is and put it on the cargo child, so the
//! embedded service sees it on every compile.
//!
//! # Precedence
//!
//! 1. `SOLDR_ZCCACHE_MODE` — soldr's own knob; `--zccache-mode` publishes it.
//! 2. `[zccache] mode` in `config.toml`.
//! 3. The user's own `ZCCACHE_MODE` — honored as-is, never rewritten.
//! 4. Unset — zccache's default (`AUTO`).
//!
//! Unlike [`crate::core::jobs`], an unrecognised value is an error rather
//! than a fall-through: silently delivering hardlinks to someone who asked
//! for `COPY` defeats the reason they asked. The embedded service only logs
//! an invalid `ZCCACHE_MODE`, so soldr rejects every tier, including the
//! user's own variable, before the build starts.

use serde::Deserialize;
use std::fmt;

/// soldr's own spelling of the mode knob. `--zccache-mode` publishes it.
pub const SOLDR_ZCCACHE_MODE_ENV_VAR: &str = "SOLDR_ZCCACHE_MODE";

/// The variable zccache itself reads, per request.
pub const ZCCACHE_MODE_ENV_VAR: &str = "ZCCACHE_MODE";

/// How the embedded zccache delivers a cache hit to its output path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZccacheMode {
    /// Reflink, else hardlink (when the output may share an inode), else copy.
    Auto,
    /// Hardlink eligible outputs.
    Link,
    /// Always an independent, writable byte copy.
    Copy,
    /// An independent copy-on-write clone; a copy where the volume cannot.
    Reflink,
}

impl ZccacheMode {
    pub const ALL: [Self; 4] = [Self::Auto, Self::Link, Self::Copy, Self::Reflink];

    /// The canonical spelling zccache documents and soldr injects.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::Link => "LINK",
            Self::Copy => "COPY",
            Self::Reflink => "REFLINK",
        }
    }

    /// Case- and whitespace-insensitive. Empty means unset (`Ok(None)`);
    /// anything else that is not a mode is [`UnknownZccacheMode`].
    pub fn parse(raw: &str) -> Result<Option<Self>, UnknownZccacheMode> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        Self::ALL
            .into_iter()
            .find(|mode| trimmed.eq_ignore_ascii_case(mode.as_str()))
            .map(Some)
            .ok_or(UnknownZccacheMode)
    }
}

/// A value that names none of the four modes. [`resolve_zccache_mode_from`]
/// attaches the offending tier and value as [`InvalidZccacheMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownZccacheMode;

impl fmt::Display for ZccacheMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `[zccache]` section of `config.toml`.
///
/// ```toml
/// [zccache]
/// mode = "reflink"
/// ```
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct ZccacheModeConfig {
    /// Cache-hit delivery mode. `None` falls through to the next tier.
    #[serde(default)]
    pub mode: Option<String>,
}

/// Where a resolved mode came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZccacheModeSource {
    SoldrEnv,
    Config,
    ZccacheEnv,
}

impl ZccacheModeSource {
    pub fn describe(self) -> &'static str {
        match self {
            Self::SoldrEnv => "SOLDR_ZCCACHE_MODE (or --zccache-mode)",
            Self::Config => "config.toml [zccache].mode",
            Self::ZccacheEnv => "ZCCACHE_MODE",
        }
    }

    /// Whether soldr owns this value and must put it on the cargo child.
    /// The user's own `ZCCACHE_MODE` already reaches zccache unaided.
    pub fn soldr_owned(self) -> bool {
        !matches!(self, Self::ZccacheEnv)
    }
}

/// The resolved mode plus where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedZccacheMode {
    pub mode: ZccacheMode,
    pub source: ZccacheModeSource,
}

impl ResolvedZccacheMode {
    /// The value soldr must set as `ZCCACHE_MODE` on the cargo child, if any.
    pub fn child_env_value(self) -> Option<&'static str> {
        self.source.soldr_owned().then(|| self.mode.as_str())
    }
}

/// A configured value that is not one of the four modes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidZccacheMode {
    pub source: ZccacheModeSource,
    pub value: String,
}

impl fmt::Display for InvalidZccacheMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "soldr: invalid {} value {:?}: expected one of AUTO, LINK, COPY, REFLINK",
            self.source.describe(),
            self.value
        )
    }
}

impl std::error::Error for InvalidZccacheMode {}

/// Resolve the mode from an explicit set of inputs. Pure, so the precedence
/// is unit-testable without touching the process environment.
pub fn resolve_zccache_mode_from(
    soldr_env: Option<&str>,
    config: Option<&str>,
    zccache_env: Option<&str>,
) -> Result<Option<ResolvedZccacheMode>, InvalidZccacheMode> {
    let tiers = [
        (soldr_env, ZccacheModeSource::SoldrEnv),
        (config, ZccacheModeSource::Config),
        (zccache_env, ZccacheModeSource::ZccacheEnv),
    ];
    for (value, source) in tiers {
        let Some(value) = value else { continue };
        match ZccacheMode::parse(value) {
            Ok(Some(mode)) => return Ok(Some(ResolvedZccacheMode { mode, source })),
            Ok(None) => continue,
            Err(UnknownZccacheMode) => {
                return Err(InvalidZccacheMode {
                    source,
                    value: value.to_string(),
                })
            }
        }
    }
    Ok(None)
}

/// [`resolve_zccache_mode_from`] against the live environment and the
/// caller's parsed config.
pub fn resolve_zccache_mode(
    config: Option<&str>,
) -> Result<Option<ResolvedZccacheMode>, InvalidZccacheMode> {
    let soldr = std::env::var(SOLDR_ZCCACHE_MODE_ENV_VAR).ok();
    let zccache = std::env::var(ZCCACHE_MODE_ENV_VAR).ok();
    resolve_zccache_mode_from(soldr.as_deref(), config, zccache.as_deref())
}

#[cfg(test)]
#[path = "materialization_mode_tests.rs"]
mod tests;
