//! Async wrappers around the blocking [`crate::daemon::db`] entry points
//! (soldr#2224).
//!
//! A state-store operation is still synchronous filesystem work: SQLite
//! WAL removed redb's exclusive-open contention, but a contended write
//! can wait out the busy timeout and every commit is real I/O. Calling
//! the sync `db::*` functions straight from an IPC handler parks a tokio
//! **worker** thread for that duration, starving every other connection
//! the runtime is serving.
//!
//! `event_batcher` already learned this (soldr#1669) and wraps its
//! `write_batch` in `spawn_blocking`; the request handlers in `server.rs`
//! did not. Every state-DB touch on an async path goes through this module
//! so the blocking pool absorbs the wait instead of the worker pool.
//!
//! The guard test `no_state_db_opens_on_tokio_workers` below fails the
//! build if a handler in `server.rs` or `build_session_ops.rs` reaches for
//! the sync module directly.

use crate::cache_lib::target_registry::RegistryError;
use crate::daemon::db::{self, Event};
use crate::daemon::protocol::BuildRecord;
use std::path::{Path, PathBuf};

/// A panic or cancellation inside the blocking pool is an I/O-shaped
/// failure from the caller's perspective: the operation did not happen and
/// there is nothing to retry inline.
fn join_err(error: tokio::task::JoinError) -> RegistryError {
    RegistryError::Io(std::io::Error::other(format!(
        "state-store blocking task failed: {error}"
    )))
}

async fn blocking<T, F>(work: F) -> Result<T, RegistryError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, RegistryError> + Send + 'static,
{
    tokio::task::spawn_blocking(work).await.map_err(join_err)?
}

pub async fn ensure_initialized(db_path: &Path) -> Result<(), RegistryError> {
    let path: PathBuf = db_path.to_path_buf();
    blocking(move || db::ensure_initialized(&path)).await
}

pub async fn get_build(
    db_path: &Path,
    session_id: u64,
) -> Result<Option<BuildRecord>, RegistryError> {
    let path: PathBuf = db_path.to_path_buf();
    blocking(move || db::get_build(&path, session_id)).await
}

pub async fn upsert_build(db_path: &Path, record: BuildRecord) -> Result<(), RegistryError> {
    let path: PathBuf = db_path.to_path_buf();
    blocking(move || db::upsert_build(&path, &record)).await
}

pub async fn append_event(db_path: &Path, event: Event) -> Result<(), RegistryError> {
    let path: PathBuf = db_path.to_path_buf();
    blocking(move || db::append_event(&path, &event)).await
}

pub async fn aggregate_session(
    db_path: &Path,
    session_id: u64,
) -> Result<(u32, Option<u64>, Option<String>), RegistryError> {
    let path: PathBuf = db_path.to_path_buf();
    blocking(move || db::aggregate_session(&path, session_id)).await
}

pub async fn finalize_build(
    db_path: &Path,
    session_id: u64,
    exit_code: i32,
    ended_at_ms: i64,
    aggregate: (u32, Option<u64>, Option<String>),
) -> Result<BuildRecord, RegistryError> {
    let path: PathBuf = db_path.to_path_buf();
    blocking(move || db::finalize_build(&path, session_id, exit_code, ended_at_ms, aggregate)).await
}

pub async fn list_builds(
    db_path: &Path,
    limit: u32,
    since_ms: Option<i64>,
) -> Result<Vec<BuildRecord>, RegistryError> {
    let path: PathBuf = db_path.to_path_buf();
    blocking(move || db::list_builds(&path, limit, since_ms)).await
}

pub async fn list_slow_builds(
    db_path: &Path,
    threshold_ms: u64,
    limit: u32,
) -> Result<Vec<BuildRecord>, RegistryError> {
    let path: PathBuf = db_path.to_path_buf();
    blocking(move || db::list_slow_builds(&path, threshold_ms, limit)).await
}

pub async fn list_events_for_session(
    db_path: &Path,
    session_id: u64,
) -> Result<Vec<Event>, RegistryError> {
    let path: PathBuf = db_path.to_path_buf();
    blocking(move || db::list_events_for_session(&path, session_id)).await
}

