//! Embedded zccache service wrapper (issue #977 / #980 L1).
//!
//! This module is the **only** soldr-side import site for the
//! `zccache::embedded::*` API. Everything else in soldr talks to
//! [`SoldrZccacheService`] so the upstream-API blast radius is bounded
//! to one file. As of the #980 L1 "no backwards compatibility" pass
//! the embedded service is mandatory — the daemon always instantiates
//! it at boot and the wrapper always dispatches through the
//! `Request::Compile` IPC verb. The legacy fork-zccache.exe path is
//! gone.
//!
//! ## Design constraint: daemon-only
//!
//! Per issue #977 (and the user's explicit instruction in the goal
//! prompt), the embedded `ZccacheService` lives **inside the long-lived
//! soldr-daemon process and nowhere else**. Transient rustc-wrapper
//! invocations (which live for the lifetime of a single rustc command)
//! must not pay `ZccacheService::start` cost — that would defeat the
//! whole point of "one process, one tokio runtime, one console-subscriber
//! pane". Wrappers always talk to the daemon over the
//! `Request::Compile` IPC verb that ferries the compile to the
//! embedded backend without a `Command::new("zccache")` fork.
//!
//! ## Tokio runtime sharing (the tokio-console story)
//!
//! `RuntimeHooks` accepts an explicit Tokio handle. Soldr starts the service
//! from inside its daemon runtime, so the ambient handle owns zccache's index
//! writer. Soldr explicitly owns the five-minute/24-hour maintenance schedule
//! so only one build-aware scanner runs. `console-subscriber` sees the union
//! of soldr and zccache tasks.
//!
//! ## Identity defaults
//!
//! - `product = "soldr"`
//! - `instance_id = "embedded-v1"` — stable across daemon restarts, soldr
//!   upgrades, and cache-root save/load relocation. The selected `SoldrPaths`
//!   root provides physical isolation between dev/prod/custom installations.
//! - `workspace_id` — currently unused by us at start-time (set per
//!   compile via `AuditContext`). Left as the same hash as instance_id
//!   so a future per-workspace bisect still has a default name.
//!
//! ## Cache root
//!
//! `<soldr_cache_root>/zccache/` per the issue text. Created at start.
//!
//! ## Audit context
//!
//! Filled per compile by [`SoldrZccacheService::compile`]. Each call
//! gets a synthetic per-invocation `AuditId`; durable correlation with
//! the soldr `BuildSession{Start,End}` events is a follow-up tracked
//! in #977 open questions.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use running_process::broker::backend_handle::DaemonProcess;
use zccache::audit::AuditMode;
use zccache::core::NormalizedPath;
use zccache::embedded::{
    AuditConfig, AuditContext, CacheOutcome, CompileRequest as ZccacheCompileRequest,
    DiskCacheLimits, DiskMaintenanceKind, DiskMaintenancePressure, FlushStepOutcome, HostIdentity,
    RuntimeHooks, ServiceLimits, ShutdownMode, ZccacheConfig, ZccacheService,
};
use zccache::hash::StreamHasher;

use crate::core::SoldrPaths;
use crate::daemon::protocol::{
    CacheFlushInfo, CacheFlushStepInfo, CompileRequest, CompileResponseBody, CompileStatsInfo,
    StagedProfileInfo,
};

/// Soldr-side handle around a started [`ZccacheService`]. Cheap to
/// clone (the inner handle is `Arc`-shared).
#[derive(Clone)]
pub struct SoldrZccacheService {
    inner: Arc<ZccacheService>,
    identity: HostIdentity,
    cache_root: PathBuf,
    disk_policy: EmbeddedDiskPolicy,
    applied_jobs: crate::core::jobs::ResolvedJobs,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmbeddedDiskPolicy {
    pub source: String,
    pub max_cache_bytes: Option<u64>,
    pub max_cache_percent: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmbeddedDiskMaintenanceReport {
    pub kind: String,
    pub pressure: String,
    pub budget_bytes: u64,
    pub usage_before_bytes: u64,
    pub usage_after_bytes: u64,
    pub bytes_reclaimed: u64,
    pub artifacts_removed: usize,
    pub expired_artifacts_removed: usize,
    pub pending_write_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyCacheSweepReport {
    pub removed: usize,
    pub failed: usize,
    pub bytes_reclaimed: u64,
}

/// Errors raised while starting or stopping the embedded service. Wrap
/// the upstream `EmbeddedError` into a plain string so soldr crates
/// further up the call stack do not have to know the zccache types.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddedServiceError {
    #[error("zccache embedded start failed: {0}")]
    Start(String),
    #[error("zccache embedded shutdown failed: {0}")]
    Shutdown(String),
    #[error("zccache embedded flush failed: {0}")]
    Flush(String),
    #[error("zccache embedded stats failed: {0}")]
    Stats(String),
    #[error("zccache embedded maintenance failed: {0}")]
    Maintenance(String),
    /// Issue #977 Phase 5 / #980 L1 — surfaced by [`SoldrZccacheService::compile`].
    /// Maps to a soldr-side `Response::Error`; the mandatory broker route
    /// propagates the compile-service failure without changing execution mode.
    #[error("zccache embedded compile failed: {0}")]
    Compile(String),
    /// Issue #977 Phase 5 — the wrapper sent a `CompileRequest` with an
    /// empty args list; we have no rustc path to invoke.
    #[error("Compile request missing compiler path (args[0])")]
    EmptyArgs,
    #[error("io while preparing zccache cache root: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "multiple legacy embedded zccache backends have the same newest mtime under {root}; refusing to choose silently: {candidates:?}"
    )]
    AmbiguousLegacyCache {
        root: PathBuf,
        candidates: Vec<PathBuf>,
    },
}

impl SoldrZccacheService {
    /// Start the embedded service. Must be called from inside the
    /// daemon's tokio runtime (the function is `async` for exactly
    /// that reason). Creates the cache root directory if missing.
    pub async fn start(
        paths: &SoldrPaths,
        daemon_identity: &DaemonProcess,
    ) -> Result<Self, EmbeddedServiceError> {
        let identity = derive_identity();
        // soldr#1635 / zccache#1085: the embedded service must never place
        // mutable zccache snapshots under a root that another broker-routed
        // daemon can open. The selected soldr root provides physical
        // isolation. A fixed relative identity keeps the backend stable across
        // save/load relocation and soldr upgrades.
        let cache_root = private_zccache_cache_root(paths, &identity);
        prepare_embedded_cache_root(paths, daemon_identity, &cache_root)?;
        scrub_existing_compile_journals(paths)?;
        // zccache#926 strict-validation: `AuditConfig::default()` ships
        // `mode = AuditMode::Normal` + `output_root = None`, which the
        // new audit-sink validation rejects ("audit sink requires
        // output_root when mode > Off"). soldr does not consume zccache
        // audit events today — the per-compile trace site (soldr#985 /
        // zccache#940) lives outside the AuditSink. Set mode = Off
        // explicitly so the embedded service starts cleanly.
        let audit = AuditConfig {
            mode: AuditMode::Off,
            ..AuditConfig::default()
        };
        let resolved_jobs = crate::compile_limit::resolve_and_announce();
        let cfg = ZccacheConfig {
            host: identity.clone(),
            cache_root: cache_root.clone().into(),
            audit,
            // soldr#1761: soldr owns the compile-concurrency limit now.
            // This used to be `ServiceLimits::default()`, i.e.
            // `max_parallel_compiles: None`, so the vendored zccache
            // default always governed and the only way to influence it
            // was to get `ZCCACHE_MAX_PARALLEL_COMPILES` into the
            // long-lived daemon's inherited environment. Resolving here
            // means `SOLDR_JOBS` and `config.toml` reach the semaphore
            // through `ServiceLimits`, not through env propagation, and
            // the outer admission queue sizes itself from the same call.
            limits: ServiceLimits {
                max_parallel_compiles: Some(resolved_jobs.jobs),
                ..ServiceLimits::default()
            },
            runtime: RuntimeHooks {
                service_name: Some("soldr-daemon".into()),
                // zccache#922: explicitly keep background maintenance and
                // index tasks on the soldr-daemon runtime.
                handle: Some(tokio::runtime::Handle::current()),
            },
            // zccache#923 — None preserves the prior behavior where
            // only `shutdown(ShutdownMode::Force)` aborts in-flight
            // work. soldr's daemon already has a Notify-based
            // shutdown path that race-completes the same scenarios.
            cancellation: None,
        };

        let (disk_limits, disk_policy) = disk_cache_limits_from_env()?;
        let svc = ZccacheService::start_with_options(
            cfg,
            crate::zccache_staging::options(&cache_root, disk_limits),
        )
        .await
        .map_err(|e| EmbeddedServiceError::Start(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(svc),
            identity,
            cache_root,
            disk_policy,
            applied_jobs: resolved_jobs,
        })
    }

