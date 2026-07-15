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
use std::time::{SystemTime, UNIX_EPOCH};

use running_process::broker::backend_handle::DaemonProcess;
use zccache::audit::AuditMode;
use zccache::core::NormalizedPath;
use zccache::embedded::{
    AuditConfig, AuditContext, CacheOutcome, CompileRequest as ZccacheCompileRequest, HostIdentity,
    RuntimeHooks, ServiceLimits, ShutdownMode, ZccacheConfig, ZccacheService,
};
use zccache::hash::StreamHasher;

use crate::core::SoldrPaths;
use crate::daemon::protocol::{
    CompileRequest, CompileResponseBody, CompileStatsInfo, StagedProfileInfo,
};

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
    #[error("zccache embedded stats failed: {0}")]
    Stats(String),
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
        migrate_legacy_cache_root(paths, daemon_identity, &cache_root)?;
        std::fs::create_dir_all(&cache_root)?;

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
        let cfg = ZccacheConfig {
            host: identity.clone(),
            cache_root: cache_root.clone().into(),
            audit,
            limits: ServiceLimits::default(),
            runtime: RuntimeHooks {
                service_name: Some("soldr-daemon".into()),
                // zccache#922 — leave None to keep today's
                // implicit-ambient-runtime behavior. We can plumb the
                // soldr-daemon's tokio handle through here in a
                // follow-up once we have a reason (tokio-console
                // attach unity, explicit handle-based shutdown).
                handle: None,
            },
            // zccache#923 — None preserves the prior behavior where
            // only `shutdown(ShutdownMode::Force)` aborts in-flight
            // work. soldr's daemon already has a Notify-based
            // shutdown path that race-completes the same scenarios.
            cancellation: None,
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

    /// Drain pending writes — called from the `Request::FlushCaches`
    /// arm (`soldr save` / `soldr cache flush`) so the on-disk cache
    /// tree is complete before archiving.
    pub async fn flush(&self) -> Result<(), EmbeddedServiceError> {
        self.inner
            .flush()
            .await
            .map(|_| ())
            .map_err(|e| EmbeddedServiceError::Flush(e.to_string()))
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

    let exact_legacy = private_zccache_cache_root(
        paths,
        &derive_legacy_identity(paths, &daemon_identity.exe_path),
    );
    if exact_legacy.is_dir() {
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
        if entry.file_type()?.is_dir() && is_legacy_identity_name(&entry.file_name()) {
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
    let mut newest = std::fs::metadata(root)?.modified().unwrap_or(UNIX_EPOCH);
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let modified = entry.metadata()?.modified().unwrap_or(UNIX_EPOCH);
            newest = newest.max(modified);
            if file_type.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(newest)
}

fn private_zccache_cache_root(paths: &SoldrPaths, identity: &HostIdentity) -> std::path::PathBuf {
    paths
        .cache
        .join("zccache")
        .join("daemon-state")
        .join(&identity.instance_id)
}

#[cfg(test)]
mod private_root_tests {
    use super::*;

    #[derive(Debug)]
    struct CompilerProbeOutput {
        success: bool,
        exit_code: Option<i32>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }

    impl From<std::process::Output> for CompilerProbeOutput {
        fn from(output: std::process::Output) -> Self {
            Self {
                success: output.status.success(),
                exit_code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
            }
        }
    }

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
        let output = std::process::Command::new(path)
            .arg("-vV")
            .output()
            .map(CompilerProbeOutput::from);
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
        let tied = UNIX_EPOCH + std::time::Duration::from_secs(7);
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
        let request = || CompileRequest {
            args: rustc_args.clone(),
            cwd: project.display().to_string(),
            env: std::env::vars().collect(),
            stdin: Vec::new(),
            lifecycle: None,
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
