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
    compile_admission: Arc<crate::resident_compile_admission::ResidentCompileAdmission>,
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
    #[error("resident compile capacity failed: {0}")]
    ResidentCapacity(String),
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
            // `max_parallel_compiles: None`, so zccache
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
        // soldr#2932 / zccache#1539: zccache owns the one canonical compiler
        // capacity semaphore and fair shared/exclusive gate. It invokes this
        // Soldr-specific classifier only after cache-hit classification, then
        // acquires capacity -> resource admission immediately before spawning
        // a real compiler child. Keeping no Soldr-side gate here ensures an
        // eligible cache hit never drains ordinary compiler work.
        let compile_admission = Arc::new(
            crate::resident_compile_admission::ResidentCompileAdmission::new(resolved_jobs.jobs),
        );
        let host_admission: Arc<dyn zccache::embedded::HostAdmissionClassifier> =
            compile_admission.clone();
        let svc = ZccacheService::start_with_options_and_host_admission_classifier(
            cfg,
            crate::zccache_staging::options(&cache_root, disk_limits),
            host_admission,
        )
        .await
        .map_err(|e| EmbeddedServiceError::Start(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(svc),
            compile_admission,
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

    /// Reserve compile capacity for a resident compiler process.
    ///
    /// The returned guard releases the reservation when dropped. A valid
    /// reservation always leaves at least one slot for ordinary cache-miss
    /// compiler work.
    pub async fn acquire_resident_capacity(
        &self,
        permits: u32,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, EmbeddedServiceError> {
        self.compile_admission
            .acquire_resident(permits)
            .await
            .map_err(|error| EmbeddedServiceError::ResidentCapacity(error.to_string()))
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
        let ci_test_report = crate::ci_test_report::prepare(&req);
        let (compiler, rustc_args) = split_compiler_and_args(&req.args)?;
        let cwd: NormalizedPath = std::path::PathBuf::from(req.cwd).into();
        // Kept for the failure path: `cwd` is moved into the request below,
        // and soldr#2781's detector resolves relative source paths against it.
        let compile_cwd = cwd.as_path().to_path_buf();
        // soldr#2781: say so on the way IN, not only in the post-mortem. If
        // this process is killed for memory, the user needs to know which
        // file the compiler was holding -- and by then the compile is gone.
        if let Some(notice) = crate::amalgamation::compile_notice(&req.args, &compile_cwd) {
            eprintln!("{notice}");
        }
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
        let stderr = crate::compiler_exit::annotate_signal_termination(
            zresp.exit_code,
            stderr,
            &req.args,
            &compile_cwd,
        );
        if let Some(report) = ci_test_report {
            crate::ci_test_report::record(report, encode_cache_outcome(zresp.cache_outcome));
        }
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

    #[test]
    fn disk_limit_overrides_are_validated_and_mutually_exclusive() {
        let (_, dynamic) = disk_cache_limits_from_values(None, None).unwrap();
        assert_eq!(dynamic.source, "dynamic_5_percent_clamped_40_200_gib");
        let (_, bytes) = disk_cache_limits_from_values(Some("42949672960"), None).unwrap();
        assert_eq!(bytes.max_cache_bytes, Some(40 * 1024 * 1024 * 1024));
        let (_, percent) = disk_cache_limits_from_values(None, Some("7")).unwrap();
        assert_eq!(percent.max_cache_percent, Some(7));
        assert!(disk_cache_limits_from_values(Some("1"), Some("5")).is_err());
        assert!(disk_cache_limits_from_values(Some("0"), None).is_err());
        assert!(disk_cache_limits_from_values(None, Some("101")).is_err());
    }
}

#[cfg(test)]
mod journal_migration_tests {
    use super::*;

    #[test]
    fn startup_scrubs_live_and_rotated_pre_redaction_journals() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/zccache/compile_journal_env_security_v1.json"
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
    }
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

    #[test]
    fn cached_internal_fallback_notice_is_removed_without_touching_diagnostics() {
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

    #[test]
    fn near_prefix_compiler_diagnostic_is_preserved() {
        let input = b"soldr: compile daemon unavailable after lunch; this is user text\n".to_vec();
        assert_eq!(strip_internal_soldr_fallback_notices(input.clone()), input);
    }

    #[test]
    fn compiler_stderr_without_internal_notice_is_byte_identical() {
        let input = b"\0non-utf8:\xff\r\nreal stderr without trailing newline".to_vec();
        assert_eq!(strip_internal_soldr_fallback_notices(input.clone()), input);
    }
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

#[path = "zccache_embedded_legacy.rs"]
mod legacy;
pub(crate) use legacy::migrate_legacy_cache_root;
pub use legacy::sweep_legacy_cache_roots;
#[cfg(test)]
use legacy::{derive_legacy_identity, select_legacy_candidate};

// The broken-symlink retention test moved to `tests/daemon_zccache_embedded.rs`
// (`#![cfg(unix)]`) — creating a dangling link is inherently host-specific
// (#2493).

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
#[path = "zccache_embedded_private_root_tests.rs"]
mod private_root_tests;
