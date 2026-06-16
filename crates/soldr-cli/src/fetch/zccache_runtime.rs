//! Single zccache runtime/source contract.
//!
//! `ZccacheResolver` is the boundary between callers that only need to inspect
//! what soldr would use and callers that are allowed to materialize or fetch a
//! runtime. This keeps doctor/status paths read-only while preserving the cargo
//! front door's local -> pinned -> managed precedence chain.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths, TargetTriple};

use super::install_zccache;
use super::zccache::{
    canonical_zccache_paths, count_debug_info_sidecars, local_zccache_source_paths_for_target,
    local_zccache_version_label, resolve_local_zccache_for_target, ZccacheBinarySummary,
    ZccacheSource,
};
use super::zccache_install::{
    install_zccache_from_crates_io, managed_zccache_repo,
    should_fallback_to_managed_zccache_cargo_install,
};
use super::{
    check_cache, fetch_repo_binary_with_paths, non_empty_env_path, FetchResult, VersionSpec,
    MANAGED_ZCCACHE_VERSION, ZCCACHE_LOCAL_DIR_ENV_VAR,
};

/// Exact branch that produced a zccache runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZccacheRuntimeSource {
    /// Materialized or inspected from `SOLDR_ZCCACHE_LOCAL_DIR`.
    Local,
    /// Installed via `soldr install-zccache`.
    Pinned,
    /// Managed zccache already existed under soldr's bin cache.
    ManagedCached,
    /// Managed zccache came from GitHub Releases during this resolution.
    ManagedDownloaded,
    /// Managed zccache came from the crates.io fallback.
    ManagedFallback,
    /// User requested `--zccache=system`.
    System,
    /// Test-only override from `SOLDR_TEST_ZCCACHE_BIN`.
    TestOverride,
    /// Nothing is currently materialized for the managed path.
    Missing,
}

impl ZccacheRuntimeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ZccacheRuntimeSource::Local => "local",
            ZccacheRuntimeSource::Pinned => "pinned",
            ZccacheRuntimeSource::ManagedCached => "managed-cached",
            ZccacheRuntimeSource::ManagedDownloaded => "managed-downloaded",
            ZccacheRuntimeSource::ManagedFallback => "managed-fallback",
            ZccacheRuntimeSource::System => "system",
            ZccacheRuntimeSource::TestOverride => "test-override",
            ZccacheRuntimeSource::Missing => "missing",
        }
    }

    pub fn summary_source(self) -> ZccacheSource {
        match self {
            ZccacheRuntimeSource::Local => ZccacheSource::Local,
            ZccacheRuntimeSource::Pinned => ZccacheSource::Pinned,
            ZccacheRuntimeSource::ManagedCached
            | ZccacheRuntimeSource::ManagedDownloaded
            | ZccacheRuntimeSource::ManagedFallback => ZccacheSource::Managed,
            ZccacheRuntimeSource::System => ZccacheSource::System,
            ZccacheRuntimeSource::TestOverride => ZccacheSource::TestOverride,
            ZccacheRuntimeSource::Missing => ZccacheSource::None,
        }
    }

    pub fn is_missing(self) -> bool {
        matches!(self, ZccacheRuntimeSource::Missing)
    }
}

/// Resolved zccache runtime plus source metadata.
#[derive(Debug, Clone)]
pub struct ZccacheRuntime {
    pub source: ZccacheRuntimeSource,
    pub version: String,
    pub runtime_dir: PathBuf,
    pub source_dir: Option<PathBuf>,
    pub cli_path: Option<PathBuf>,
    pub daemon_path: Option<PathBuf>,
    pub fp_path: Option<PathBuf>,
    pub debug_info_found: usize,
    pub debug_info_expected: usize,
    pub symbol_path: PathBuf,
    pub cached: bool,
}

impl ZccacheRuntime {
    pub fn from_test_override(binary_path: PathBuf) -> Result<Self, SoldrError> {
        let target = TargetTriple::detect()?;
        Ok(Self::from_fetch_result(
            ZccacheRuntimeSource::TestOverride,
            FetchResult {
                binary_path,
                version: MANAGED_ZCCACHE_VERSION.to_string(),
                cached: true,
            },
            None,
            &target,
        ))
    }