    /// The compile limit this service started with — the number now baked
    /// into the semaphore, not a fresh resolution.
    ///
    /// soldr#2023: the daemon outlives the environment that spawned it, so
    /// this is the only value that answers "what is the running daemon
    /// actually doing?" Re-resolving would answer "what would a new daemon
    /// do?", which is the other half of the comparison a client makes.
    pub fn applied_jobs(&self) -> crate::core::jobs::ResolvedJobs {
        self.applied_jobs
    }

    /// Resolved on-disk cache root. Useful for diagnostics surfaces.
    pub fn cache_root(&self) -> &std::path::Path {
        &self.cache_root
    }

    /// Stable host identity used to derive the embedded endpoint and
    /// namespace cache entries.
    pub fn identity(&self) -> &HostIdentity {
        &self.identity
    }

    pub fn disk_policy(&self) -> &EmbeddedDiskPolicy {
        &self.disk_policy
    }

    /// Dispatch a soldr [`CompileRequest`] through the embedded zccache
    /// service (issue #977 Phase 5 / #980 L1).
    ///
    /// The conversion lives here so `daemon::server::handle_connection`
    /// stays oblivious to `zccache::embedded::*` types. The default
    /// [`AuditContext`] uses synthetic per-call ids — durable audit
    /// correlation between soldr's build session and zccache compiles is
    /// a Phase 5+ follow-up tracked in #977 open questions.
    ///
    /// `args[0]` from the soldr request is the rustc binary path; we
    /// pass it as `CompileRequest::compiler` and forward `args[1..]` as
    /// the actual rustc argument list, matching the wrapper's existing
    /// `args[2..]` convention (where soldr's argv[0] is the soldr
    /// binary and argv[1] is the tool path).
    pub async fn compile(
        &self,
        req: CompileRequest,
    ) -> Result<CompileResponseBody, EmbeddedServiceError> {
        let (compiler, rustc_args) = split_compiler_and_args(&req.args)?;
        let cwd: NormalizedPath = std::path::PathBuf::from(req.cwd).into();
        let audit = default_audit_context();
        let zreq = ZccacheCompileRequest {
            audit,
            compiler,
            args: rustc_args,
            cwd,
            env: req.env,
            stdin: req.stdin,
        };
        // Keep zccache's compile state behind one heap indirection. Its
        // streaming implementation nests a large compile pipeline future;
        // carrying that state inline makes this adapter's callers inherit the
        // full stack footprint even when they only use the buffered API.
        let zresp = Box::pin(self.inner.compile(zreq))
            .await
            .map_err(|e| EmbeddedServiceError::Compile(e.to_string()))?;
        let stderr = if zresp.cached {
            strip_internal_soldr_fallback_notices(zresp.stderr)
        } else {
            zresp.stderr
        };
        let stderr = crate::compiler_exit::annotate_signal_termination(zresp.exit_code, stderr);
        Ok(CompileResponseBody {
            exit_code: zresp.exit_code,
            stdout: zresp.stdout,
            stderr,
            cached: zresp.cached,
            cache_outcome: encode_cache_outcome(zresp.cache_outcome),
        })
    }

    /// Drain pending writes — called from the `Request::FlushCaches`
    /// arm (`soldr save` / `soldr cache flush`) so the on-disk cache
    /// tree is complete before archiving.
    pub async fn flush(&self) -> Result<CacheFlushInfo, EmbeddedServiceError> {
        let report = self
            .inner
            .flush_detailed()
            .await
            .map_err(|e| EmbeddedServiceError::Flush(e.to_string()))?;
        let complete = report.is_complete();
        Ok(CacheFlushInfo {
            complete,
            pending_writes_drained: report.pending_writes_drained,
            index_writer_drained: report.index_writer_drained,
            steps: report
                .steps
                .into_iter()
                .map(|step| {
                    let (status, error) = match step.outcome {
                        FlushStepOutcome::Completed => ("completed", None),
                        FlushStepOutcome::Failed(error) => ("failed", Some(error)),
                        FlushStepOutcome::TimedOut => ("timed_out", None),
                    };
                    CacheFlushStepInfo {
                        step: step.step,
                        status: status.to_owned(),
                        error,
                    }
                })
                .collect(),
            artifact_entries: report.artifact_entries,
            metadata_entries: report.metadata_entries,
        })
    }

    /// Return the embedded service's cumulative compile counters as the
    /// soldr-daemon protocol type (soldr#1368). Keeps `zccache::embedded::*`
    /// types out of `daemon::server` — the conversion lives here.
    pub async fn stats(&self) -> Result<CompileStatsInfo, EmbeddedServiceError> {
        let s = self
            .inner
            .stats()
            .await
            .map_err(|e| EmbeddedServiceError::Stats(e.to_string()))?;
        Ok(CompileStatsInfo {
            total_compilations: s.total_compilations,
            cache_hits: s.cache_hits,
            cache_misses: s.cache_misses,
            non_cacheable: s.non_cacheable,
            compile_errors: s.compile_errors,
            time_saved_ms: s.time_saved_ms,
            staged_profile: Some(StagedProfileInfo {
                counters: s.phase_profile.staged.counters,
                timings_ns: s.phase_profile.staged.timings_ns,
                bytes: s.phase_profile.staged.bytes,
                failures: s.phase_profile.staged.failures,
            }),
        })
    }

