//! L4 (issue soldr#980): coalesce per-compile redb transactions into a
//! background flusher.
//!
//! ## Problem
//!
//! Pre-L4, every `Request::RecordCompile` IPC call landed in
//! [`crate::daemon::db::append_event`], which opens the shared
//! `state.redb`, runs a write txn to bump `next_event_id` (fsync #1) and
//! a second write txn to insert the event row (fsync #2). The cold
//! profile recorded 171 compile misses ≈ ~342 fsyncs ≈ ~3.4 s of
//! pure-sync wait on Windows/WSL2 at ~10 ms/fsync. None of those events
//! is critical-path for the build itself; they are diagnostics consumed
//! by `soldr daemon status` and `soldr daemon ls-builds` after the build
//! finishes.
//!
//! ## Design
//!
//! [`EventBatcher::start`] spawns one background tokio task that owns the
//! redb writer. The hot path ([`EventBatcher::record`]) just pushes a
//! [`crate::daemon::db::Event`] onto a bounded mpsc channel — a few
//! microseconds at most. The drain task batches up to
//! [`MAX_BATCH_ROWS`] rows OR up to [`MAX_BATCH_LATENCY`] of wall time
//! before opening a single redb write txn that allocates a contiguous
//! range of event IDs and inserts every staged row in one fsync.
//!
//! On `BuildSessionEnd` and on daemon shutdown the server sends an
//! explicit [`BatcherCmd::Flush`] with a oneshot acknowledgement so the
//! caller can confirm every staged row landed before the session is
//! treated as finalized (the post-session `aggregate_session` pass
//! relies on the events being visible).
//!
//! ## Race window
//!
//! A reader querying redb between two flushes (e.g. a `soldr daemon
//! status` racing with an in-progress build) sees a snapshot that does
//! NOT include rows still buffered in memory. This is acceptable for the
//! per-compile diagnostic path — at worst the operator sees one or two
//! fewer events than the daemon will eventually persist; the next
//! interval tick / batch-fullness trip will flush the rest. No data is
//! lost: shutdown and `BuildSessionEnd` both force a flush before they
//! complete, and the `Drop` semantics of the mpsc receiver mean even an
//! aborted task drains the channel one more time before exiting.

use crate::cache_lib::redb_lock::{state_db_open_lock, StateDbHandle};
use crate::daemon::db::Event;
use crate::daemon::wire::{self, prost_tagged_bytes};
use redb::{Database, ReadableTable, TableDefinition};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Mirrors the table definitions in [`crate::daemon::db`]. Kept as
/// private constants here so the batcher does not have to take a write
/// txn through `daemon::db` (which would re-allocate IDs one at a time
/// and defeat the entire point of L4).
const EVENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("daemon_events");
const META: TableDefinition<&str, u64> = TableDefinition::new("daemon_meta");
const META_NEXT_EVENT_ID: &str = "next_event_id";

/// Flush whenever the staged batch reaches this many rows. Picked to
/// keep one write txn small enough to commit in a single fsync without
/// stalling the daemon's IPC accept loop.
const MAX_BATCH_ROWS: usize = 64;

/// Flush at least this often, even when fewer than [`MAX_BATCH_ROWS`]
/// rows have accumulated. Keeps `soldr daemon status` snapshots fresh
/// during slow builds.
const MAX_BATCH_LATENCY: Duration = Duration::from_millis(100);

/// Bounded backpressure on the producer side. 4096 in-flight events is
/// far more than any realistic build session generates between flushes;
/// the channel acts mostly as a queue, not a buffer.
const CHANNEL_CAPACITY: usize = 4096;

/// Commands accepted by the drain task.
enum BatcherCmd {
    /// Stage a new event for the next batch.
    Insert(Event),
    /// Flush whatever is buffered NOW; reply via the oneshot after the
    /// commit succeeds (or after the failed write is logged).
    Flush(oneshot::Sender<()>),
    /// Drain and exit. Reply via the oneshot to acknowledge.
    Shutdown(oneshot::Sender<()>),
}