    pub fn is_missing(&self) -> bool {
        self.source.is_missing()
    }

    pub fn to_fetch_result(&self) -> Result<FetchResult, SoldrError> {
        let Some(binary_path) = self.cli_path.clone() else {
            return Err(SoldrError::Other(format!(
                "zccache runtime source {} has no CLI binary",
                self.source.as_str()
            )));
        };
        Ok(FetchResult {
            binary_path,
            version: self.version.clone(),
            cached: self.cached,
        })
    }

    pub fn to_summary(&self) -> ZccacheBinarySummary {
        ZccacheBinarySummary {
            source: self.source.summary_source(),
            version: self.version.clone(),
            runtime_dir: self.runtime_dir.clone(),
            source_dir: self.source_dir.clone(),
            cli_path: self.cli_path.clone(),
            daemon_path: self.daemon_path.clone(),
            fp_path: self.fp_path.clone(),
            debug_info_found: self.debug_info_found,
            debug_info_expected: self.debug_info_expected,
            symbol_path: self.symbol_path.clone(),
        }
    }

    fn from_fetch_result(
        source: ZccacheRuntimeSource,
        fetch: FetchResult,
        source_dir: Option<PathBuf>,
        target: &TargetTriple,
    ) -> Self {
        let runtime_dir = fetch
            .binary_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let (cli, daemon, fp) = canonical_zccache_paths(&runtime_dir, target);
        let (debug_info_found, debug_info_expected) =
            count_debug_info_sidecars(&[cli.as_path(), daemon.as_path(), fp.as_path()]);
        Self {
            source,
            version: fetch.version,
            symbol_path: runtime_dir.clone(),
            runtime_dir,
            source_dir,
            cli_path: Some(fetch.binary_path),
            daemon_path: daemon.exists().then_some(daemon),
            fp_path: fp.exists().then_some(fp),
            debug_info_found,
            debug_info_expected,
            cached: fetch.cached,
        }
    }

    fn missing_managed(paths: &SoldrPaths, target: &TargetTriple) -> Self {
        let runtime_dir = managed_runtime_dir(paths);
        let (cli, daemon, fp) = canonical_zccache_paths(&runtime_dir, target);
        let (debug_info_found, debug_info_expected) =
            count_debug_info_sidecars(&[cli.as_path(), daemon.as_path(), fp.as_path()]);
        Self {
            source: ZccacheRuntimeSource::Missing,
            version: String::new(),
            symbol_path: runtime_dir.clone(),
            runtime_dir,
            source_dir: None,
            cli_path: None,
            daemon_path: None,
            fp_path: None,
            debug_info_found,
            debug_info_expected,
            cached: false,
        }
    }
}

/// Resolver bound to a soldr root and host target.
pub struct ZccacheResolver<'a> {
    paths: &'a SoldrPaths,
    target: TargetTriple,
}

impl<'a> ZccacheResolver<'a> {
    pub fn new(paths: &'a SoldrPaths) -> Result<Self, SoldrError> {
        Ok(Self {
            paths,
            target: TargetTriple::detect()?,
        })
    }

