//! Per-unit measured peak memory, remembered across builds (soldr#3152 step 4).
//!
//! The owner's decision for memory-aware admission: remember the last measured
//! peak of each compile unit and admit on it, instead of predicting peak memory
//! from command-line features. The first 1,161 joined CI units showed those
//! features barely predict it (R\u{b2} 0.09 on `--extern` bytes), while a unit's
//! measured peak is stable from run to run.
//!
//! Units are keyed by `memory_estimate::unit_key` (`<crate name>/<cargo -C
//! metadata>`), the identity that survives zccache rewriting the argument
//! vector. The measurement comes from zccache's per-compile `ChildMemory`:
//! the compiler's own high-water mark and the sampled peak of its whole live
//! process tree, which is the one that includes a linking `rustc`'s `ld`.
//!
//! Rows live in the shared `state.sqlite3` table `unit_memory_history` as
//! `[0x01][prost WireUnitMemory]`, like every other persisted row.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::{mpsc, oneshot, watch};

use crate::cache_lib::target_registry::RegistryError;
use crate::core::wire::{prost_tagged_bytes, proto::WireUnitMemory, REDB_TAG_PROST};
use prost::Message;

/// One unit's last measured peak memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitMemory {
    pub peak_rss_bytes: u64,
    pub tree_peak_rss_bytes: u64,
    pub updated_ms: i64,
    /// How many measurements have replaced this row.
    pub samples: u64,
}