/// Cheap-to-clone handle. Each request handler in
/// [`crate::daemon::server`] holds one; the inner `mpsc::Sender` is
/// itself a clone-friendly handle so the wrapper just forwards.
#[derive(Clone)]
pub struct EventBatcher {
    tx: mpsc::Sender<BatcherCmd>,
}

impl EventBatcher {
    /// Start the background drain task. The returned handle can be
    /// freely cloned across request handlers and outlives every
    /// in-flight IPC call; the task itself exits cleanly when
    /// [`shutdown`] is called or when the last [`EventBatcher`] is
    /// dropped (the receiver returns `None`, which the loop treats as a
    /// final flush + exit).
    pub fn start(db_path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel::<BatcherCmd>(CHANNEL_CAPACITY);
        tokio::spawn(drain_loop(db_path, rx));
        Self { tx }
    }

    /// Hot path. Non-blocking on the common case (channel has free
    /// capacity); the await is microseconds. On a full channel the send
    /// awaits until the drain task makes room, which is the desired
    /// backpressure shape — we never want to silently drop diagnostic
    /// events.
    pub async fn record(&self, event: Event) {
        if self.tx.send(BatcherCmd::Insert(event)).await.is_err() {
            tracing::debug!("event_batcher: drain task gone; event dropped");
        }
    }

    /// Force a flush and wait for the drain task to acknowledge. Called
    /// from `Request::BuildSessionEnd` before the session-level
    /// aggregation runs, so the aggregator sees every event recorded
    /// during the session.
    pub async fn flush(&self) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(BatcherCmd::Flush(ack_tx)).await.is_err() {
            tracing::debug!("event_batcher: drain task gone; flush is a no-op");
            return;
        }
        let _ = ack_rx.await;
    }

    /// Final flush + drain task exit. Called from the daemon shutdown
    /// path before the redb file lock is released.
    pub async fn shutdown(&self) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(BatcherCmd::Shutdown(ack_tx)).await.is_err() {
            return;
        }
        let _ = ack_rx.await;
    }
}

/// Drain-loop entry point. Owns the in-memory staging buffer and the
/// 100 ms heartbeat that flushes a partial batch.
async fn drain_loop(db_path: PathBuf, mut rx: mpsc::Receiver<BatcherCmd>) {
    let mut buf: Vec<Event> = Vec::with_capacity(MAX_BATCH_ROWS);
    let mut interval = tokio::time::interval(MAX_BATCH_LATENCY);
    // Skip the first tick — `interval` fires immediately on the first
    // poll otherwise, which would flush an empty buffer for no reason.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(BatcherCmd::Insert(event)) => {
                    buf.push(event);
                    if buf.len() >= MAX_BATCH_ROWS {
                        flush_batch(&db_path, &mut buf);
                    }
                }
                Some(BatcherCmd::Flush(ack)) => {
                    flush_batch(&db_path, &mut buf);
                    let _ = ack.send(());
                }
                Some(BatcherCmd::Shutdown(ack)) => {
                    flush_batch(&db_path, &mut buf);
                    let _ = ack.send(());
                    return;
                }
                None => {
                    // Last sender dropped. Drain whatever is left and
                    // exit — no one is around to receive an ack.
                    flush_batch(&db_path, &mut buf);
                    return;
                }
            },
            _ = interval.tick() => {
                if !buf.is_empty() {
                    flush_batch(&db_path, &mut buf);
                }
            }
        }
    }
}

/// One redb write txn that allocates a contiguous range of event IDs
/// and inserts every staged row. Empty buffer is a no-op. Errors are
/// logged at `debug` and swallowed: the diagnostic events are
/// best-effort and we never want a failed flush to take down the
/// daemon.
fn flush_batch(db_path: &Path, buf: &mut Vec<Event>) {
    if buf.is_empty() {
        return;
    }
    let count = buf.len();
    tracing::debug!("event_batcher: flushing {count} pending event rows");
    if let Err(err) = write_batch(db_path, buf) {
        tracing::debug!("event_batcher: flush failed: {err}");
    }
    buf.clear();
}

