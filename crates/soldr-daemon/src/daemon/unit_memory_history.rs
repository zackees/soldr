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

use rusqlite::{params, Connection, OptionalExtension};

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

#[cfg(test)]
#[path = "unit_memory_history_tests.rs"]
mod tests;