    /// Resolve the default cargo runtime, materializing local builds and
    /// fetching managed zccache when necessary.
    pub async fn materialize_default(&self) -> Result<ZccacheRuntime, SoldrError> {
        self.paths.ensure_dirs()?;

        if let Some(local_dir) = non_empty_env_path(ZCCACHE_LOCAL_DIR_ENV_VAR) {
            let fetch = resolve_local_zccache_for_target(&local_dir, self.paths, &self.target)?;
            return Ok(ZccacheRuntime::from_fetch_result(
                ZccacheRuntimeSource::Local,
                fetch,
                Some(local_dir),
                &self.target,
            ));
        }

        if let Some(pinned) = self.resolve_usable_pinned(true)? {
            return Ok(self.runtime_from_pinned(pinned));
        }

        if let Some(result) = self.managed_cached_fetch_result()? {
            return Ok(ZccacheRuntime::from_fetch_result(
                ZccacheRuntimeSource::ManagedCached,
                result,
                None,
                &self.target,
            ));
        }

        let release_version = VersionSpec::Exact(MANAGED_ZCCACHE_VERSION.to_string());
        let repo = managed_zccache_repo();
        let release_outcome = fetch_repo_binary_with_paths(
            "zccache",
            &["zccache", "zccache-daemon", "zccache-fp"],
            &repo,
            &release_version,
            None,
            self.paths,
        )
        .await;

        match release_outcome {
            Ok(result) => {
                let source = if result.cached {
                    ZccacheRuntimeSource::ManagedCached
                } else {
                    ZccacheRuntimeSource::ManagedDownloaded
                };
                Ok(ZccacheRuntime::from_fetch_result(
                    source,
                    result,
                    None,
                    &self.target,
                ))
            }
            Err(err) if should_fallback_to_managed_zccache_cargo_install(&err) => {
                eprintln!(
                    "soldr: managed zccache prebuilt unavailable ({err}); falling back to cargo install"
                );
                let binary_path = install_zccache_from_crates_io(
                    self.paths,
                    MANAGED_ZCCACHE_VERSION,
                    &self.target,
                )?;
                Ok(ZccacheRuntime::from_fetch_result(
                    ZccacheRuntimeSource::ManagedFallback,
                    FetchResult {
                        binary_path,
                        version: MANAGED_ZCCACHE_VERSION.to_string(),
                        cached: false,
                    },
                    None,
                    &self.target,
                ))
            }
            Err(err) => Err(err),
        }
    }

    /// Inspect the default runtime without copying local builds or fetching
    /// managed binaries.
    pub fn inspect_default(&self) -> Result<ZccacheRuntime, SoldrError> {
        if let Some(local_dir) = non_empty_env_path(ZCCACHE_LOCAL_DIR_ENV_VAR) {
            return self.inspect_local(&local_dir);
        }

        if let Some(pinned) = self.resolve_usable_pinned(false)? {
            return Ok(self.runtime_from_pinned(pinned));
        }

        self.inspect_managed_only()
    }

    pub fn inspect_managed_only(&self) -> Result<ZccacheRuntime, SoldrError> {
        if let Some(result) = self.managed_cached_fetch_result()? {
            return Ok(ZccacheRuntime::from_fetch_result(
                ZccacheRuntimeSource::ManagedCached,
                result,
                None,
                &self.target,
            ));
        }
        Ok(ZccacheRuntime::missing_managed(self.paths, &self.target))
    }

    pub fn inspect_pinned(&self) -> Result<Option<ZccacheRuntime>, SoldrError> {
        let Some(pinned) =
            install_zccache::resolve_pinned_zccache_for_target(self.paths, &self.target)?
        else {
            return Ok(None);
        };
        Ok(Some(self.runtime_from_pinned(pinned)))
    }

    pub fn materialize_system(&self) -> Result<ZccacheRuntime, SoldrError> {
        let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default();
        self.materialize_system_with_path_dirs(&path_dirs)
    }

    pub fn materialize_system_with_path_dirs(
        &self,
        path_dirs: &[PathBuf],
    ) -> Result<ZccacheRuntime, SoldrError> {
        let fetch =
            super::zccache_install::resolve_system_zccache_with_path_dirs(&self.target, path_dirs)?;
        Ok(ZccacheRuntime::from_fetch_result(
            ZccacheRuntimeSource::System,
            fetch,
            None,
            &self.target,
        ))
    }

    fn inspect_local(&self, local_dir: &Path) -> Result<ZccacheRuntime, SoldrError> {
        let [cli, daemon, fp] = local_zccache_source_paths_for_target(local_dir, &self.target)?;
        let (debug_info_found, debug_info_expected) =
            count_debug_info_sidecars(&[cli.as_path(), daemon.as_path(), fp.as_path()]);
        Ok(ZccacheRuntime {
            source: ZccacheRuntimeSource::Local,
            version: local_zccache_version_label(&cli),
            runtime_dir: local_dir.to_path_buf(),
            source_dir: Some(local_dir.to_path_buf()),
            cli_path: Some(cli),
            daemon_path: Some(daemon),
            fp_path: Some(fp),
            debug_info_found,
            debug_info_expected,
            symbol_path: local_dir.to_path_buf(),
            cached: true,
        })
    }

