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
//! treated as finalized.
//!
//! soldr#1536: the batcher additionally maintains an in-memory
//! [`SessionAggregate`] per build session, updated synchronously in
//! [`EventBatcher::record`]. `BuildSessionEnd` consumes it via
//! [`EventBatcher::take_session_aggregate`] so finalization is
//! proportional to the current session instead of scanning (and
//! prost-decoding) the entire retained `daemon_events` table; the
//! historical `aggregate_session` scan remains only as the fallback for
//! sessions this daemon process did not observe from their start.
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

use crate::cache_lib::redb_lock::open_state_db;
use crate::daemon::db::{Event, EventKind};
use crate::daemon::wire::{self, prost_tagged_bytes};
use redb::{ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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

/// Upper bound on concurrently tracked per-session aggregates
/// (soldr#1536). Realistic hosts run a handful of parallel builds; the
/// cap only exists so sessions that never receive a `BuildSessionEnd`
/// (wrapper killed mid-build) cannot grow the map without bound. On
/// overflow the entry with the oldest last-observed timestamp is
/// evicted; its finalization then falls back to the redb scan.
const MAX_TRACKED_SESSIONS: usize = 128;

/// Daemon-owned per-session rollup (soldr#1536). Mirrors the semantics
/// of [`crate::daemon::db::aggregate_session`] — crate count is the
/// number of `CompileEnd` events (falling back to `CompileStart` when
/// no compile completed) and the slowest crate is tracked across every
/// event that carries a `duration_us` — but is maintained incrementally
/// as events flow through [`EventBatcher::record`], so build-session
/// finalization no longer has to decode the entire retained event
/// table.
#[derive(Debug, Clone, Default)]
pub struct SessionAggregate {
    start_count: u32,
    end_count: u32,
    slowest_us: Option<u64>,
    slowest_name: Option<String>,
    /// True only when the aggregate observed the session from its
    /// `SessionStart` event. A session first seen via a compile event
    /// was started before this daemon process existed (daemon restart /
    /// late auto-start mid-build), so earlier events may already sit in
    /// redb — the aggregate is incomplete and finalization must fall
    /// back to the historical scan.
    complete: bool,
    /// Timestamp of the most recent observed event; used only for
    /// oldest-first eviction when [`MAX_TRACKED_SESSIONS`] overflows.
    last_seen_ms: i64,
}

impl SessionAggregate {
    fn observe(&mut self, event: &Event) {
        self.last_seen_ms = self.last_seen_ms.max(event.ts_ms);
        match event.kind {
            EventKind::CompileStart => self.start_count = self.start_count.saturating_add(1),
            EventKind::CompileEnd => self.end_count = self.end_count.saturating_add(1),
            EventKind::SessionStart | EventKind::SessionEnd => {}
        }
        if let Some(d) = event.duration_us {
            if d > self.slowest_us.unwrap_or(0) {
                self.slowest_us = Some(d);
                self.slowest_name = event.crate_name.clone();
            }
        }
    }

    /// Collapse into the `(crate_count, slowest_us, slowest_name)` tuple
    /// [`crate::daemon::db::aggregate_session`] returns.
    pub fn finalize(self) -> (u32, Option<u64>, Option<String>) {
        let count = if self.end_count > 0 {
            self.end_count
        } else {
            self.start_count
        };
        (count, self.slowest_us, self.slowest_name)
    }
}

type SessionAggregates = Arc<Mutex<HashMap<u64, SessionAggregate>>>;

/// Commands accepted by the drain task.
enum BatcherCmd {
    /// Stage a new event for the next batch.
    Insert(Event),
    /// Flush whatever is buffered NOW; reply via the oneshot after the
    /// commit succeeds (or after the failed write is logged).
    Flush(oneshot::Sender<Result<(), String>>),
    /// Drain and exit. Reply via the oneshot to acknowledge.
    Shutdown(oneshot::Sender<Result<(), String>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventBatcherError(pub String);

impl std::fmt::Display for EventBatcherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EventBatcherError {}

/// Cheap-to-clone handle. Each request handler in
/// [`crate::daemon::server`] holds one; the inner `mpsc::Sender` is
/// itself a clone-friendly handle so the wrapper just forwards.
#[derive(Clone)]
pub struct EventBatcher {
    tx: mpsc::Sender<BatcherCmd>,
    /// soldr#1536: incrementally-maintained per-session rollups, updated
    /// synchronously in [`record`](Self::record) before the event is
    /// staged for persistence. `Request::BuildSessionEnd` consumes an
    /// entry via [`take_session_aggregate`](Self::take_session_aggregate)
    /// instead of scanning the whole `daemon_events` table.
    aggregates: SessionAggregates,
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
        Self {
            tx,
            aggregates: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Hot path. Non-blocking on the common case (channel has free
    /// capacity); the await is microseconds. On a full channel the send
    /// awaits until the drain task makes room, which is the desired
    /// backpressure shape — we never want to silently drop diagnostic
    /// events.
    pub async fn record(&self, event: Event) -> Result<(), EventBatcherError> {
        self.observe_for_aggregate(&event);
        if self.tx.send(BatcherCmd::Insert(event)).await.is_err() {
            return Err(EventBatcherError("event batcher drain task stopped".into()));
        }
        Ok(())
    }

    /// Update the in-memory per-session rollup (soldr#1536). Runs on the
    /// caller's task before the event is staged so the aggregate is
    /// never behind the persistence channel.
    fn observe_for_aggregate(&self, event: &Event) {
        let Some(session_id) = event.session_id else {
            return;
        };
        let mut map = self
            .aggregates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match event.kind {
            EventKind::SessionStart => {
                // Session observed from birth: the aggregate is
                // authoritative for the whole session.
                let entry = map.entry(session_id).or_default();
                entry.complete = true;
                entry.observe(event);
            }
            EventKind::CompileStart | EventKind::CompileEnd => {
                // First sight mid-session (daemon auto-started or
                // restarted mid-build): track it, but leave `complete`
                // false so finalization falls back to the redb scan.
                map.entry(session_id).or_default().observe(event);
            }
            // The terminator is recorded by the finalizer AFTER the
            // aggregate was consumed — never resurrect an entry for it.
            EventKind::SessionEnd => return,
        }
        if map.len() > MAX_TRACKED_SESSIONS {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, agg)| agg.last_seen_ms)
                .map(|(id, _)| *id)
            {
                map.remove(&oldest);
            }
        }
    }

    /// Consume the per-session rollup for `session_id` (soldr#1536).
    /// Returns `None` when the daemon did not observe the session from
    /// its `SessionStart` (restart / late auto-start mid-build) or never
    /// saw it at all — callers must then fall back to
    /// [`crate::daemon::db::aggregate_session`]. The entry is removed
    /// either way; the session is over.
    pub fn take_session_aggregate(&self, session_id: u64) -> Option<SessionAggregate> {
        let mut map = self
            .aggregates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.remove(&session_id).filter(|agg| agg.complete)
    }

    /// Put a consumed aggregate back when finalization could not be persisted.
    /// This keeps a later retry lossless.
    pub fn restore_session_aggregate(&self, session_id: u64, aggregate: SessionAggregate) {
        let mut map = self
            .aggregates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.insert(session_id, aggregate);
    }

    /// Force a flush and wait for the drain task to acknowledge. Called
    /// from `Request::BuildSessionEnd` before the session-level
    /// aggregation runs, so the aggregator sees every event recorded
    /// during the session.
    pub async fn flush(&self) -> Result<(), EventBatcherError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(BatcherCmd::Flush(ack_tx)).await.is_err() {
            return Err(EventBatcherError("event batcher drain task stopped".into()));
        }
        ack_rx
            .await
            .map_err(|_| EventBatcherError("event batcher dropped flush acknowledgement".into()))?
            .map_err(EventBatcherError)
    }

    /// Final flush + drain task exit. Called from the daemon shutdown
    /// path before the redb file lock is released.
    pub async fn shutdown(&self) -> Result<(), EventBatcherError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(BatcherCmd::Shutdown(ack_tx)).await.is_err() {
            return Err(EventBatcherError("event batcher drain task stopped".into()));
        }
        ack_rx
            .await
            .map_err(|_| {
                EventBatcherError("event batcher dropped shutdown acknowledgement".into())
            })?
            .map_err(EventBatcherError)
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
                        let _ = flush_batch(&db_path, &mut buf);
                    }
                }
                Some(BatcherCmd::Flush(ack)) => {
                    let result = flush_batch(&db_path, &mut buf);
                    let _ = ack.send(result);
                }
                Some(BatcherCmd::Shutdown(ack)) => {
                    let result = flush_batch(&db_path, &mut buf);
                    let done = result.is_ok();
                    let _ = ack.send(result);
                    if done { return; }
                }
                None => {
                    // Last sender dropped. Drain whatever is left and
                    // exit — no one is around to receive an ack.
                    let _ = flush_batch(&db_path, &mut buf);
                    return;
                }
            },
            _ = interval.tick() => {
                if !buf.is_empty() {
                    let _ = flush_batch(&db_path, &mut buf);
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
fn flush_batch(db_path: &Path, buf: &mut Vec<Event>) -> Result<(), String> {
    if buf.is_empty() {
        return Ok(());
    }
    let count = buf.len();
    tracing::debug!("event_batcher: flushing {count} pending event rows");
    if let Err(err) = write_batch(db_path, buf) {
        tracing::error!("event_batcher: flush failed; retaining {count} pending event rows: {err}");
        return Err(err.to_string());
    }
    buf.clear();
    Ok(())
}

/// Open `state.redb` under the shared `state_db_open_lock`, allocate
/// `count` event IDs in a single META insert, and insert every row in
/// the same write txn. One fsync covers the entire batch.
///
/// `pub(crate)` so tests (soldr#1536 finalization-scaling guards) can
/// seed large event histories in a single transaction.
pub(crate) fn write_batch(db_path: &Path, buf: &[Event]) -> std::io::Result<()> {
    let handle =
        open_state_db(db_path).map_err(|e| std::io::Error::other(format!("redb open: {e}")))?;
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
#[allow(unused_must_use)]
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

    fn session_start_event(session: u64) -> Event {
        Event {
            ts_ms: 1_700_000_000_000,
            session_id: Some(session),
            kind: EventKind::SessionStart,
            crate_name: None,
            duration_us: None,
            target_dir: None,
            exit_code: None,
        }
    }

    crate::timed_test!(
        session_aggregate_matches_db_aggregate_and_is_consumed_once,
        {
            let rt = Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let dir = TempDir::new().expect("tempdir");
                let path = dir.path().join("state.redb");
                crate::daemon::db::ensure_initialized(&path).expect("init");
                let batcher = EventBatcher::start(path.clone());
                batcher.record(session_start_event(21)).await;
                for i in 0..4 {
                    batcher
                        .record(sample_event(21, &format!("s{i}"), None))
                        .await;
                    batcher
                        .record(sample_event(21, &format!("s{i}"), Some(500 + i)))
                        .await;
                }
                batcher.flush().await;

                let agg = batcher
                    .take_session_aggregate(21)
                    .expect("aggregate tracked from SessionStart");
                let in_memory = agg.finalize();
                let from_db = aggregate_session(&path, 21).expect("db agg");
                assert_eq!(
                    in_memory, from_db,
                    "incremental rollup must mirror the scan"
                );
                assert_eq!(in_memory.0, 4);
                assert_eq!(in_memory.1, Some(503));
                assert_eq!(in_memory.2.as_deref(), Some("s3"));

                // Consumed: a second take falls back to the scan path.
                assert!(batcher.take_session_aggregate(21).is_none());
                batcher.shutdown().await;
            });
        }
    );

    crate::timed_test!(session_aggregate_without_session_start_is_untrusted, {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("state.redb");
            crate::daemon::db::ensure_initialized(&path).expect("init");
            let batcher = EventBatcher::start(path.clone());
            // Daemon (re)started mid-build: compiles arrive without a
            // SessionStart. Earlier events may already be in redb, so
            // the incremental rollup must NOT claim authority.
            batcher.record(sample_event(33, "late", Some(9))).await;
            batcher.flush().await;
            assert!(
                batcher.take_session_aggregate(33).is_none(),
                "mid-session aggregates must force the redb-scan fallback"
            );
            batcher.shutdown().await;
        });
    });

    crate::timed_test!(session_end_event_does_not_resurrect_aggregate, {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("state.redb");
            crate::daemon::db::ensure_initialized(&path).expect("init");
            let batcher = EventBatcher::start(path.clone());
            batcher.record(session_start_event(55)).await;
            assert!(batcher.take_session_aggregate(55).is_some());
            // The finalizer records the SessionEnd terminator after the
            // take; it must not re-create a (now-stale) entry.
            batcher
                .record(Event {
                    ts_ms: 1_700_000_000_500,
                    session_id: Some(55),
                    kind: EventKind::SessionEnd,
                    crate_name: None,
                    duration_us: None,
                    target_dir: None,
                    exit_code: Some(0),
                })
                .await;
            assert!(batcher.take_session_aggregate(55).is_none());
            batcher.shutdown().await;
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

    crate::timed_test!(failed_flush_retains_rows_for_retry, {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new().expect("tempdir");
            let parent = dir.path().join("file-parent");
            let path = parent.join("state.redb");
            std::fs::write(&parent, "not a directory").expect("create blocking parent");
            let batcher = EventBatcher::start(path.clone());
            batcher
                .record(sample_event(101, "retained", Some(7)))
                .await
                .expect("queue event");
            assert!(batcher.flush().await.is_err(), "file parent must fail");

            std::fs::remove_file(&parent).expect("remove blocking parent");
            std::fs::create_dir(&parent).expect("create db parent");
            crate::daemon::db::ensure_initialized(&path).expect("init");
            batcher.flush().await.expect("retry flush");
            let (count, _, _) = aggregate_session(&path, 101).expect("aggregate");
            assert_eq!(count, 1, "failed batch must be retried, not dropped");
            batcher.shutdown().await.expect("shutdown");
        });
    });
}