fn invalid_data(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> RegistryError {
    RegistryError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

fn decode(bytes: &[u8]) -> Result<UnitMemory, RegistryError> {
    let Some((&REDB_TAG_PROST, body)) = bytes.split_first() else {
        return Err(invalid_data(
            "unit_memory_history row is not a tagged prost row",
        ));
    };
    let wire = WireUnitMemory::decode(body).map_err(invalid_data)?;
    Ok(UnitMemory {
        peak_rss_bytes: wire.peak_rss_bytes,
        tree_peak_rss_bytes: wire.tree_peak_rss_bytes,
        updated_ms: wire.updated_ms,
        samples: wire.samples,
    })
}

/// The unit's last measured peak, or `None` when it has never been measured.
pub fn lookup_in(db: &Connection, unit_key: &str) -> Result<Option<UnitMemory>, RegistryError> {
    let row: Option<Vec<u8>> = db
        .query_row(
            "SELECT value FROM unit_memory_history WHERE key = ?1",
            params![unit_key],
            |row| row.get(0),
        )
        .optional()?;
    row.as_deref().map(decode).transpose()
}

/// Record a new measurement for `unit_key`. The latest measurement replaces
/// the stored peaks, so a unit that got cheaper is not held to an old figure;
/// `samples` counts how many measurements the row has seen.
pub fn record_in(
    db: &Connection,
    unit_key: &str,
    peak_rss_bytes: u64,
    tree_peak_rss_bytes: u64,
    now_ms: i64,
) -> Result<(), RegistryError> {
    let samples = lookup_in(db, unit_key)
        .ok()
        .flatten()
        .map_or(0, |unit| unit.samples)
        .saturating_add(1);
    let bytes = prost_tagged_bytes(&WireUnitMemory {
        peak_rss_bytes,
        tree_peak_rss_bytes,
        updated_ms: now_ms,
        samples,
    });
    db.execute(
        "INSERT INTO unit_memory_history(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![unit_key, bytes],
    )?;
    Ok(())
}

/// Every recorded unit, for warming an in-memory map at daemon start.
pub fn load_all_in(db: &Connection) -> Result<Vec<(String, UnitMemory)>, RegistryError> {
    let mut statement = db.prepare("SELECT key, value FROM unit_memory_history")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut units = Vec::new();
    for row in rows {
        let (key, bytes) = row?;
        units.push((key, decode(&bytes)?));
    }
    Ok(units)
}

/// Bounded so a stalled disk cannot grow memory without limit; a full channel
/// drops the write, never the compile. The in-memory map still has the value.
const CHANNEL_CAPACITY: usize = 1024;

/// Most rows written in one SQLite transaction batch.
const MAX_BATCH_ROWS: usize = 256;

enum Command {
    Record {
        unit_key: String,
        peak_rss_bytes: u64,
        tree_peak_rss_bytes: u64,
        now_ms: i64,
    },
    Flush(oneshot::Sender<Result<(), String>>),
}

/// The daemon's live view of [`unit_memory_history`](self).
///
/// Admission reads [`UnitMemoryHistory::lookup`] on the compile path, so it
/// never touches SQLite: lookups answer from an in-memory map that
/// [`UnitMemoryHistory::record`] updates immediately. One background task owns
/// the database. It warms the map from the table at start, then writes
/// recorded measurements in batches through `spawn_blocking`, the same shape
/// as the event batcher (soldr#980).
#[derive(Clone, Debug)]
pub struct UnitMemoryHistory {
    units: Arc<Mutex<HashMap<String, UnitMemory>>>,
    tx: mpsc::Sender<Command>,
    loaded: watch::Receiver<bool>,
}

impl UnitMemoryHistory {
    /// Start the owner task. Must be called inside a tokio runtime.
    pub fn start(db_path: PathBuf) -> Self {
        let units = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (loaded_tx, loaded) = watch::channel(false);
        tokio::spawn(drain(db_path, Arc::clone(&units), rx, loaded_tx));
        Self { units, tx, loaded }
    }

    /// The unit's last measured peak, if this daemon has one.
    pub fn lookup(&self, unit_key: &str) -> Option<UnitMemory> {
        self.units
            .lock()
            .ok()
            .and_then(|units| units.get(unit_key).copied())
    }

    /// Record a compile's measured memory. An all-zero measurement is the
    /// absence of one (a cache hit spawns no compiler) and is ignored.
    pub fn record(&self, unit_key: &str, peak_rss_bytes: u64, tree_peak_rss_bytes: u64) {
        if peak_rss_bytes == 0 && tree_peak_rss_bytes == 0 {
            return;
        }
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        if let Ok(mut units) = self.units.lock() {
            let samples = units
                .get(unit_key)
                .map_or(0, |unit| unit.samples)
                .saturating_add(1);
            units.insert(
                unit_key.to_string(),
                UnitMemory {
                    peak_rss_bytes,
                    tree_peak_rss_bytes,
                    updated_ms: now_ms,
                    samples,
                },
            );
        }
        let _ = self.tx.try_send(Command::Record {
            unit_key: unit_key.to_string(),
            peak_rss_bytes,
            tree_peak_rss_bytes,
            now_ms,
        });
    }

    /// Resolve once the warm load from the table has finished.
    pub async fn ready(&self) -> Result<(), String> {
        let mut loaded = self.loaded.clone();
        loaded
            .wait_for(|done| *done)
            .await
            .map(|_| ())
            .map_err(|_| "unit memory history task stopped before loading".to_string())
    }

    /// Resolve once every measurement recorded so far is on disk.
    pub async fn flush(&self) -> Result<(), String> {
        let (reply, done) = oneshot::channel();
        self.tx
            .send(Command::Flush(reply))
            .await
            .map_err(|_| "unit memory history task stopped".to_string())?;
        done.await
            .map_err(|_| "unit memory history task dropped the flush".to_string())?
    }
}

async fn drain(
    db_path: PathBuf,
    units: Arc<Mutex<HashMap<String, UnitMemory>>>,
    mut rx: mpsc::Receiver<Command>,
    loaded: watch::Sender<bool>,
) {
    let load_path = db_path.clone();
    let stored = tokio::task::spawn_blocking(move || {
        let db = crate::cache_lib::state_store::open_state_db(&load_path).ok()?;
        load_all_in(&db).ok()
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    if let Ok(mut map) = units.lock() {
        for (key, unit) in stored {
            // A measurement recorded while the load ran is newer; keep it.
            map.entry(key).or_insert(unit);
        }
    }
    let _ = loaded.send(true);

    let mut pending = Vec::new();
    while let Some(command) = rx.recv().await {
        let mut flushes = Vec::new();
        let mut next = Some(command);
        while let Some(command) = next.take() {
            match command {
                Command::Record {
                    unit_key,
                    peak_rss_bytes,
                    tree_peak_rss_bytes,
                    now_ms,
                } => pending.push((unit_key, peak_rss_bytes, tree_peak_rss_bytes, now_ms)),
                Command::Flush(reply) => flushes.push(reply),
            }
            if pending.len() < MAX_BATCH_ROWS {
                next = rx.try_recv().ok();
            }
        }
        let result = write_batch(db_path.clone(), std::mem::take(&mut pending)).await;
        for reply in flushes {
            let _ = reply.send(result.clone());
        }
    }
}

async fn write_batch(db_path: PathBuf, batch: Vec<(String, u64, u64, i64)>) -> Result<(), String> {
    if batch.is_empty() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || {
        let db = crate::cache_lib::state_store::open_state_db(&db_path)
            .map_err(|error| error.to_string())?;
        for (unit_key, peak, tree_peak, now_ms) in batch {
            record_in(&db, &unit_key, peak, tree_peak, now_ms)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(test)]
#[path = "unit_memory_history_tests.rs"]
mod tests;
