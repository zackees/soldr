//! First-class in-process zccache service API.
//!
//! This module exposes the embedded service contract used by host daemons that
//! already own a Tokio runtime. The service reuses the daemon compile/session
//! machinery directly and does not bind or listen on zccache IPC endpoints.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::core::NormalizedPath;
use crate::daemon::server::{
    EmbeddedCompileRequest, EmbeddedDaemon, EmbeddedFlushReport, EmbeddedStatsSnapshot,
    StreamingSink,
};

pub use crate::audit::{AuditConfig, AuditContext};

/// Result type used by the embedded service API.
pub type Result<T> = std::result::Result<T, EmbeddedError>;

/// Errors returned by the embedded service API.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddedError {
    #[error("failed to start embedded zccache service: {0}")]
    Start(String),
    #[error("embedded zccache compile failed: {0}")]
    Compile(String),
    #[error("embedded zccache service is already shut down")]
    ShutDown,
}

/// Opaque in-process zccache service handle.
#[derive(Clone)]
pub struct ZccacheService {
    daemon: Arc<EmbeddedDaemon>,
    shutdown: Arc<AtomicBool>,
}

/// Configuration for [`ZccacheService::start`].
#[derive(Debug, Clone)]
pub struct ZccacheConfig {
    pub host: HostIdentity,
    pub cache_root: NormalizedPath,
    pub audit: AuditConfig,
    pub limits: ServiceLimits,
    pub runtime: RuntimeHooks,
}

/// Host identity used to namespace and diagnose an embedded service instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentity {
    pub product: String,
    pub instance_id: String,
    pub workspace_id: String,
}

/// Runtime integration hooks reserved for host-owned Tokio runtimes.
#[derive(Debug, Clone, Default)]
pub struct RuntimeHooks {
    pub service_name: Option<String>,
}

/// Optional service limits. `None` means zccache's existing daemon defaults.
#[derive(Debug, Clone, Default)]
pub struct ServiceLimits {
    pub max_parallel_compiles: Option<usize>,
}

/// One compile invocation submitted to the embedded service.
#[derive(Debug, Clone)]
pub struct CompileRequest {
    pub audit: AuditContext,
    pub compiler: NormalizedPath,
    pub args: Vec<String>,
    pub cwd: NormalizedPath,
    pub env: Vec<(String, String)>,
    pub stdin: Vec<u8>,
}

/// Compile response returned by the embedded service.
#[derive(Debug, Clone)]
pub struct CompileResponse {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub cached: bool,
    pub cache_outcome: CacheOutcome,
    pub compile_id: String,
}

/// Conservative cache outcome exposed by the MVP embedded API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    Hit,
    Miss,
    Error,
}

/// Streaming compile event.
///
/// Phase 5b2 (soldr#983) introduced this enum so the embedded service can
/// emit captured rustc output as it arrives from the subprocess pipes
/// instead of holding the full `Vec<u8>` buffers until `wait_with_output`
/// returns. The terminal [`CompileChunk::Done`] event carries the exit
/// code, cache outcome, and `compile_id` (mirrors the buffered
/// [`CompileResponse`] metadata).
///
/// The `cache_outcome` field is encoded as an `i32` (1=Hit, 2=Miss,
/// 3=Error) to match the soldr-side wire format introduced in Phase 5b1
/// — keeping the conversion zero-cost on the daemon-side bridge.
#[derive(Debug, Clone)]
pub enum CompileChunk {
    /// One slice of rustc's captured stdout. Emitted as soon as the
    /// pipe-pumping task reads a non-empty buffer from the child.
    Stdout(Vec<u8>),
    /// Mirror of [`CompileChunk::Stdout`] for stderr. For MSVC compiles
    /// that depend on `/showIncludes` stderr parsing the embedded service
    /// falls back to the buffered fast-path — see notes in
    /// `handle_compile/pipeline/compile_exec.rs`.
    Stderr(Vec<u8>),
    /// Terminal event. The consumer MUST treat any frame after this as a
    /// protocol violation.
    Done {
        exit_code: i32,
        cached: bool,
        cache_outcome: CacheOutcome,
        compile_id: String,
    },
}

/// Sender half of the streaming compile mpsc.
///
/// The default channel capacity is 64 — enough to keep the rustc pipe-
/// pump task from blocking on a slow consumer while bounding daemon-side
/// memory under any single in-flight compile to ~64 × 64 KiB = 4 MiB.
pub const DEFAULT_STREAMING_CAPACITY: usize = 64;

