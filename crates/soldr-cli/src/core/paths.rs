//! `~/.soldr/` layout, `SoldrConfig` (`config.toml`), and the
//! `resolve_cargo_home` / `resolve_rustup_home` helpers.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{home_dir, non_empty_env_path, soldr_root_from_env_var};
use super::{SoldrError, CARGO_HOME_ENV_VAR, RUSTUP_HOME_ENV_VAR};

pub const SOLDR_CACHE_DIR_ENV_VAR: &str = "SOLDR_CACHE_DIR";

#[derive(Clone)]
pub struct SoldrPaths {
    pub root: PathBuf,
    pub bin: PathBuf,
    /// Directory holding the pinned-zccache install (issue #426). Lives at
    /// the user's home-anchored `~/.soldr/bin/` independent of
    /// `SOLDR_CACHE_DIR`, so a pin registered against the default cache dir
    /// remains visible to builds that re-root with `SOLDR_CACHE_DIR=/x`.
    /// For synthetic-root construction (`SoldrPaths::with_root`, used by
    /// tests) this collapses into `bin` so test isolation is preserved.
    pub pinned_bin: PathBuf,
    pub cache: PathBuf,
    pub config_file: PathBuf,
}

impl SoldrPaths {
    pub fn new() -> Result<Self, SoldrError> {
        Self::from_root_env_value(std::env::var_os(SOLDR_CACHE_DIR_ENV_VAR).as_deref())
    }

    fn from_root_env_value(value: Option<&OsStr>) -> Result<Self, SoldrError> {
        let env_root = soldr_root_from_env_var(value).transpose()?;
        Self::from_env_root_and_home(env_root, home_dir().map(|h| h.join(".soldr")))
    }

    /// Inner constructor split out so tests can inject explicit `env_root`
    /// + `home_root` values without mutating the process env. Other tests
    ///   in this binary read HOME / USERPROFILE too, so global env mutation
    ///   in a unit test races with parallel cases.
    fn from_env_root_and_home(
        env_root: Option<PathBuf>,
        home_root: Result<PathBuf, SoldrError>,
    ) -> Result<Self, SoldrError> {
        let root = match (&env_root, &home_root) {
            (Some(env), _) => env.clone(),
            (None, Ok(home)) => home.clone(),
            (None, Err(_)) => return Err(SoldrError::NoHomeDir),
        };
        // The pin must NOT move when SOLDR_CACHE_DIR overrides the rest of
        // the install root (issue #426 — pinned binaries are a machine-level
        // user preference, not per-cache-dir state). When no $HOME / no
        // USERPROFILE is available — e.g. a headless CI sandbox that relies
        // entirely on SOLDR_CACHE_DIR — fall back to the env-rooted bin/ so
        // the pin lives somewhere accessible; that env does lose the
        // cross-SOLDR_CACHE_DIR survival property, but it matches today's
        // behavior so nothing regresses.
        let pinned_bin = match &home_root {
            Ok(home) => home.join("bin"),
            Err(_) => root.join("bin"),
        };
        Ok(Self {
            bin: root.join("bin"),
            pinned_bin,
            cache: root.join("cache"),
            config_file: root.join("config.toml"),
            root,
        })
    }

    pub fn with_root(root: PathBuf) -> Self {
        let bin = root.join("bin");
        Self {
            bin: bin.clone(),
            // Tests use synthetic roots and rely on the pin landing inside
            // the test workspace. Collapse pinned_bin into bin so isolation
            // works; production goes through `from_root_env_value` which
            // anchors pinned_bin at the user's home.
            pinned_bin: bin,
            cache: root.join("cache"),
            config_file: root.join("config.toml"),
            root,
        }
    }

    pub fn ensure_dirs(&self) -> Result<(), SoldrError> {
        std::fs::create_dir_all(&self.bin)?;
        std::fs::create_dir_all(&self.pinned_bin)?;
        std::fs::create_dir_all(&self.cache)?;
        Ok(())
    }

    /// Load the soldr config from `config.toml` if it exists. Missing
    /// or malformed files yield `Default::default()` so callers can
    /// proceed with reasonable defaults.
    pub fn load_config(&self) -> SoldrConfig {
        SoldrConfig::load(&self.config_file)
    }
}

// ---------------------------------------------------------------------------
// Config — `~/.soldr/config.toml`
// ---------------------------------------------------------------------------

