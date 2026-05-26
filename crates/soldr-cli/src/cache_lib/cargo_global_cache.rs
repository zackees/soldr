//! Read-only access to cargo's `$CARGO_HOME/.global-cache` SQLite
//! database (issue #349).
//!
//! Cargo maintains accurate last-access timestamps for everything it
//! tracks in `~/.cargo/.global-cache` so its own `cargo clean gc` /
//! `-Zgc` can evict the least-recently-used entries. That data is more
//! accurate than the directory mtimes we used to derive in
//! `walk_cargo_registry_src`, where `cargo build` re-extracting an
//! existing crate source dir can leave the mtime stale.
//!
//! This module exposes a single function: open the SQLite file
//! read-only, run a join against `registry_src` and `registry_index`,
//! and return a map keyed by `(registry-hash, crate, version)`. Any
//! failure (missing file, locked database, schema drift, table
//! missing) yields `None` so the caller falls back to mtime.
//!
//! Schema documented inline in cargo's source:
//! <https://github.com/rust-lang/cargo/blob/master/src/cargo/core/global_cache_tracker.rs>

use rusqlite::{Connection, OpenFlags, Result as SqlResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Path of the SQLite database cargo writes to.
pub const GLOBAL_CACHE_DB_FILENAME: &str = ".global-cache";

/// Map key: `(registry-hash-dirname, crate-name, version)`. The
/// registry-hash matches the directory name under
/// `$CARGO_HOME/registry/src/`, e.g. `index.crates.io-6f17d22bba15001f`.
pub type RegistrySrcKey = (String, String, String);

/// Resolve the path of cargo's global cache database for the given
/// `$CARGO_HOME`. Does not check that the file exists.
pub fn global_cache_db_path(cargo_home: &Path) -> PathBuf {
    cargo_home.join(GLOBAL_CACHE_DB_FILENAME)
}

/// Read `registry_src.timestamp` joined with `registry_index.name`
/// (the registry-hash dirname) and return a map of last-used Unix
/// seconds keyed by `(registry-hash, crate, version)`.
///
/// Returns `None` when:
///
/// - The database file does not exist.
/// - The SQLite open call fails (file is locked, permissions denied,
///   etc.).
/// - Either `registry_src` or `registry_index` is missing.
/// - The expected columns are missing or have an incompatible type.
///
/// The caller falls back to filesystem mtime in all of those cases.
pub fn read_registry_src_last_used(cargo_home: &Path) -> Option<HashMap<RegistrySrcKey, i64>> {
    let path = global_cache_db_path(cargo_home);
    if !path.is_file() {
        return None;
    }
    read_registry_src_last_used_from_path(&path).ok()
}

fn read_registry_src_last_used_from_path(path: &Path) -> SqlResult<HashMap<RegistrySrcKey, i64>> {
    // Read-only open: never mutates cargo's database. `NO_MUTEX` lets
    // multiple soldr invocations open the DB concurrently — cargo's
    // own writes coordinate through its package-cache lock above the
    // SQLite layer.
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    // Read-only busy-timeout: a brief wait while cargo finishes a
    // write is preferable to an immediate SQLITE_BUSY error.
    conn.busy_timeout(std::time::Duration::from_millis(500))?;

    let mut stmt = conn.prepare(
        "SELECT registry_index.name, registry_src.name, registry_src.version, registry_src.timestamp \
         FROM registry_src \
         JOIN registry_index ON registry_src.registry_id = registry_index.id",
    )?;

    let mut rows = stmt.query([])?;
    let mut out: HashMap<RegistrySrcKey, i64> = HashMap::new();
    while let Some(row) = rows.next()? {
        let registry_hash: String = row.get(0)?;
        let crate_name: String = row.get(1)?;
        let version: String = row.get(2)?;
        let timestamp: i64 = row.get(3)?;
        out.insert((registry_hash, crate_name, version), timestamp);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::tempdir;

    /// Build a minimal SQLite database that matches the schema soldr
    /// reads. We don't need a full cargo install — just the two tables
    /// and the join columns.
    fn make_global_cache_db(cargo_home: &Path) -> PathBuf {
        let path = global_cache_db_path(cargo_home);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE registry_index (
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                name TEXT UNIQUE NOT NULL,
                timestamp INTEGER NOT NULL
            );
            CREATE TABLE registry_src (
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                registry_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                version TEXT NOT NULL,
                size INTEGER NOT NULL,
                timestamp INTEGER NOT NULL,
                UNIQUE (registry_id, name, version)
            );
            "#,
        )
        .unwrap();
        path
    }

    fn insert_registry(conn: &Connection, name: &str, ts: i64) -> i64 {
        conn.execute(
            "INSERT INTO registry_index (name, timestamp) VALUES (?, ?)",
            params![name, ts],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_registry_src(
        conn: &Connection,
        registry_id: i64,
        name: &str,
        version: &str,
        ts: i64,
    ) {
        conn.execute(
            "INSERT INTO registry_src (registry_id, name, version, size, timestamp) VALUES (?, ?, ?, ?, ?)",
            params![registry_id, name, version, 0i64, ts],
        )
        .unwrap();
    }

    #[test]
    fn returns_none_when_db_missing() {
        let tmp = tempdir().unwrap();
        assert!(read_registry_src_last_used(tmp.path()).is_none());
    }

    #[test]
    fn returns_none_when_file_is_not_a_sqlite_database() {
        let tmp = tempdir().unwrap();
        std::fs::write(global_cache_db_path(tmp.path()), b"not a database").unwrap();
        assert!(read_registry_src_last_used(tmp.path()).is_none());
    }

    #[test]
    fn returns_none_when_schema_is_unrelated() {
        let tmp = tempdir().unwrap();
        let path = global_cache_db_path(tmp.path());
        let conn = Connection::open(&path).unwrap();
        // No `registry_src` / `registry_index` — cargo schema drift
        // (or someone else's DB at the same path). Falling back to
        // mtime is correct here.
        conn.execute("CREATE TABLE unrelated (id INTEGER)", [])
            .unwrap();
        assert!(read_registry_src_last_used(tmp.path()).is_none());
    }

    #[test]
    fn returns_joined_map_for_well_formed_db() {
        let tmp = tempdir().unwrap();
        let db_path = make_global_cache_db(tmp.path());
        let conn = Connection::open(&db_path).unwrap();
        let crates_io = insert_registry(&conn, "index.crates.io-6f17d22bba15001f", 1700);
        let alt = insert_registry(&conn, "alt-registry-deadbeef", 1800);
        insert_registry_src(&conn, crates_io, "serde", "1.0.0", 1000);
        insert_registry_src(&conn, crates_io, "serde", "1.1.0", 2000);
        insert_registry_src(&conn, alt, "private-thing", "0.1.0", 3000);
        drop(conn);

        let map = read_registry_src_last_used(tmp.path()).expect("populated db must return Some");
        assert_eq!(
            map.get(&(
                "index.crates.io-6f17d22bba15001f".to_string(),
                "serde".to_string(),
                "1.0.0".to_string()
            )),
            Some(&1000)
        );
        assert_eq!(
            map.get(&(
                "index.crates.io-6f17d22bba15001f".to_string(),
                "serde".to_string(),
                "1.1.0".to_string()
            )),
            Some(&2000)
        );
        assert_eq!(
            map.get(&(
                "alt-registry-deadbeef".to_string(),
                "private-thing".to_string(),
                "0.1.0".to_string()
            )),
            Some(&3000)
        );
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn empty_tables_yield_empty_map_not_none() {
        // The DB exists, schema matches, but there are no rows yet —
        // an empty map (Some) tells the caller "consult me, I have
        // no data on this crate" rather than the "missing/locked DB"
        // signal None implies.
        let tmp = tempdir().unwrap();
        make_global_cache_db(tmp.path());
        let map = read_registry_src_last_used(tmp.path()).unwrap();
        assert!(map.is_empty());
    }
}
