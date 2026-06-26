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
//! `ZccacheConfig` does not yet accept a `tokio::runtime::Handle`
//! (`RuntimeHooks` is still `{ service_name: Option<String> }` as of
//! 1.12.11; the upstream architecture doc calls this out as a known
//! gap). The stop-gap is: [`SoldrZccacheService::start`] is
//! `async`, so it must be `.await`ed from inside the daemon's existing
//! tokio runtime — and `ZccacheService::start` internally spawns its
//! own background tasks via `tokio::spawn` from the ambient runtime,
//! which means those tasks land on the *same* runtime the daemon owns.
//! `console-subscriber` therefore sees the union of soldr + zccache
//! tasks. When upstream adds an explicit `RuntimeHooks::handle` field
//! we will bump the git pin and switch to the explicit form.
//!
//! ## Identity defaults
//!
//! - `product = "soldr"`
//! - `instance_id` — blake3 of (`SoldrPaths::root`, current exe path).
//!   Stable across daemon restarts on the same machine + same install,
//!   which is what `endpoint_for_identity` wants for cache continuity.
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

use blake3::Hasher;
use zccache::core::NormalizedPath;
use zccache::embedded::{
    AuditConfig, AuditContext, CacheOutcome, CompileChunk, CompileRequest as ZccacheCompileRequest,
    HostIdentity, RuntimeHooks, ServiceLimits, ShutdownMode, ZccacheConfig, ZccacheService,
};

use crate::core::SoldrPaths;
use crate::daemon::protocol::{CompileRequest, CompileResponseBody};

/// Soldr-side handle around a started [`ZccacheService`]. Cheap to
/// clone (the inner handle is `Arc`-shared).
#[derive(Clone)]
pub struct SoldrZccacheService {
    inner: Arc<ZccacheService>,
    identity: HostIdentity,
    cache_root: PathBuf,
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
    /// Issue #977 Phase 5 / #980 L1 — surfaced by [`SoldrZccacheService::compile`].
    /// Maps to a soldr-side `Response::Error` so the wrapper falls back
    /// to the legacy `zccache.exe` fork path.
    #[error("zccache embedded compile failed: {0}")]
    Compile(String),
    /// Issue #977 Phase 5 — the wrapper sent a `CompileRequest` with an
    /// empty args list; we have no rustc path to invoke.
    #[error("Compile request missing compiler path (args[0])")]
    EmptyArgs,
    #[error("io while preparing zccache cache root: {0}")]
    Io(#[from] std::io::Error),
}

impl SoldrZccacheService {
    /// Start the embedded service. Must be called from inside the
    /// daemon's tokio runtime (the function is `async` for exactly
    /// that reason). Creates the cache root directory if missing.
    pub async fn start(paths: &SoldrPaths) -> Result<Self, EmbeddedServiceError> {
        let cache_root = paths.cache.join("zccache");
        std::fs::create_dir_all(&cache_root)?;
        let identity = derive_identity(paths);

        let cfg = ZccacheConfig {
            host: identity.clone(),
            cache_root: cache_root.clone().into(),
            audit: AuditConfig::default(),
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks {
                service_name: Some("soldr-daemon".into()),
            },
        };

        let svc = ZccacheService::start(cfg)
            .await
            .map_err(|e| EmbeddedServiceError::Start(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(svc),
            identity,
            cache_root,
        })
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
        let zresp = self
            .inner
            .compile(zreq)
            .await
            .map_err(|e| EmbeddedServiceError::Compile(e.to_string()))?;
        Ok(CompileResponseBody {
            exit_code: zresp.exit_code,
            stdout: zresp.stdout,
            stderr: zresp.stderr,
            cached: zresp.cached,
            cache_outcome: encode_cache_outcome(zresp.cache_outcome),
        })
    }

    /// Streaming variant of [`Self::compile`] (Phase 5b2, soldr#983).
    /// Returns the borrowed receiver half of the embedded service's
    /// streaming channel. The caller (daemon `dispatch_compile_streaming`)
    /// consumes [`StreamingCompileEvent`]s and forwards them as wire
    /// frames to the wrapper. The driver future MUST be polled
    /// concurrently with the receiver — `tokio::join!` on the two
    /// sides is the canonical pattern; see the daemon server for the
    /// production use.
    ///
    /// The mpsc capacity is [`zccache::embedded::DEFAULT_STREAMING_CAPACITY`]
    /// (64 chunks × 64 KiB = ~4 MiB max in-flight). When rustc
    /// produces faster than the consumer can ship frames the channel
    /// applies backpressure to the rustc-pipe pumping tasks inside
    /// the embedded service — the daemon never accumulates an
    /// unbounded buffer.
    pub fn compile_streaming(
        &self,
        req: CompileRequest,
    ) -> Result<
        (
            tokio::sync::mpsc::Receiver<StreamingCompileEvent>,
            impl std::future::Future<Output = Result<(), EmbeddedServiceError>> + Send,
        ),
        EmbeddedServiceError,
    > {
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
        let (inner_tx, mut inner_rx) =
            tokio::sync::mpsc::channel::<CompileChunk>(zccache::embedded::DEFAULT_STREAMING_CAPACITY);
        let (out_tx, out_rx) =
            tokio::sync::mpsc::channel::<StreamingCompileEvent>(zccache::embedded::DEFAULT_STREAMING_CAPACITY);
        let svc = Arc::clone(&self.inner);
        let driver = async move {
            // Two-task fan-out: one task drives the embedded compile
            // (which sends chunks into `inner_tx`); the current task
            // drains them into the soldr-side `out_tx`. Without the
            // concurrent split the producer would deadlock the moment
            // the inner channel filled up.
            let stream_task = tokio::spawn(async move {
                svc.compile_streaming(zreq, inner_tx)
                    .await
                    .map_err(|e| EmbeddedServiceError::Compile(e.to_string()))
            });
            while let Some(chunk) = inner_rx.recv().await {
                let out_event = match chunk {
                    CompileChunk::Stdout(bytes) => StreamingCompileEvent::Stdout(bytes),
                    CompileChunk::Stderr(bytes) => StreamingCompileEvent::Stderr(bytes),
                    CompileChunk::Done {
                        exit_code,
                        cached,
                        cache_outcome,
                        compile_id,
                    } => StreamingCompileEvent::Done {
                        exit_code,
                        cached,
                        cache_outcome: encode_cache_outcome(cache_outcome),
                        compile_id,
                    },
                };
                if out_tx.send(out_event).await.is_err() {
                    break;
                }
            }
            stream_task
                .await
                .map_err(|e| EmbeddedServiceError::Compile(format!("compile task join failed: {e}")))?
        };
        Ok((out_rx, driver))
    }