/// Shutdown behavior requested by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
    Graceful,
    Force,
}

/// Report returned by [`ZccacheService::shutdown`].
#[derive(Debug, Clone)]
pub struct ShutdownReport {
    pub mode: ShutdownMode,
    pub flushed: FlushReport,
}

/// Report returned by [`ZccacheService::flush`].
#[derive(Debug, Clone)]
pub struct FlushReport {
    pub pending_writes_drained: bool,
    pub artifact_entries: u64,
    pub metadata_entries: u64,
}

/// Current service statistics.
#[derive(Debug, Clone)]
pub struct ServiceStats {
    pub cache_root: NormalizedPath,
    pub uptime_secs: u64,
    pub total_compilations: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub non_cacheable: u64,
    pub compile_errors: u64,
    pub compile_errors_cached: u64,
    pub time_saved_ms: u64,
    pub artifact_count: u64,
    pub cache_size_bytes: u64,
    pub metadata_entries: u64,
    pub dep_graph_contexts: u64,
    pub dep_graph_files: u64,
    pub sessions_total: u64,
    pub sessions_active: u64,
    pub phase_profile: crate::protocol::PhaseProfileSummary,
}

impl ZccacheService {
    /// Start an in-process zccache service on the caller's Tokio runtime.
    pub async fn start(config: ZccacheConfig) -> Result<Self> {
        let endpoint = embedded_endpoint(&config.host);
        let cache_root =
            crate::core::config::effective_cache_root_from_top_level(&config.cache_root);
        let daemon = EmbeddedDaemon::start(endpoint, cache_root)
            .await
            .map_err(|err| EmbeddedError::Start(err.to_string()))?;
        Ok(Self {
            daemon: Arc::new(daemon),
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Compile using the embedded daemon engine.
    ///
    /// Implemented as a thin wrapper around [`Self::compile_streaming`]
    /// that collects chunks into the buffered [`CompileResponse`] shape
    /// existing callers expect. The mpsc capacity is sized large enough
    /// (1024) that a fast-producing rustc never blocks waiting for the
    /// collector — the buffered API was never bounded so the streaming
    /// reshape must not introduce backpressure regressions for it.
    pub async fn compile(&self, request: CompileRequest) -> Result<CompileResponse> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<CompileChunk>(1024);

        // Drive the streaming side and the collector concurrently. The
        // streaming task ends by sending `Done`; the collector loop
        // returns once `rx` is closed.
        let request_clone = request.clone();
        let stream_fut = async move { self.compile_streaming(request_clone, tx).await };
        let collect_fut = async move {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut tail: Option<(i32, bool, CacheOutcome, String)> = None;
            while let Some(chunk) = rx.recv().await {
                match chunk {
                    CompileChunk::Stdout(bytes) => stdout.extend_from_slice(&bytes),
                    CompileChunk::Stderr(bytes) => stderr.extend_from_slice(&bytes),
                    CompileChunk::Done {
                        exit_code,
                        cached,
                        cache_outcome,
                        compile_id,
                    } => {
                        tail = Some((exit_code, cached, cache_outcome, compile_id));
                    }
                }
            }
            (stdout, stderr, tail)
        };

        let (stream_result, (stdout, stderr, tail)) = tokio::join!(stream_fut, collect_fut);
        stream_result?;
        let (exit_code, cached, cache_outcome, compile_id) = tail.ok_or_else(|| {
            EmbeddedError::Compile(
                "embedded compile_streaming finished without CompileChunk::Done".into(),
            )
        })?;
        Ok(CompileResponse {
            exit_code,
            stdout,
            stderr,
            cached,
            cache_outcome,
            compile_id,
        })
    }

    /// Streaming variant of [`Self::compile`]. Each chunk of rustc's
    /// stdout / stderr emits a [`CompileChunk::Stdout`] /
    /// [`CompileChunk::Stderr`] event as it arrives from the subprocess
    /// pipes (not after `wait_with_output`). The terminal
    /// [`CompileChunk::Done`] event carries the exit code, cache outcome,
    /// and `compile_id`.
    ///
    /// Phase 5b2 (soldr#983) — the wire-side streaming groundwork landed
    /// in soldr-side commit 82e26f4; this is the daemon-side source that
    /// finally takes the per-compile buffer hop off the critical path.
    /// The on-disk artifact store and post-compile hashing pipeline still
    /// receive a fully accumulated `Arc<Vec<u8>>` because they need it
    /// for cached-error replay; the streaming win lives in the IPC
    /// shipping path between the rustc child and the wrapper process.
    pub async fn compile_streaming(
        &self,
        request: CompileRequest,
        sink: tokio::sync::mpsc::Sender<CompileChunk>,
    ) -> Result<()> {
        let compile_id = request
            .audit
            .compile_id
            .clone()
            .or_else(|| request.audit.command_id.clone())
            .map(String::from)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EmbeddedError::ShutDown);
        }
        let streaming_sink = StreamingSink::new(sink.clone());
        let response = self
            .daemon
            .compile_streaming(
                EmbeddedCompileRequest {
                    compiler: request.compiler.into_path_buf(),
                    args: request.args,
                    cwd: request.cwd.into_path_buf(),
                    env: Some(request.env),
                    stdin: request.stdin,
                },
                streaming_sink.clone(),
            )
            .await
            .map_err(EmbeddedError::Compile)?;
        let cache_outcome = if response.exit_code != 0 {
            CacheOutcome::Error
        } else if response.cached {
            CacheOutcome::Hit
        } else {
            CacheOutcome::Miss
        };
        // If the inner pipeline did not stream the captured buffers
        // (e.g. cached-hit replay, MSVC `/showIncludes`, error
        // surfaces), chunk-emit them here so the consumer always sees
        // the full output before `Done`.
        if !streaming_sink.streamed() {
            for chunk in response.stdout.chunks(64 * 1024) {
                if sink
                    .send(CompileChunk::Stdout(chunk.to_vec()))
                    .await
                    .is_err()
                {
                    return Ok(()); // consumer hung up — bail cleanly
                }
            }
            for chunk in response.stderr.chunks(64 * 1024) {
                if sink
                    .send(CompileChunk::Stderr(chunk.to_vec()))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
        let _ = sink
            .send(CompileChunk::Done {
                exit_code: response.exit_code,
                cached: response.cached,
                cache_outcome,
                compile_id,
            })
            .await;
        Ok(())
    }

    /// Return a daemon-compatible stats snapshot.
    pub async fn stats(&self) -> Result<ServiceStats> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EmbeddedError::ShutDown);
        }
        Ok(ServiceStats::from_snapshot(self.daemon.stats().await))
    }

    /// Flush pending embedded service state to disk.
    pub async fn flush(&self) -> Result<FlushReport> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(EmbeddedError::ShutDown);
        }
        Ok(FlushReport::from_report(self.daemon.flush().await))
    }

    /// Shut down the service and flush relevant persisted state.
    pub async fn shutdown(self, mode: ShutdownMode) -> Result<ShutdownReport> {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return Err(EmbeddedError::ShutDown);
        }
        let report = self.daemon.shutdown().await;
        Ok(ShutdownReport {
            mode,
            flushed: FlushReport::from_report(report),
        })
    }
}

