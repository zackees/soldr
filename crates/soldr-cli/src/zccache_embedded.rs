//! Embedded zccache service wrapper (issue #977 Phase 1 + 2).
//!
//! This module is the **only** soldr-side import site for the
//! `zccache::embedded::*` API. Everything else in soldr talks to
//! [`SoldrZccacheService`] so the upstream-API blast radius is bounded
//! to one file. The whole module is compiled only when the `embedded`
//! Cargo feature is on; the daemon resolves the backend choice at boot
//! and stores either `CompileBackend::Embedded(_)` or
//! `CompileBackend::Wrapped(_)` on its `State`.
//!
//! ## Design constraint: daemon-only
//!
//! Per issue #977 (and the user's explicit instruction in the goal
//! prompt), the embedded `ZccacheService` lives **inside the long-lived
//! soldr-daemon process and nowhere else**. Transient rustc-wrapper
//! invocations (which live for the lifetime of a single rustc command)
//! must not pay `ZccacheService::start` cost — that would defeat the
//! whole point of "one process, one tokio runtime, one console-subscriber
//! pane". Wrappers continue talking to the daemon over the existing IPC
//! frame; Phase 5 wires a `Request::Compile` verb that ferries the
//! compile to the embedded backend without a `Command::new("zccache")`
//! fork. In Phase 1 + 2 the service simply starts and stops with the
//! daemon — proving the runtime-sharing wiring works end to end without
//! changing the on-the-wire protocol.
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
//! Filled per compile in Phase 5 once the wrapper's IPC `Request::Compile`
//! verb lands. Phase 1 + 2 intentionally does NOT instantiate
//! `AuditContext` — the embedded service starts, holds state, flushes,
//! and stops, but no compiles travel through it. Keeping the audit-side
//! surface minimal here avoids a churn target while upstream's audit
//! schema is still settling (see issue #977 open questions).

#![cfg(feature = "embedded")]

use std::path::PathBuf;
use std::sync::Arc;

use blake3::Hasher;
use zccache::embedded::{
    AuditConfig, HostIdentity, RuntimeHooks, ServiceLimits, ShutdownMode, ZccacheConfig,
    ZccacheService,
};

use crate::core::SoldrPaths;

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