    /// Drain pending writes — called from the `Request::BuildSessionEnd`
    /// arm before the session aggregate write hits redb.
    pub async fn flush(&self) -> Result<(), EmbeddedServiceError> {
        self.inner
            .flush()
            .await
            .map(|_| ())
            .map_err(|e| EmbeddedServiceError::Flush(e.to_string()))
    }

    /// Graceful shutdown — called from the daemon's normal exit path
    /// after the accept loop has been aborted.
    pub async fn shutdown(self, mode: ShutdownMode) -> Result<(), EmbeddedServiceError> {
        // `ZccacheService::shutdown` takes `self` by value. We hold
        // an `Arc<ZccacheService>`; if we are the last reference we
        // can `Arc::try_unwrap`, otherwise the best we can do is
        // call `flush` and let `Drop` clean up. In Phase 1 + 2 the
        // service is only stored on the daemon's `State` which is
        // dropped after `run_async` returns, so the unwrap path is
        // the steady-state branch.
        match Arc::try_unwrap(self.inner) {
            Ok(svc) => svc
                .shutdown(mode)
                .await
                .map(|_| ())
                .map_err(|e| EmbeddedServiceError::Shutdown(e.to_string())),
            Err(arc) => {
                // Some other clone still holds the service; do a flush
                // and rely on Drop. Better than refusing to exit.
                let _ = arc.flush().await;
                Ok(())
            }
        }
    }
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
    let mut hasher = Hasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    hex::encode(&hasher.finalize().as_bytes()[..16])
}

/// Soldr-side mirror of [`zccache::embedded::CompileChunk`] (Phase 5b2,
/// soldr#983). The `cache_outcome` field is pre-encoded to the same i32
/// shape the [`CompileResponseBody`] wire format uses (1=Hit, 2=Miss,
/// 3=Error) so the daemon dispatcher can fill the `CompileDone` frame
/// without re-importing the zccache `CacheOutcome` enum.
#[derive(Debug, Clone)]
pub enum StreamingCompileEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Done {
        exit_code: i32,
        cached: bool,
        cache_outcome: i32,
        compile_id: String,
    },
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

fn derive_identity(paths: &SoldrPaths) -> HostIdentity {
    let mut hasher = Hasher::new();
    hasher.update(paths.root.as_os_str().to_string_lossy().as_bytes());
    if let Ok(exe) = std::env::current_exe() {
        hasher.update(exe.as_os_str().to_string_lossy().as_bytes());
    }
    let hash = hasher.finalize();
    let id_hex = hex::encode(&hash.as_bytes()[..16]);
    HostIdentity {
        product: "soldr".to_string(),
        instance_id: id_hex.clone(),
        workspace_id: id_hex,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(encode_cache_outcome_matches_wire_int_encoding, {
        // Phase 5b2 (soldr#983): the streaming bridge must encode
        // `CacheOutcome` to the same int the buffered wire format used.
        // Diverging here would silently break the wrapper's
        // `cache_outcome` reporting.
        assert_eq!(encode_cache_outcome(CacheOutcome::Hit), 1);
        assert_eq!(encode_cache_outcome(CacheOutcome::Miss), 2);
        assert_eq!(encode_cache_outcome(CacheOutcome::Error), 3);
    });

    crate::timed_test!(streaming_compile_event_carries_done_metadata, {
        // Phase 5b2: smoke-check the soldr-side mirror of
        // `CompileChunk`. The bridge in `dispatch_compile_streaming`
        // depends on the field shape; a future structural change here
        // should fail this test until the dispatcher is updated.
        let ev = StreamingCompileEvent::Done {
            exit_code: 0,
            cached: true,
            cache_outcome: 1,
            compile_id: "abc".into(),
        };
        match ev {
            StreamingCompileEvent::Done {
                exit_code,
                cached,
                cache_outcome,
                compile_id,
            } => {
                assert_eq!(exit_code, 0);
                assert!(cached);
                assert_eq!(cache_outcome, 1);
                assert_eq!(compile_id, "abc");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    });
}