/// Open `state.redb` under the shared `state_db_open_lock`, allocate
/// `count` event IDs in a single META insert, and insert every row in
/// the same write txn. One fsync covers the entire batch.
fn write_batch(db_path: &Path, buf: &[Event]) -> std::io::Result<()> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let guard = state_db_open_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let db = Database::builder()
        .create(db_path)
        .map_err(|e| std::io::Error::other(format!("redb open: {e}")))?;
    // Keep the handle in scope for the entire txn. `StateDbHandle`'s
    // declared field order (db before guard) guarantees the redb file
    // lock is released before the process-wide mutex is, so a follow-up
    // opener never races us.
    let handle = StateDbHandle::new(db, guard);
    let txn = handle
        .begin_write()
        .map_err(|e| std::io::Error::other(format!("redb begin_write: {e}")))?;
    {
        // Allocate `count` consecutive IDs in one META write.
        let start_id: u64;
        {
            let mut meta = txn
                .open_table(META)
                .map_err(|e| std::io::Error::other(format!("redb open META: {e}")))?;
            let current = meta
                .get(META_NEXT_EVENT_ID)
                .map_err(|e| std::io::Error::other(format!("redb meta get: {e}")))?
                .map(|v| v.value())
                .unwrap_or(1);
            start_id = current;
            let next = current.saturating_add(buf.len() as u64);
            meta.insert(META_NEXT_EVENT_ID, &next)
                .map_err(|e| std::io::Error::other(format!("redb meta insert: {e}")))?;
        }
        let mut events = txn
            .open_table(EVENTS)
            .map_err(|e| std::io::Error::other(format!("redb open EVENTS: {e}")))?;
        for (i, event) in buf.iter().enumerate() {
            let id = start_id.saturating_add(i as u64);
            let bytes = prost_tagged_bytes(&wire::event_to_wire(event));
            events
                .insert(id, bytes.as_slice())
                .map_err(|e| std::io::Error::other(format!("redb events insert: {e}")))?;
        }
    }
    txn.commit()
        .map_err(|e| std::io::Error::other(format!("redb commit: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::db::{aggregate_session, list_builds, EventKind};
    use tempfile::TempDir;
    use tokio::runtime::Builder;

    fn sample_event(session: u64, krate: &str, dur_us: Option<u64>) -> Event {
        Event {
            ts_ms: 1_700_000_000_000,
            session_id: Some(session),
            kind: if dur_us.is_some() {
                EventKind::CompileEnd
            } else {
                EventKind::CompileStart
            },
            crate_name: Some(krate.into()),
            duration_us: dur_us,
            target_dir: Some("/t".into()),
            exit_code: None,
        }
    }

    crate::timed_test!(batcher_persists_rows_after_flush, {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("state.redb");
            crate::daemon::db::ensure_initialized(&path).expect("init");
            let batcher = EventBatcher::start(path.clone());
            for i in 0..5 {
                batcher
                    .record(sample_event(99, &format!("c{i}"), Some(1000 + i)))
                    .await;
            }
            batcher.flush().await;
            let (count, slowest, name) = aggregate_session(&path, 99).expect("agg");
            assert_eq!(count, 5);
            assert_eq!(slowest, Some(1004));
            assert_eq!(name.as_deref(), Some("c4"));
            batcher.shutdown().await;
            // Build list table is untouched by the batcher.
            assert!(list_builds(&path, 10, None).expect("list").is_empty());
        });
    });

    crate::timed_test!(batcher_batches_at_capacity_boundary, {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("state.redb");
            crate::daemon::db::ensure_initialized(&path).expect("init");
            let batcher = EventBatcher::start(path.clone());
            // Push more than MAX_BATCH_ROWS to force an auto-flush before
            // we explicitly call flush().
            for i in 0..(MAX_BATCH_ROWS + 7) {
                batcher
                    .record(sample_event(7, &format!("c{i}"), Some(i as u64)))
                    .await;
            }
            batcher.flush().await;
            let (count, _, _) = aggregate_session(&path, 7).expect("agg");
            assert_eq!(count as usize, MAX_BATCH_ROWS + 7);
            batcher.shutdown().await;
        });
    });
}