    /// Coordinate a host-requested pass with zccache's own startup/periodic
    /// worker.  Upstream serializes the pass against publication and confines
    /// it to this service's exact configured artifact root (#1148).
    pub async fn maintain_disk(
        &self,
        full: bool,
    ) -> Result<EmbeddedDiskMaintenanceReport, EmbeddedServiceError> {
        let report = self
            .inner
            .maintain_disk(if full {
                DiskMaintenanceKind::Full
            } else {
                DiskMaintenanceKind::Pressure
            })
            .await
            .map_err(|error| EmbeddedServiceError::Maintenance(error.to_string()))?;
        Ok(EmbeddedDiskMaintenanceReport {
            kind: match report.kind {
                DiskMaintenanceKind::Pressure => "pressure",
                DiskMaintenanceKind::Full => "full",
            }
            .to_string(),
            pressure: match report.pressure {
                DiskMaintenancePressure::None => "none",
                DiskMaintenancePressure::Soft => "soft",
                DiskMaintenancePressure::Hard => "hard",
            }
            .to_string(),
            budget_bytes: report.budget_bytes,
            usage_before_bytes: report.usage_before_bytes,
            usage_after_bytes: report.usage_after_bytes,
            bytes_reclaimed: report.bytes_reclaimed,
            artifacts_removed: report.artifacts_removed,
            expired_artifacts_removed: report.expired_artifacts_removed,
            pending_write_bytes: report.pending_write_bytes,
        })
    }

    /// Graceful shutdown — called from the daemon's normal exit path
    /// after the accept loop has been aborted.
    pub async fn shutdown(self, mode: ShutdownMode) -> Result<(), EmbeddedServiceError> {
        // `ZccacheService` is intentionally Clone: each clone shares the
        // shutdown flag and daemon state. Consume a clone of the service value
        // here instead of trying to unwrap soldr's Arc. Signal-handler and
        // connection tasks may still hold soldr State clones during teardown;
        // requiring Arc uniqueness therefore reduced "shutdown" to a flush and
        // left zccache's index writer alive until Tokio runtime drop.
        self.inner
            .as_ref()
            .clone()
            .shutdown_detailed(mode)
            .await
            .map_err(|e| EmbeddedServiceError::Shutdown(e.to_string()))
            .and_then(|report| ensure_complete_shutdown(&report))
    }
}

fn prepare_embedded_cache_root(
    paths: &SoldrPaths,
    daemon_identity: &DaemonProcess,
    cache_root: &std::path::Path,
) -> Result<(), EmbeddedServiceError> {
    migrate_legacy_cache_root(paths, daemon_identity, cache_root)?;
    std::fs::create_dir_all(cache_root)?;
    crate::cache_lib::path_safety::validate_owned_directory(&paths.root, cache_root)?;
    let version_root = cache_root.join(zccache::core::config::versioned_subdir());
    std::fs::create_dir_all(&version_root)?;
    crate::cache_lib::path_safety::validate_owned_directory(&paths.root, &version_root)?;
    Ok(())
}

fn disk_cache_limits_from_env(
) -> Result<(DiskCacheLimits, EmbeddedDiskPolicy), EmbeddedServiceError> {
    const BYTES_ENV: &str = "ZCCACHE_CACHE_SIZE_BYTES";
    const PERCENT_ENV: &str = "ZCCACHE_CACHE_SIZE_PERCENT";
    let bytes_raw = std::env::var(BYTES_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let percent_raw = std::env::var(PERCENT_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    disk_cache_limits_from_values(bytes_raw.as_deref(), percent_raw.as_deref())
}

fn disk_cache_limits_from_values(
    bytes_raw: Option<&str>,
    percent_raw: Option<&str>,
) -> Result<(DiskCacheLimits, EmbeddedDiskPolicy), EmbeddedServiceError> {
    const BYTES_ENV: &str = "ZCCACHE_CACHE_SIZE_BYTES";
    const PERCENT_ENV: &str = "ZCCACHE_CACHE_SIZE_PERCENT";
    if bytes_raw.is_some() && percent_raw.is_some() {
        return Err(EmbeddedServiceError::Start(format!(
            "{BYTES_ENV} and {PERCENT_ENV} are mutually exclusive"
        )));
    }
    let max_cache_bytes = bytes_raw
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                EmbeddedServiceError::Start(format!(
                    "{BYTES_ENV} must be a positive integer byte count"
                ))
            })
        })
        .transpose()?;
    if max_cache_bytes == Some(0) {
        return Err(EmbeddedServiceError::Start(format!(
            "{BYTES_ENV} must be greater than zero"
        )));
    }
    let max_cache_percent = percent_raw
        .map(|value| {
            value.parse::<u8>().map_err(|_| {
                EmbeddedServiceError::Start(format!(
                    "{PERCENT_ENV} must be an integer from 1 through 100"
                ))
            })
        })
        .transpose()?;
    if max_cache_percent.is_some_and(|percent| !(1..=100).contains(&percent)) {
        return Err(EmbeddedServiceError::Start(format!(
            "{PERCENT_ENV} must be an integer from 1 through 100"
        )));
    }
    let source = if max_cache_bytes.is_some() {
        "explicit_bytes"
    } else if max_cache_percent.is_some() {
        "explicit_percent"
    } else {
        "dynamic_5_percent_clamped_40_200_gib"
    };
    Ok((
        DiskCacheLimits {
            max_cache_bytes,
            max_cache_percent,
        },
        EmbeddedDiskPolicy {
            source: source.to_string(),
            max_cache_bytes,
            max_cache_percent,
        },
    ))
}

#[cfg(test)]
mod disk_limit_tests {
    use super::*;

    crate::timed_test!(disk_limit_overrides_are_validated_and_mutually_exclusive, {
        let (_, dynamic) = disk_cache_limits_from_values(None, None).unwrap();
        assert_eq!(dynamic.source, "dynamic_5_percent_clamped_40_200_gib");
        let (_, bytes) = disk_cache_limits_from_values(Some("42949672960"), None).unwrap();
        assert_eq!(bytes.max_cache_bytes, Some(40 * 1024 * 1024 * 1024));
        let (_, percent) = disk_cache_limits_from_values(None, Some("7")).unwrap();
        assert_eq!(percent.max_cache_percent, Some(7));
        assert!(disk_cache_limits_from_values(Some("1"), Some("5")).is_err());
        assert!(disk_cache_limits_from_values(Some("0"), None).is_err());
        assert!(disk_cache_limits_from_values(None, Some("101")).is_err());
    });
}

#[cfg(test)]
mod journal_migration_tests {
    use super::*;

    crate::timed_test!(startup_scrubs_live_and_rotated_pre_redaction_journals, {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../_vender/zccache/crates/zccache-daemon-core/src/daemon/compile_journal/tests/compile_journal_env_security_v1.json"
        ))
        .unwrap();
        let legacy = serde_json::to_string(&fixture["legacy_record"]).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let current = embedded_compile_journal_path(&paths);
        let rotated = current.with_file_name("compile_journal.jsonl.123");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        for path in [&current, &rotated] {
            std::fs::write(path, format!("{legacy}\nnot-json-with-secret\n")).unwrap();
        }

        scrub_existing_compile_journals(&paths).unwrap();

        for path in [&current, &rotated] {
            let body = std::fs::read_to_string(path).unwrap();
            assert!(!body.contains("legacy-full-env-token"));
            assert!(!body.contains("UNRESTRICTED_LEGACY_VARIABLE"));
            assert!(!body.contains("not-json-with-secret"));
            let row: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
            assert!(row.get("env").is_none());
        }
        assert!(current.starts_with(paths.cache.join("zccache/daemon-state/embedded-v1")));
    });
}

fn ensure_complete_shutdown(
    report: &zccache::embedded::DetailedShutdownReport,
) -> Result<(), EmbeddedServiceError> {
    if report.flushed.is_complete() {
        return Ok(());
    }

    Err(EmbeddedServiceError::Shutdown(format!(
        "cache checkpoint incomplete: pending_writes_drained={}, index_writer_drained={}, steps={:?}",
        report.flushed.pending_writes_drained,
        report.flushed.index_writer_drained,
        report.flushed.steps
    )))
}