impl ServiceStats {
    fn from_snapshot(snapshot: EmbeddedStatsSnapshot) -> Self {
        let status = snapshot.status;
        Self {
            cache_root: status.cache_dir,
            uptime_secs: status.uptime_secs,
            total_compilations: status.total_compilations,
            cache_hits: status.cache_hits,
            cache_misses: status.cache_misses,
            non_cacheable: status.non_cacheable,
            compile_errors: status.compile_errors,
            compile_errors_cached: status.compile_errors_cached,
            time_saved_ms: status.time_saved_ms,
            artifact_count: status.artifact_count,
            cache_size_bytes: status.cache_size_bytes,
            metadata_entries: status.metadata_entries,
            dep_graph_contexts: status.dep_graph_contexts,
            dep_graph_files: status.dep_graph_files,
            sessions_total: status.sessions_total,
            sessions_active: status.sessions_active,
            phase_profile: snapshot.phase_profile,
        }
    }
}

impl FlushReport {
    fn from_report(report: EmbeddedFlushReport) -> Self {
        Self {
            pending_writes_drained: report.pending_writes_drained,
            artifact_entries: report.artifact_entries,
            metadata_entries: report.metadata_entries,
        }
    }
}

fn embedded_endpoint(host: &HostIdentity) -> String {
    format!(
        "embedded:{}:{}:{}",
        sanitize_identity(&host.product),
        sanitize_identity(&host.instance_id),
        sanitize_identity(&host.workspace_id)
    )
}

fn sanitize_identity(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}