/// Top-level soldr configuration. Currently carries the `[gc]` section
/// (issue #234), the `[auto_gc]` section (issue #323) and an optional
/// top-level `linker = "..."` field (issue #285); future sections can
/// be added freely.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SoldrConfig {
    #[serde(default)]
    pub gc: GcConfig,
    /// Automatic GC under disk pressure (issue #323).
    #[serde(default)]
    pub auto_gc: AutoGcConfig,
    /// User-configured linker choice for `soldr cargo ...`. Mirrors the
    /// `SOLDR_LINKER` env var; the env var wins when both are set.
    /// Accepted values: `default`, `ld`, `mold`, `rust-lld`, `fast`.
    #[serde(default)]
    pub linker: Option<String>,
}

/// `gc` section of `config.toml`.
///
/// ```toml
/// [gc]
/// allowlist_roots = ["~/dev", "/work/repos"]
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GcConfig {
    /// Roots that `soldr gc` will consider for reclamation. If unset
    /// (or empty after expansion), callers should fall back to
    /// `~/dev`. `~` is expanded to the user's home directory.
    #[serde(default)]
    pub allowlist_roots: Option<Vec<String>>,
}

/// `auto_gc` section of `config.toml` (issue #323 — automatic GC under
/// disk pressure).
///
/// ```toml
/// [auto_gc]
/// enabled = true            # opt-out; on by default
/// trigger_free_gb = 20      # start GC when free space < this
/// target_free_gb  = 30      # stop GC when free space >= this
/// min_age_secs    = 3600    # never touch files modified in this window
/// ```
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AutoGcConfig {
    /// Whether soldr should automatically reclaim disk space when free
    /// space on a soldr-relevant volume drops below `trigger_free_gb`.
    /// Defaults to `true` (opt-out).
    #[serde(default = "AutoGcConfig::default_enabled")]
    pub enabled: bool,
    /// Auto-GC fires when free space on a soldr-relevant volume drops
    /// below this threshold (in GiB).
    #[serde(default = "AutoGcConfig::default_trigger_free_gb")]
    pub trigger_free_gb: u64,
    /// Auto-GC stops escalating once free space reaches this number of
    /// GiB on the affected volume. Must be >= `trigger_free_gb`.
    #[serde(default = "AutoGcConfig::default_target_free_gb")]
    pub target_free_gb: u64,
    /// Floor applied uniformly to every age-based filter used by
    /// auto-GC. Files / directories modified within this many seconds
    /// of "now" are never touched.
    #[serde(default = "AutoGcConfig::default_min_age_secs")]
    pub min_age_secs: u64,
}

impl AutoGcConfig {
    pub const fn default_enabled() -> bool {
        true
    }
    pub const fn default_trigger_free_gb() -> u64 {
        20
    }
    pub const fn default_target_free_gb() -> u64 {
        30
    }
    pub const fn default_min_age_secs() -> u64 {
        3600
    }
}

impl Default for AutoGcConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            trigger_free_gb: Self::default_trigger_free_gb(),
            target_free_gb: Self::default_target_free_gb(),
            min_age_secs: Self::default_min_age_secs(),
        }
    }
}

impl SoldrConfig {
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_default()
    }
}

/// Resolve `$CARGO_HOME` if set and non-empty, otherwise `~/.cargo`.
/// Returns `None` when neither resolves cleanly.
pub fn resolve_cargo_home() -> Option<PathBuf> {
    if let Some(path) = non_empty_env_path(std::env::var_os(CARGO_HOME_ENV_VAR).as_deref()) {
        return Some(path);
    }
    home_dir().ok().map(|home| home.join(".cargo"))
}