/// Build the default per-call `AuditContext` for soldr-issued compiles
/// (issue #977 Phase 5). Each call gets a fresh synthetic run_id +
/// trace_id; the daemon already carries build-session correlation via
/// `BuildSessionStart`/`End`, so threading it through here is a Phase
/// 5+ refinement.
fn default_audit_context() -> AuditContext {
    use zccache::audit::AuditId;
    let id = uuid_like_random_id();
    // `AuditId::new` only fails on the empty string, which uuid_like
    // cannot produce. unwrap is intentional and unreachable in practice.
    let run_id = AuditId::new(id.clone()).expect("non-empty audit id");
    let trace_id = AuditId::new(id).expect("non-empty audit id");
    AuditContext::new(run_id, trace_id)
}

/// Generate a 32-char hex id without pulling in `uuid` purely for this
/// module. blake3 of `(pid, current monotonic nanos)` is plenty random
/// for per-call audit ids and avoids a new dependency.
fn uuid_like_random_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = StreamHasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    hex::encode(&hasher.finalize().as_bytes()[..16])
}
/// Encode [`CacheOutcome`] as the integer protocol expects on the wire:
/// 1=Hit, 2=Miss, 3=Error. Kept inside the embedded module so the
/// soldr daemon layer never imports `CacheOutcome` directly.
fn encode_cache_outcome(outcome: CacheOutcome) -> i32 {
    match outcome {
        CacheOutcome::Hit => 1,
        CacheOutcome::Miss => 2,
        CacheOutcome::Error => 3,
    }
}

/// Soldr wrapper failures are operational diagnostics, not compiler output.
/// Older cache entries can contain the once-per-wrapper fallback notice and
/// Cargo persists any replayed stderr under `.fingerprint/output-*`, turning
/// one transient outage into permanent warm-build spam. Drop only that exact
/// internal line at the embedded-cache response boundary.
pub fn strip_internal_soldr_fallback_notices(stderr: Vec<u8>) -> Vec<u8> {
    const PREFIX: &[u8] = b"soldr: compile daemon unavailable after ";
    const MIDDLE: &[u8] =
        b"ms \xe2\x80\x94 falling back to direct uncached rustc (soldr#1657); reason=";
    fn is_internal_notice(line: &[u8]) -> bool {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(rest) = line.strip_prefix(PREFIX) else {
            return false;
        };
        let Some(middle_at) = rest
            .windows(MIDDLE.len())
            .position(|window| window == MIDDLE)
        else {
            return false;
        };
        let (budget, suffix) = rest.split_at(middle_at);
        !budget.is_empty()
            && budget.iter().all(u8::is_ascii_digit)
            && suffix
                .strip_prefix(MIDDLE)
                .is_some_and(|reason| !reason.is_empty())
    }

    if !stderr.split(|byte| *byte == b'\n').any(is_internal_notice) {
        return stderr;
    }

    let mut filtered = Vec::with_capacity(stderr.len());
    for line in stderr.split_inclusive(|byte| *byte == b'\n') {
        if !is_internal_notice(line) {
            filtered.extend_from_slice(line);
        }
    }
    filtered
}

#[cfg(test)]
mod fallback_output_tests {
    use super::strip_internal_soldr_fallback_notices;

    crate::timed_test!(
        cached_internal_fallback_notice_is_removed_without_touching_diagnostics,
        {
            let input = b"warning: first\n\
soldr: compile daemon unavailable after 30000ms \xe2\x80\x94 falling back to direct uncached rustc (soldr#1657); reason=daemon unavailable\n\
error[E0001]: real compiler diagnostic\r\n\
note: contains soldr: compile daemon unavailable after but is user text"
                .to_vec();
            let filtered = strip_internal_soldr_fallback_notices(input);
            assert_eq!(
                filtered,
                b"warning: first\n\
error[E0001]: real compiler diagnostic\r\n\
note: contains soldr: compile daemon unavailable after but is user text"
            );
        }
    );

    crate::timed_test!(near_prefix_compiler_diagnostic_is_preserved, {
        let input = b"soldr: compile daemon unavailable after lunch; this is user text\n".to_vec();
        assert_eq!(strip_internal_soldr_fallback_notices(input.clone()), input);
    });

    crate::timed_test!(compiler_stderr_without_internal_notice_is_byte_identical, {
        let input = b"\0non-utf8:\xff\r\nreal stderr without trailing newline".to_vec();
        assert_eq!(strip_internal_soldr_fallback_notices(input.clone()), input);
    });
}

/// Pull the rustc binary path out of the wrapper's argv. The wrapper
/// sends `args[0] = rustc-path`, `args[1..] = rustc-args` (i.e. it has
/// already stripped its own argv[0] = soldr-binary entry). An empty
/// args list is a soldr-side bug — surface it as `EmptyArgs` so the
/// daemon returns a clean structured error instead of panicking.
fn split_compiler_and_args(
    args: &[String],
) -> Result<(NormalizedPath, Vec<String>), EmbeddedServiceError> {
    let mut it = args.iter().cloned();
    let compiler_str = it.next().ok_or(EmbeddedServiceError::EmptyArgs)?;
    let compiler: NormalizedPath = std::path::PathBuf::from(compiler_str).into();
    Ok((compiler, it.collect()))
}

fn derive_identity() -> HostIdentity {
    let id = "embedded-v1".to_string();
    HostIdentity {
        product: "soldr".to_string(),
        instance_id: id.clone(),
        workspace_id: id,
    }
}

/// Effective version directory used by the embedded backend. Keeping this in
/// the sole zccache adapter prevents CLI history from guessing the layout.
pub fn embedded_version_root(paths: &SoldrPaths) -> PathBuf {
    private_zccache_cache_root(paths, &derive_identity())
        .join(zccache::core::config::versioned_subdir())
}

pub fn embedded_compile_journal_path(paths: &SoldrPaths) -> PathBuf {
    embedded_version_root(paths)
        .join("logs")
        .join("compile_journal.jsonl")
}

/// Remove pre-#1149 raw environment values from live and rotated journals
/// before zccache opens the current writer. Invalid legacy lines are dropped
/// closed because retaining an unparseable line could retain a credential.
fn scrub_existing_compile_journals(paths: &SoldrPaths) -> std::io::Result<()> {
    let logs = embedded_version_root(paths).join("logs");
    match std::fs::symlink_metadata(&logs) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(_) => crate::cache_lib::path_safety::validate_owned_directory(&paths.root, &logs)?,
    }
    for entry in std::fs::read_dir(&logs)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name != "compile_journal.jsonl" && !name.starts_with("compile_journal.jsonl.") {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if !metadata.is_file() || crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unsafe compile journal path: {}", entry.path().display()),
            ));
        }
        scrub_compile_journal_file(&entry.path())?;
    }
    Ok(())
}