    fn runtime_from_pinned(&self, pinned: install_zccache::PinnedResolution) -> ZccacheRuntime {
        ZccacheRuntime::from_fetch_result(
            ZccacheRuntimeSource::Pinned,
            FetchResult {
                binary_path: pinned.binary_path,
                version: pinned.version,
                cached: true,
            },
            None,
            &self.target,
        )
    }

    fn managed_cached_fetch_result(&self) -> Result<Option<FetchResult>, SoldrError> {
        check_cache(
            self.paths,
            "zccache",
            MANAGED_ZCCACHE_VERSION,
            &["zccache", "zccache-daemon", "zccache-fp"],
            &self.target,
        )
    }

    fn resolve_usable_pinned(
        &self,
        log_stale_pin: bool,
    ) -> Result<Option<install_zccache::PinnedResolution>, SoldrError> {
        let Some(pinned) =
            install_zccache::resolve_pinned_zccache_for_target(self.paths, &self.target)?
        else {
            return Ok(None);
        };
        if install_zccache::pinned_version_older_than_managed(&pinned.version) {
            if log_stale_pin {
                eprintln!(
                    "{}",
                    stale_pin_warning(
                        &pinned.version,
                        &pinned.runtime_dir,
                        MANAGED_ZCCACHE_VERSION,
                        stderr_should_use_color(),
                    )
                );
            }
            return Ok(None);
        }
        Ok(Some(pinned))
    }
}

fn managed_runtime_dir(paths: &SoldrPaths) -> PathBuf {
    paths.bin.join(format!("zccache-{MANAGED_ZCCACHE_VERSION}"))
}

/// Build the two-line "stale pinned zccache" warning. Pure so the
/// color-on / color-off forms can be asserted from a unit test;
/// mirrors `cargo_front_door::disk::low_disk_warning_for_free_bytes`.
fn stale_pin_warning(
    pinned_version: &str,
    pinned_dir: &Path,
    managed_version: &str,
    use_color: bool,
) -> String {
    let warning = if use_color {
        "\x1b[33mwarning\x1b[0m"
    } else {
        "warning"
    };
    format!(
        "soldr: {warning}: pinned zccache {pinned_version} at {dir} is older than the managed zccache {managed_version} this soldr build requires; falling back to the managed install.\n\
         soldr: {warning}:   recommendation: run `soldr install-zccache --remove` to purge the stale pin and silence this warning.",
        dir = pinned_dir.display(),
    )
}

fn stderr_should_use_color() -> bool {
    use std::io::IsTerminal;

    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    crate::timed_test!(stale_pin_warning_colorizes_only_warning_label, {
        let dir = PathBuf::from("/fake/.soldr/bin/zccache-pinned");
        let colored = stale_pin_warning("1.8.1", &dir, "1.12.7", true);
        assert!(
            colored.contains("\x1b[33mwarning\x1b[0m"),
            "expected ANSI-yellow warning label, got: {colored}"
        );
        assert!(
            colored.starts_with("soldr: "),
            "soldr: prefix must remain uncolored for log-grep: {colored}"
        );
        assert!(
            colored.contains("1.8.1"),
            "pinned version must appear in the message: {colored}"
        );
        assert!(
            colored.contains("1.12.7"),
            "managed version must appear in the message: {colored}"
        );
        assert!(
            colored.contains("older than the managed zccache"),
            "diagnostic phrasing missing: {colored}"
        );
        assert!(
            colored.contains("soldr install-zccache --remove"),
            "remediation recommendation missing: {colored}"
        );
    });

    crate::timed_test!(stale_pin_warning_plain_form_drops_ansi, {
        let dir = PathBuf::from("/fake/.soldr/bin/zccache-pinned");
        let plain = stale_pin_warning("1.8.1", &dir, "1.12.7", false);
        assert!(
            !plain.contains('\x1b'),
            "plain form must not contain ANSI escapes: {plain:?}"
        );
        assert!(plain.contains("warning"), "plain warning token missing");
        assert!(
            plain.contains("soldr install-zccache --remove"),
            "remediation recommendation missing in plain form: {plain}"
        );
    });
}