/// Resolve `$RUSTUP_HOME` if set and non-empty, otherwise `~/.rustup`.
/// Returns `None` when neither resolves cleanly.
pub fn resolve_rustup_home() -> Option<PathBuf> {
    if let Some(path) = non_empty_env_path(std::env::var_os(RUSTUP_HOME_ENV_VAR).as_deref()) {
        return Some(path);
    }
    home_dir().ok().map(|home| home.join(".rustup"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paths() {
        let paths = SoldrPaths::from_root_env_value(None).unwrap();
        assert!(paths.root.ends_with(".soldr"));
        assert!(paths.bin.ends_with("bin"));
        assert!(paths.cache.ends_with("cache"));
    }

    #[test]
    fn pinned_bin_survives_soldr_cache_dir_override() {
        // Issue #426: SOLDR_CACHE_DIR overrides the install root (and bin/
        // and cache/ along with it), but the pin must keep living at the
        // home-anchored ~/.soldr/bin/ so a pin registered against the
        // default cache dir stays visible to builds that re-root the cache.
        //
        // We exercise `from_env_root_and_home` directly so the test
        // doesn't have to mutate HOME / USERPROFILE — that would race with
        // every other case in this binary that reads them.
        let home_root = PathBuf::from("/synthetic-home/.soldr");
        let env_root = PathBuf::from("/synthetic-cache-dir");
        let paths =
            SoldrPaths::from_env_root_and_home(Some(env_root.clone()), Ok(home_root.clone()))
                .unwrap();

        assert_eq!(paths.root, env_root, "root follows SOLDR_CACHE_DIR");
        assert_eq!(
            paths.bin,
            env_root.join("bin"),
            "bin/ follows SOLDR_CACHE_DIR (managed binaries stay per-cache-dir)"
        );
        assert_eq!(
            paths.pinned_bin,
            home_root.join("bin"),
            "pinned_bin stays at $HOME/.soldr/bin/ regardless of SOLDR_CACHE_DIR"
        );
        assert_ne!(
            paths.bin, paths.pinned_bin,
            "the whole point of the fix: bin and pinned_bin diverge under env override"
        );
    }

    #[test]
    fn pinned_bin_equals_bin_when_no_cache_dir_override() {
        // No SOLDR_CACHE_DIR → root falls back to home → bin == pinned_bin.
        // No behavior change in the dominant case.
        let home_root = PathBuf::from("/synthetic-home/.soldr");
        let paths = SoldrPaths::from_env_root_and_home(None, Ok(home_root.clone())).unwrap();
        assert_eq!(paths.root, home_root);
        assert_eq!(paths.bin, home_root.join("bin"));
        assert_eq!(paths.pinned_bin, paths.bin);
    }

    #[test]
    fn pinned_bin_falls_back_to_env_root_when_no_home_available() {
        // Headless sandbox case: SOLDR_CACHE_DIR set but no $HOME /
        // USERPROFILE. pinned_bin must NOT panic — it falls back to the
        // env-rooted bin/ (loses cross-SOLDR_CACHE_DIR survival, but
        // matches today's behavior so nothing regresses).
        let env_root = PathBuf::from("/sandbox/.soldr");
        let paths =
            SoldrPaths::from_env_root_and_home(Some(env_root.clone()), Err(SoldrError::NoHomeDir))
                .unwrap();
        assert_eq!(paths.pinned_bin, env_root.join("bin"));
        assert_eq!(paths.pinned_bin, paths.bin);
    }

    #[test]
    fn pinned_bin_collapses_into_bin_with_synthetic_root() {
        // `SoldrPaths::with_root` is the test-construction path used by
        // integration tests. The pin dir must live inside the synthetic
        // root so test workspaces can register pins without escaping to
        // the host's home dir.
        let root = PathBuf::from("/tmp/synthetic-soldr-root");
        let paths = SoldrPaths::with_root(root.clone());
        assert_eq!(paths.bin, root.join("bin"));
        assert_eq!(paths.pinned_bin, paths.bin);
    }

    #[test]
    fn auto_gc_config_default_round_trip() {
        let cfg: SoldrConfig = toml::from_str("[auto_gc]\n").unwrap();
        assert_eq!(cfg.auto_gc, AutoGcConfig::default());
        assert!(cfg.auto_gc.enabled);
        assert_eq!(cfg.auto_gc.trigger_free_gb, 20);
        assert_eq!(cfg.auto_gc.target_free_gb, 30);
        assert_eq!(cfg.auto_gc.min_age_secs, 3600);
    }

    #[test]
    fn auto_gc_config_custom_values_parse() {
        let toml_text = r#"
[auto_gc]
enabled = false
trigger_free_gb = 10
target_free_gb = 50
min_age_secs = 7200
"#;
        let cfg: SoldrConfig = toml::from_str(toml_text).unwrap();
        assert!(!cfg.auto_gc.enabled);
        assert_eq!(cfg.auto_gc.trigger_free_gb, 10);
        assert_eq!(cfg.auto_gc.target_free_gb, 50);
        assert_eq!(cfg.auto_gc.min_age_secs, 7200);
    }

    #[test]
    fn missing_auto_gc_section_uses_defaults() {
        let cfg: SoldrConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.auto_gc, AutoGcConfig::default());
    }

    #[test]
    fn soldr_root_override_uses_env_path() {
        let root = soldr_root_from_env_var(Some(OsStr::new("C:\\temp\\soldr-cache-root")))
            .unwrap()
            .unwrap();
        assert_eq!(root, PathBuf::from("C:\\temp\\soldr-cache-root"));
    }

    #[test]
    fn soldr_root_override_ignores_empty_env() {
        assert!(soldr_root_from_env_var(Some(OsStr::new(""))).is_none());
    }
}