fn scrub_compile_journal_file(path: &std::path::Path) -> std::io::Result<()> {
    let body = std::fs::read_to_string(path)?;
    let mut sanitized = String::new();
    for line in body.lines() {
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(object) = value.as_object_mut() else {
            continue;
        };
        if let Some(raw_env) = object.remove("env") {
            if let Ok(env) = serde_json::from_value::<Vec<(String, String)>>(raw_env) {
                if let Some(env) = zccache::daemon::compile_journal::sanitize_journal_env(Some(env))
                {
                    object.insert(
                        "env".to_string(),
                        serde_json::to_value(env).map_err(std::io::Error::other)?,
                    );
                }
            }
        }
        sanitized.push_str(&serde_json::to_string(&value).map_err(std::io::Error::other)?);
        sanitized.push('\n');
    }
    let temp = path.with_extension(format!("scrub-{}.tmp", std::process::id()));
    std::fs::write(&temp, sanitized)?;
    if let Err(error) = std::fs::rename(&temp, path) {
        if path.exists() {
            std::fs::remove_file(path)?;
            std::fs::rename(&temp, path)?;
        } else {
            return Err(error);
        }
    }
    Ok(())
}

/// Re-home the pre-#1651 backend directory into the stable namespace.
///
/// The legacy identity hashed `(SoldrPaths::root, current_exe path)`. For a
/// normal in-place upgrade we can derive that exact path and prefer it even
/// when stale sibling identities exist. A cache restored to a different root
/// cannot recover the old root string, so it selects the uniquely most-recent
/// legacy backend instead. `soldr save` flushes the active backend immediately
/// before archiving and preserves nanosecond mtimes, making that ordering
/// durable across load. A tied newest mtime is rejected rather than silently
/// starting with an arbitrary cold cache.
fn migrate_legacy_cache_root(
    paths: &SoldrPaths,
    daemon_identity: &DaemonProcess,
    stable_root: &std::path::Path,
) -> Result<(), EmbeddedServiceError> {
    if stable_root.exists() {
        return Ok(());
    }

    let parent = stable_root
        .parent()
        .expect("private zccache cache root always has a parent");
    if !parent.exists() {
        return Ok(());
    }
    crate::cache_lib::path_safety::validate_owned_directory(&paths.root, parent)?;

    let exact_legacy = private_zccache_cache_root(
        paths,
        &derive_legacy_identity(paths, &daemon_identity.exe_path),
    );
    if std::fs::symlink_metadata(&exact_legacy).is_ok_and(|metadata| {
        metadata.is_dir() && !crate::cache_lib::path_safety::is_link_or_reparse(&metadata)
    }) {
        std::fs::rename(&exact_legacy, stable_root)?;
        tracing::info!(
            from = %exact_legacy.display(),
            to = %stable_root.display(),
            "migrated exact legacy embedded zccache backend"
        );
        return Ok(());
    }

    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir()
            && !crate::cache_lib::path_safety::is_link_or_reparse(&metadata)
            && is_legacy_identity_name(&entry.file_name())
        {
            candidates.push((latest_tree_mtime(&entry.path())?, entry.path()));
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let selected = select_legacy_candidate(parent, candidates)?;
    std::fs::rename(&selected, stable_root)?;
    tracing::warn!(
        from = %selected.display(),
        to = %stable_root.display(),
        "migrated most recently flushed legacy embedded zccache backend from a relocated cache"
    );
    Ok(())
}

fn select_legacy_candidate(
    parent: &std::path::Path,
    mut candidates: Vec<(SystemTime, PathBuf)>,
) -> Result<PathBuf, EmbeddedServiceError> {
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    if candidates.len() > 1 && candidates[0].0 == candidates[1].0 {
        let newest = candidates[0].0;
        return Err(EmbeddedServiceError::AmbiguousLegacyCache {
            root: parent.to_path_buf(),
            candidates: candidates
                .into_iter()
                .take_while(|(mtime, _)| *mtime == newest)
                .map(|(_, path)| path)
                .collect(),
        });
    }
    Ok(candidates
        .into_iter()
        .next()
        .expect("caller rejects an empty legacy candidate list")
        .1)
}

fn derive_legacy_identity(paths: &SoldrPaths, exe_path: &std::path::Path) -> HostIdentity {
    let mut hasher = StreamHasher::new();
    hasher.update(paths.root.as_os_str().to_string_lossy().as_bytes());
    hasher.update(exe_path.as_os_str().to_string_lossy().as_bytes());
    let id = hex::encode(&hasher.finalize().as_bytes()[..16]);
    HostIdentity {
        product: "soldr".to_string(),
        instance_id: id.clone(),
        workspace_id: id,
    }
}

fn is_legacy_identity_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn latest_tree_mtime(root: &std::path::Path) -> Result<SystemTime, std::io::Error> {
    let root_metadata = std::fs::symlink_metadata(root)?;
    if crate::cache_lib::path_safety::is_link_or_reparse(&root_metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("linked cache tree retained: {}", root.display()),
        ));
    }
    let mut newest = root_metadata.modified()?;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("linked cache entry retained: {}", entry.path().display()),
                ));
            }
            let modified = metadata.modified()?;
            newest = newest.max(modified);
            if metadata.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(newest)
}

/// Reclaim stale soldr-owned embedded generations beneath exactly one selected
/// product root.  The active stable identity and current version are always
/// protected; links and every sibling product root are ignored.
pub fn sweep_legacy_cache_roots(
    paths: &SoldrPaths,
    now: SystemTime,
    max_age: std::time::Duration,
) -> LegacyCacheSweepReport {
    let zccache_root = paths.cache.join("zccache");
    let daemon_state = zccache_root.join("daemon-state");
    let embedded_root = daemon_state.join("embedded-v1");
    let current_version = zccache::core::config::versioned_subdir();
    let mut report = LegacyCacheSweepReport::default();
    if !zccache_root.exists() {
        return report;
    }
    for root in [&zccache_root, &daemon_state, &embedded_root] {
        if root.exists()
            && crate::cache_lib::path_safety::validate_owned_directory(&paths.root, root).is_err()
        {
            report.failed += 1;
            return report;
        }
    }
    let mut candidates = Vec::new();
    if daemon_state.exists() {
        match std::fs::read_dir(&daemon_state) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) if is_legacy_identity_name(&entry.file_name()) => {
                            candidates.push(entry.path());
                        }
                        Ok(_) => {}
                        Err(_) => report.failed += 1,
                    }
                }
            }
            Err(_) => report.failed += 1,
        }
    }
    for (root, protect_current) in [(&zccache_root, false), (&embedded_root, true)] {
        if !root.exists() {
            continue;
        }
        match std::fs::read_dir(root) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => {
                            let name = entry.file_name();
                            if name.to_str().is_some_and(|name| {
                                zccache::core::config::is_version_dir_name(name)
                                    && (!protect_current || name != current_version)
                            }) {
                                candidates.push(entry.path());
                            }
                        }
                        Err(_) => report.failed += 1,
                    }
                }
            }
            Err(_) => report.failed += 1,
        }
    }

    for path in candidates {
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            report.failed += 1;
            continue;
        };
        if !metadata.is_dir() || crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
            report.failed += 1;
            continue;
        }
        let Ok(modified) = latest_tree_mtime(&path) else {
            report.failed += 1;
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() < max_age {
            continue;
        }
        let bytes = crate::cache_lib::target_registry::directory_size(&path);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                report.removed += 1;
                report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(bytes);
            }
            Err(_) => report.failed += 1,
        }
    }
    report
}

#[cfg(test)]
mod legacy_gc_tests {
    use super::*;