/// Run an arbitrary closure against a single caller-scoped state-DB handle
/// on the blocking pool.
///
/// This is the escape hatch for multi-step handler work (read → mutate →
/// write) that must not pay several opens: the closure receives one
/// connection and uses the `_in` variants throughout.
pub async fn with_handle<T, F>(db_path: &Path, work: F) -> Result<T, RegistryError>
where
    T: Send + 'static,
    F: FnOnce(&rusqlite::Connection) -> Result<T, RegistryError> + Send + 'static,
{
    let path: PathBuf = db_path.to_path_buf();
    blocking(move || {
        let handle = db::open_handle(&path)?;
        work(&handle)
    })
    .await
}

#[cfg(test)]
mod tests {
    /// The sync `db` entry points that open `state.sqlite3` themselves. Each
    /// takes a `&Path`; the `*_in` variants take an already-open handle and
    /// are therefore fine to call from inside a `spawn_blocking` closure.
    const PATH_TAKING_OPENERS: &[&str] = &[
        "db::get_build(",
        "db::upsert_build(",
        "db::append_event(",
        "db::aggregate_session(",
        "db::finalize_build(",
        "db::list_builds(",
        "db::list_slow_builds(",
        "db::list_events_for_session(",
        "db::prune_events_older_than(",
        "db::mark_archives_unavailable(",
        "db::clear_legacy_archive_paths(",
    ];

    /// Drop every `#[cfg(test)]` item. rustfmt puts the closing brace of a
    /// top-level item in column 0, which makes the end unambiguous.
    fn strip_test_items(source: &str) -> String {
        let mut kept = String::new();
        let mut skipping = false;
        for line in source.lines() {
            if !skipping && line.trim_start() == "#[cfg(test)]" && !line.starts_with(' ') {
                skipping = true;
                continue;
            }
            if skipping {
                if line == "}" {
                    skipping = false;
                }
                continue;
            }
            kept.push_str(line);
            kept.push('\n');
        }
        kept
    }

    // soldr#2224 acceptance: no `state.sqlite3` open runs on a tokio worker.
    //
    // The IPC handlers are `async`, so a sync `db::*` call in one runs on
    // the worker that is polling the connection. Those calls are not
    // "short transactions": the *open* alone waits up to 5 s for another
    // process to release redb's file lock, which parks the worker and
    // stalls every other connection the runtime is serving — the very
    // thing soldr#1669 moved the event batcher's writes to avoid.
    //
    // Textual, because the property is about which module a handler
    // reaches for, and that is exactly what the reviewer would check.
    #[test]
    fn no_state_db_opens_on_tokio_workers() {
        let handler_sources = [
            (
                "server handlers",
                concat!(
                    include_str!("server_runtime.rs"),
                    include_str!("server_dispatch.rs"),
                    include_str!("server_compile.rs")
                ),
            ),
            ("build_session_ops.rs", include_str!("build_session_ops.rs")),
        ];
        for (name, source) in handler_sources {
            let code_only = strip_test_items(source);
            // A scan over accidentally-empty input passes vacuously, which
            // is the one way this guard could quietly stop guarding.
            assert!(
                code_only.contains("db_async::"),
                "{name}: the test-stripped source lost its handler code, so the \
                 scan below would prove nothing"
            );
            for (number, line) in code_only.lines().enumerate() {
                let code = line.split("//").next().unwrap_or(line);
                for opener in PATH_TAKING_OPENERS {
                    assert!(
                        !code.contains(opener),
                        "{name} calls `{opener}` directly (line {} of the test-stripped \
                         source). Handlers are async, so this opens state.sqlite3 on a tokio \
                         worker and can park it for the full 5 s contention budget. Use \
                         `db_async::*` (or `db_async::with_handle` for multi-step work) \
                         instead (soldr#2224).\n  {line}",
                        number + 1,
                    );
                }
            }
        }
    }

    #[test]
    fn strip_test_items_removes_whole_cfg_test_blocks() {
        let source =
            "fn keep() {}\n#[cfg(test)]\nmod tests {\n    fn drop_me() {}\n}\nfn also_keep() {}\n";
        let stripped = strip_test_items(source);
        assert!(stripped.contains("fn keep()"));
        assert!(stripped.contains("fn also_keep()"));
        assert!(!stripped.contains("drop_me"));
    }
}