    crate::timed_test!(legacy_sweep_protects_current_and_sibling_roots, {
        let temp = tempfile::tempdir().unwrap();
        let owned = SoldrPaths::with_root(temp.path().join(".soldr"));
        let sibling = SoldrPaths::with_root(temp.path().join(".soldr-dev"));
        let legacy = owned
            .cache
            .join("zccache/daemon-state/0123456789abcdef0123456789abcdef");
        let embedded = owned.cache.join("zccache/daemon-state/embedded-v1");
        let current = embedded.join(zccache::core::config::versioned_subdir());
        let nested_old_version = embedded.join("v0.0.1");
        let top_old_version = owned.cache.join("zccache/v0.0.2");
        // Top-level versions belong to the removed standalone/legacy layout,
        // even when their version text happens to equal the embedded build.
        let top_current_version = owned
            .cache
            .join("zccache")
            .join(zccache::core::config::versioned_subdir());
        let malformed = owned.cache.join("zccache/vprivate");
        let sibling_sentinel = sibling
            .cache
            .join("zccache/daemon-state/0123456789abcdef0123456789abcdef/sentinel");
        for path in [
            legacy.join("artifact"),
            current.join("artifact"),
            nested_old_version.join("artifact"),
            top_old_version.join("artifact"),
            top_current_version.join("artifact"),
            malformed.join("artifact"),
            sibling_sentinel.clone(),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"payload").unwrap();
        }

        let report = sweep_legacy_cache_roots(&owned, SystemTime::now(), std::time::Duration::ZERO);
        assert_eq!(report.removed, 4);
        assert!(!legacy.exists());
        assert!(!nested_old_version.exists());
        assert!(!top_old_version.exists());
        assert!(!top_current_version.exists());
        assert!(current.join("artifact").is_file());
        assert!(malformed.join("artifact").is_file());
        assert!(sibling_sentinel.is_file());
    });

    #[cfg(unix)]
    crate::timed_test!(legacy_sweep_retains_version_with_unreadable_linked_tree, {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        let candidate = paths.cache.join("zccache/v0.0.1");
        std::fs::create_dir_all(&candidate).unwrap();
        std::os::unix::fs::symlink(candidate.join("missing"), candidate.join("broken")).unwrap();
        let report = sweep_legacy_cache_roots(&paths, SystemTime::now(), std::time::Duration::ZERO);
        assert_eq!(report.removed, 0);
        assert_eq!(report.failed, 1);
        assert!(candidate.is_dir());
    });
}

fn private_zccache_cache_root(paths: &SoldrPaths, identity: &HostIdentity) -> std::path::PathBuf {
    paths
        .cache
        .join("zccache")
        .join("daemon-state")
        .join(&identity.instance_id)
}

/// Top-level cache root passed to the embedded zccache service.
///
/// Callers that need to locate zccache-owned state should derive it from this
/// shared boundary rather than duplicating the stable host identity.
pub fn embedded_cache_root(paths: &SoldrPaths) -> std::path::PathBuf {
    private_zccache_cache_root(paths, &derive_identity())
}

#[cfg(test)]
#[path = "zccache_embedded_process_tests.rs"]
mod zccache_embedded_process_tests;

#[cfg(test)]
mod private_root_tests {
    #[cfg(windows)]
    use super::zccache_embedded_process_tests::contained_status;
    use super::zccache_embedded_process_tests::{bounded_output, CompilerProbeOutput};
    use super::*;

    fn shutdown_report(
        pending_writes_drained: bool,
        index_writer_drained: bool,
        outcome: FlushStepOutcome,
    ) -> zccache::embedded::DetailedShutdownReport {
        zccache::embedded::DetailedShutdownReport {
            mode: ShutdownMode::Graceful,
            flushed: zccache::embedded::DetailedFlushReport {
                pending_writes_drained,
                index_writer_drained,
                steps: vec![zccache::embedded::FlushStepReport {
                    step: "persist indexes".to_owned(),
                    outcome,
                }],
                artifact_entries: 1,
                metadata_entries: 1,
            },
        }
    }

    crate::timed_test!(shutdown_requires_a_complete_cache_checkpoint, {
        let complete = shutdown_report(true, true, FlushStepOutcome::Completed);
        ensure_complete_shutdown(&complete).expect("complete checkpoint");

        let incomplete = shutdown_report(true, false, FlushStepOutcome::TimedOut);
        let error = ensure_complete_shutdown(&incomplete).expect_err("incomplete checkpoint");
        let message = error.to_string();
        assert!(message.contains("cache checkpoint incomplete"));
        assert!(message.contains("index_writer_drained=false"));
        assert!(message.contains("TimedOut"));
    });

    fn validate_compiler_probe(
        path: &std::path::Path,
        output: Result<CompilerProbeOutput, std::io::Error>,
    ) -> Result<String, String> {
        let output = output.map_err(|error| {
            format!(
                "Rust compiler prerequisite failed: path={} spawn_error={error}",
                path.display()
            )
        })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.success {
            return Err(format!(
                "Rust compiler prerequisite failed: path={} exit_code={:?}\nstdout:\n{}\nstderr:\n{}",
                path.display(),
                output.exit_code,
                stdout,
                stderr
            ));
        }
        let version = stdout.trim();
        let mut lines = version.lines();
        let has_rustc_version = lines.next().is_some_and(|line| line.starts_with("rustc "));
        let has_host = lines.any(|line| line.starts_with("host: "));
        if !has_rustc_version || !has_host {
            return Err(format!(
                "Rust compiler prerequisite failed: path={} exit_code={:?} unexpected rustc -vV output\nstdout:\n{}\nstderr:\n{}",
                path.display(),
                output.exit_code,
                stdout,
                stderr
            ));
        }
        Ok(version.to_string())
    }

    fn probe_working_compiler(path: &std::path::Path) -> Result<String, String> {
        let mut command = std::process::Command::new(path);
        command.arg("-vV");
        let output = bounded_output(command).map(CompilerProbeOutput::from);
        validate_compiler_probe(path, output)
    }

    fn test_daemon_identity() -> DaemonProcess {
        use running_process::broker::protocol::Endpoint;
        DaemonProcess::current_process(
            Endpoint {
                namespace_id: "soldr-zccache-test".to_string(),
                path: "soldr-zccache-test.sock".to_string(),
            },
            None,
        )
        .expect("current test process identity")
    }

    crate::timed_test!(identity_is_portable_across_cache_roots, {
        let identity = derive_identity();
        let cold = SoldrPaths::with_root(std::path::PathBuf::from("/tmp/cache-cold"));
        let warm = SoldrPaths::with_root(std::path::PathBuf::from("/tmp/cache-warm"));

        let cold_root = private_zccache_cache_root(&cold, &identity);
        let warm_root = private_zccache_cache_root(&warm, &identity);
        assert_eq!(
            cold_root
                .strip_prefix(&cold.cache)
                .expect("cold cache prefix"),
            warm_root
                .strip_prefix(&warm.cache)
                .expect("warm cache prefix"),
            "save/load roots must select the same archived private subtree",
        );
    });

    crate::timed_test!(identity_survives_soldr_upgrades, {
        assert_eq!(derive_identity().instance_id, "embedded-v1");
    });

    crate::timed_test!(embedded_root_rejects_a_cross_product_link, {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("selected-product"));
        let daemon = test_daemon_identity();
        let stable = private_zccache_cache_root(&paths, &derive_identity());
        std::fs::create_dir_all(stable.parent().unwrap()).unwrap();
        let external = temp.path().join("other-product");
        std::fs::create_dir_all(&external).unwrap();
        let sentinel = external.join("sentinel");
        std::fs::write(&sentinel, b"keep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&external, &stable).unwrap();
        #[cfg(windows)]
        {
            let mut command = std::process::Command::new("cmd");
            command
                .args(["/c", "mklink", "/J"])
                .arg(&stable)
                .arg(&external);
            assert_eq!(contained_status(command).unwrap(), 0);
        }
        assert!(prepare_embedded_cache_root(&paths, &daemon, &stable).is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
        assert!(!external.join("logs").exists());
    });

    crate::timed_test!(embedded_version_root_rejects_a_cross_product_link, {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("selected-product"));
        let daemon = test_daemon_identity();
        let stable = private_zccache_cache_root(&paths, &derive_identity());
        std::fs::create_dir_all(&stable).unwrap();
        let version_root = stable.join(zccache::core::config::versioned_subdir());
        let external = temp.path().join("other-product-version");
        std::fs::create_dir_all(&external).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&external, &version_root).unwrap();
        #[cfg(windows)]
        {
            let mut command = std::process::Command::new("cmd");
            command
                .args(["/c", "mklink", "/J"])
                .arg(&version_root)
                .arg(&external);
            assert_eq!(contained_status(command).unwrap(), 0);
        }
        assert!(prepare_embedded_cache_root(&paths, &daemon, &stable).is_err());
        assert!(!external.join("logs").exists());
    });

    crate::timed_test!(exact_same_root_legacy_identity_wins_over_newer_siblings, {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let daemon = test_daemon_identity();
        let stable = private_zccache_cache_root(&paths, &derive_identity());
        let exact =
            private_zccache_cache_root(&paths, &derive_legacy_identity(&paths, &daemon.exe_path));
        let sibling = stable
            .parent()
            .expect("stable parent")
            .join("11111111111111111111111111111111");
        std::fs::create_dir_all(&exact).expect("create exact legacy root");
        std::fs::write(exact.join("selected"), b"exact").expect("write exact marker");
        std::fs::create_dir_all(&sibling).expect("create sibling legacy root");
        std::fs::write(sibling.join("selected"), b"sibling").expect("write sibling marker");

        migrate_legacy_cache_root(&paths, &daemon, &stable).expect("migrate exact root");

        assert_eq!(
            std::fs::read(stable.join("selected")).expect("read migrated marker"),
            b"exact"
        );
        assert!(sibling.is_dir(), "unselected sibling must remain untouched");
    });

    crate::timed_test!(relocated_legacy_cache_uses_uniquely_newest_backend, {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("relocated"));
        let daemon = test_daemon_identity();
        let stable = private_zccache_cache_root(&paths, &derive_identity());
        let parent = stable.parent().expect("stable parent");
        let older = parent.join("11111111111111111111111111111111");
        let newer = parent.join("22222222222222222222222222222222");
        std::fs::create_dir_all(&older).expect("create older legacy root");
        std::fs::write(older.join("selected"), b"older").expect("write older marker");
        std::thread::sleep(std::time::Duration::from_millis(25));
        std::fs::create_dir_all(&newer).expect("create newer legacy root");
        std::fs::write(newer.join("selected"), b"newer").expect("write newer marker");

        migrate_legacy_cache_root(&paths, &daemon, &stable).expect("migrate newest root");

        assert_eq!(
            std::fs::read(stable.join("selected")).expect("read migrated marker"),
            b"newer"
        );
        assert!(
            older.is_dir(),
            "unselected older root must remain untouched"
        );
    });

    crate::timed_test!(tied_legacy_candidates_are_rejected_loudly, {
        let parent = std::path::PathBuf::from("cache/zccache/daemon-state");
        let tied = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(7);
        let result = select_legacy_candidate(
            &parent,
            vec![
                (tied, parent.join("11111111111111111111111111111111")),
                (tied, parent.join("22222222222222222222222222222222")),
            ],
        );
        assert!(
            matches!(
                result,
                Err(EmbeddedServiceError::AmbiguousLegacyCache { .. })
            ),
            "equal newest mtimes must not choose an arbitrary backend: {result:?}"
        );
    });

    crate::timed_test!(save_load_restores_the_selected_private_subtree, {
        use crate::cache_lib::save::{
            load, save, LoadOptions, SaveOptions, SaveProfile, DEFAULT_ZSTD_LEVEL,
        };

        let temp = tempfile::tempdir().expect("tempdir");
        let cold = SoldrPaths::with_root(temp.path().join("cache-cold"));
        let warm = SoldrPaths::with_root(temp.path().join("cache-warm"));
        let identity = derive_identity();
        let cold_object = private_zccache_cache_root(&cold, &identity)
            .join("artifacts")
            .join("probe-object");
        std::fs::create_dir_all(cold_object.parent().expect("object parent"))
            .expect("create cold object directory");
        std::fs::write(&cold_object, b"portable-cache-object").expect("write cold object");

        let archive = temp.path().join("cache.tar.zst");
        save(&SaveOptions {
            workspace: None,
            cache_dir: Some(&cold.cache),
            out: &archive,
            zstd_level: DEFAULT_ZSTD_LEVEL,
            threads: Some(1),
            mtimes_only: false,
            profile: SaveProfile::Full,
        })
        .expect("save cold cache");
        load(&LoadOptions {
            archive: &archive,
            cache_dir: Some(&warm.cache),
            workspace: None,
            threads: Some(1),
            mtimes_only: false,
            profile_extract: false,
            auto_defender_exclude: false,
        })
        .expect("load warm cache");

        let warm_object = private_zccache_cache_root(&warm, &identity)
            .join("artifacts")
            .join("probe-object");
        assert_eq!(
            std::fs::read(warm_object).expect("read restored object"),
            b"portable-cache-object",
        );
    });

    #[cfg(unix)]
    crate::timed_test!(working_fake_compiler_probe_is_accepted, {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let compiler = temp.path().join("fake-compiler");
        std::fs::write(
            &compiler,
            "#!/bin/sh\nprintf 'rustc 1.94.1 (fake)\\nhost: fake-target\\n'\n",
        )
        .expect("write fake compiler");
        let mut permissions = std::fs::metadata(&compiler)
            .expect("fake compiler metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&compiler, permissions).expect("make fake compiler executable");

        let version = probe_working_compiler(&compiler).expect("working compiler probe");
        assert!(version.contains("rustc 1.94.1 (fake)"));
    });

    crate::timed_test!(unusable_proxy_probe_reports_complete_diagnostics, {
        let compiler = std::path::Path::new("rustc-proxy");
        let error = validate_compiler_probe(
            compiler,
            Ok(CompilerProbeOutput {
                success: false,
                exit_code: Some(1),
                stdout: b"proxy stdout".to_vec(),
                stderr: b"compiler component is not applicable".to_vec(),
            }),
        )
        .expect_err("unusable proxy must fail");
        assert!(error.contains("path=rustc-proxy"));
        assert!(error.contains("exit_code=Some(1)"));
        assert!(error.contains("proxy stdout"));
        assert!(error.contains("compiler component is not applicable"));
    });

    crate::timed_test!(successful_non_compiler_probe_is_rejected, {
        let compiler = std::path::Path::new("not-rustc");
        let error = validate_compiler_probe(
            compiler,
            Ok(CompilerProbeOutput {
                success: true,
                exit_code: Some(0),
                stdout: b"some unrelated executable\n".to_vec(),
                stderr: b"unexpected shim diagnostics".to_vec(),
            }),
        )
        .expect_err("non-rustc output must fail");
        assert!(error.contains("path=not-rustc"));
        assert!(error.contains("unexpected rustc -vV output"));
        assert!(error.contains("some unrelated executable"));
        assert!(error.contains("unexpected shim diagnostics"));
    });

    crate::timed_test!(missing_compiler_probe_reports_path_and_spawn_error, {
        let temp = tempfile::tempdir().expect("tempdir");
        let compiler = temp.path().join("missing-compiler");
        let error = probe_working_compiler(&compiler).expect_err("missing compiler must fail");
        assert!(error.contains(&format!("path={}", compiler.display())));
        assert!(error.contains("spawn_error="));
    });

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_rustc_hit_survives_full_and_ci_save_load_relocation() {
        use crate::cache_lib::save::{
            load, save, LoadOptions, SaveOptions, SaveProfile, DEFAULT_ZSTD_LEVEL,
        };

        let current_dir = std::env::current_dir().expect("resolve test working directory");
        let repo_workspace = current_dir
            .ancestors()
            .find(|candidate| candidate.join("rust-toolchain.toml").is_file())
            .expect("find repository rust-toolchain.toml from test working directory");
        let pinned_toolchain = crate::core::read_rust_toolchain_manifest(repo_workspace)
            .expect("read repository rust-toolchain.toml")
            .channel
            .expect("repository rust-toolchain.toml must declare a channel");
        let rustc = zccache::test_support::find_rustc()
            .expect("Rust compiler prerequisite failed: no compiler found on PATH");
        let compiler_version =
            probe_working_compiler(rustc.as_path()).unwrap_or_else(|error| panic!("{error}"));
        eprintln!(
            "using verified compiler {}: {}",
            rustc.as_path().display(),
            compiler_version.lines().next().unwrap_or("unknown version")
        );
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("workspace");
        std::fs::create_dir_all(project.join("src")).expect("create source directory");
        std::fs::write(
            project.join("src/lib.rs"),
            "pub fn portable_cache_answer() -> u32 { 1651 }\n",
        )
        .expect("write source");

        let rustc_args = vec![
            rustc.as_path().display().to_string(),
            "--edition".into(),
            "2021".into(),
            "--crate-type".into(),
            "lib".into(),
            "--crate-name".into(),
            "soldr_portable_cache".into(),
            "--emit=dep-info,metadata,link".into(),
            "-C".into(),
            "embed-bitcode=no".into(),
            "-C".into(),
            "metadata=z1651".into(),
            "-C".into(),
            "extra-filename=-z1651".into(),
            "--out-dir".into(),
            "target/debug/deps".into(),
            "src/lib.rs".into(),
        ];
        let mut compile_env: Vec<(String, String)> = std::env::vars()
            .filter(|(key, _)| key != "RUSTUP_TOOLCHAIN")
            .collect();
        compile_env.push(("RUSTUP_TOOLCHAIN".into(), pinned_toolchain));
        let request = || CompileRequest {
            args: rustc_args.clone(),
            cwd: project.display().to_string(),
            env: compile_env.clone(),
            stdin: Vec::new(),
            lifecycle: None,
            ipc_busy_retries: 0,
        };
        let daemon = test_daemon_identity();
        let cold = SoldrPaths::with_root(temp.path().join("cold-root"));
        let cold_service = SoldrZccacheService::start(&cold, &daemon)
            .await
            .expect("start cold embedded service");
        let first = cold_service.compile(request()).await.expect("cold compile");
        assert_eq!(
            first.exit_code,
            0,
            "cold rustc failed: {}",
            String::from_utf8_lossy(&first.stderr)
        );
        assert!(!first.cached, "first compile must populate the cache");
        assert_eq!(first.cache_outcome, 2, "first compile must be a miss");
        let flush = cold_service
            .flush()
            .await
            .expect("flush cold service before inspecting durable state");
        assert!(
            flush.is_complete(),
            "cold service durability barrier must complete before archive: {flush:?}"
        );
        let cold_stats = cold_service
            .inner
            .stats()
            .await
            .expect("read cold service stats");
        assert!(
            cold_stats.dep_graph_contexts > 0 && cold_stats.artifact_count > 0,
            "cold compile must populate depgraph and artifact state: {cold_stats:?}"
        );
        cold_service
            .shutdown(ShutdownMode::Graceful)
            .await
            .expect("shutdown cold service");

        for profile in [SaveProfile::Full, SaveProfile::Ci] {
            let archive = temp.path().join(format!("{}.tar.zst", profile.as_str()));
            save(&SaveOptions {
                workspace: None,
                cache_dir: Some(&cold.cache),
                out: &archive,
                zstd_level: DEFAULT_ZSTD_LEVEL,
                threads: Some(2),
                mtimes_only: false,
                profile,
            })
            .unwrap_or_else(|error| panic!("save {} profile: {error}", profile.as_str()));

            let warm =
                SoldrPaths::with_root(temp.path().join(format!("warm-{}-root", profile.as_str())));
            load(&LoadOptions {
                archive: &archive,
                cache_dir: Some(&warm.cache),
                workspace: None,
                threads: Some(2),
                mtimes_only: false,
                profile_extract: false,
                auto_defender_exclude: false,
            })
            .unwrap_or_else(|error| panic!("load {} profile: {error}", profile.as_str()));

            if project.join("target").exists() {
                std::fs::remove_dir_all(project.join("target"))
                    .expect("remove compiler outputs before restored hit");
            }
            let warm_service = SoldrZccacheService::start(&warm, &daemon)
                .await
                .unwrap_or_else(|error| {
                    panic!("start {} restored service: {error}", profile.as_str())
                });
            let restored_stats = warm_service.inner.stats().await.unwrap_or_else(|error| {
                panic!("read {} restored service stats: {error}", profile.as_str())
            });
            assert!(
                restored_stats.dep_graph_contexts > 0 && restored_stats.artifact_count > 0,
                "{} restore must load depgraph and artifact state: {restored_stats:?}",
                profile.as_str()
            );
            let restored = warm_service
                .compile(request())
                .await
                .unwrap_or_else(|error| panic!("{} restored compile: {error}", profile.as_str()));
            assert_eq!(
                restored.exit_code,
                0,
                "{} restored rustc failed: {}",
                profile.as_str(),
                String::from_utf8_lossy(&restored.stderr)
            );
            assert!(
                restored.cached,
                "{} save/load into another root must produce a real rustc cache hit; pre-compile stats: {restored_stats:?}",
                profile.as_str(),
            );
            assert_eq!(
                restored.cache_outcome,
                1,
                "{} restored compile must report Hit",
                profile.as_str()
            );
            warm_service
                .shutdown(ShutdownMode::Graceful)
                .await
                .unwrap_or_else(|error| {
                    panic!("shutdown {} restored service: {error}", profile.as_str())
                });
        }
    }

    crate::timed_test!(private_root_is_stable_per_backend_identity, {
        let paths = SoldrPaths::with_root(std::path::PathBuf::from("/tmp/soldr"));
        let first = HostIdentity {
            product: "soldr".into(),
            instance_id: "backend-a".into(),
            workspace_id: "workspace-a".into(),
        };
        let second = HostIdentity {
            product: "soldr".into(),
            instance_id: "backend-b".into(),
            workspace_id: "workspace-b".into(),
        };
        assert_eq!(
            private_zccache_cache_root(&paths, &first),
            paths.cache.join("zccache/daemon-state/backend-a")
        );
        assert_ne!(
            private_zccache_cache_root(&paths, &first),
            private_zccache_cache_root(&paths, &second)
        );
    });
}
