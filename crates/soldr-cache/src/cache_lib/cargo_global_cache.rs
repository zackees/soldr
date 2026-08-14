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
//! This module exposes read-only readers: open the SQLite file
//! read-only, run a join against `registry_src` and `registry_index`
//! (or `git_checkout` and `git_db`), and return a map keyed by
//! `(registry-hash, src-dirname)` / `(git-db-dirname,
//! checkout-dirname)`. Any failure (missing file, locked database,
//! schema drift, table missing) yields `None` so the caller falls
//! back to mtime.
//!
//! Schema documented inline in cargo's source:
//! <https://github.com/rust-lang/cargo/blob/master/src/cargo/core/global_cache_tracker.rs>

use rusqlite::{Connection, OpenFlags, Result as SqlResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Path of the SQLite database cargo writes to.
pub const GLOBAL_CACHE_DB_FILENAME: &str = ".global-cache";

/// Map key: `(registry-hash-dirname, src-dirname)`. The registry-hash
/// matches the directory name under `$CARGO_HOME/registry/src/`, e.g.
/// `index.crates.io-6f17d22bba15001f`. The src-dirname is the crate
/// source directory name in the `<crate>-<version>` format cargo uses
/// both on disk and in the `registry_src.name` column, e.g.
/// `serde-1.0.219` (#1569 — there is no separate version column in
/// real cargo databases).
pub type RegistrySrcKey = (String, String);

/// Resolve the path of cargo's global cache database for the given
/// `$CARGO_HOME`. Does not check that the file exists.
pub fn global_cache_db_path(cargo_home: &Path) -> PathBuf {
    cargo_home.join(GLOBAL_CACHE_DB_FILENAME)
}

/// Read `registry_src.timestamp` joined with `registry_index.name`
/// (the registry-hash dirname) and return a map of last-used Unix
/// seconds keyed by `(registry-hash, src-dirname)` where src-dirname
/// is `<crate>-<version>`.
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

/// Map key: `(git-db-dirname, checkout-dirname)`. The first component
/// matches the directory name under both `$CARGO_HOME/git/db/` and
/// `$CARGO_HOME/git/checkouts/` (cargo uses the same `<repo>-<hash>`
/// ident for both), e.g. `soldr-76b10f3504cf35a4`. The second matches
/// the per-commit checkout directory name (cargo's short revision,
/// e.g. `3381ba4`).
pub type GitCheckoutKey = (String, String);

/// Read `git_checkout.timestamp` joined with `git_db.name` and return
/// a map of last-used Unix seconds keyed by
/// `(git-db-dirname, checkout-dirname)` (issue #1544).
///
/// Cargo refreshes the `git_checkout` row on every build that uses
/// the checkout, but sets the checkout directory's mtime only once at
/// checkout time — so an actively-used checkout looks arbitrarily old
/// to an mtime-only walker. This reader gives gc selection the real
/// usage recency.
///
/// Same failure contract as [`read_registry_src_last_used`]: any open
/// / lock / schema-drift / column-type failure yields `None` and the
/// caller falls back to filesystem mtime. The database is opened
/// strictly read-only; no bytes or mtimes are mutated.
pub fn read_git_checkout_last_used(cargo_home: &Path) -> Option<HashMap<GitCheckoutKey, i64>> {
    let path = global_cache_db_path(cargo_home);
    if !path.is_file() {
        return None;
    }
    read_git_checkout_last_used_from_path(&path).ok()
}

fn read_git_checkout_last_used_from_path(path: &Path) -> SqlResult<HashMap<GitCheckoutKey, i64>> {
    let conn = open_read_only(path)?;
    // Schema verified against a live cargo 1.94 `.global-cache`:
    //   git_db(id, name UNIQUE, timestamp)
    //   git_checkout(git_id, name, size, timestamp,
    //                PRIMARY KEY (git_id, name))
    let mut stmt = conn.prepare(
        "SELECT git_db.name, git_checkout.name, git_checkout.timestamp \
         FROM git_checkout \
         JOIN git_db ON git_checkout.git_id = git_db.id",
    )?;

    let mut rows = stmt.query([])?;
    let mut out: HashMap<GitCheckoutKey, i64> = HashMap::new();
    while let Some(row) = rows.next()? {
        let repo_dir: String = row.get(0)?;
        let checkout_dir: String = row.get(1)?;
        let timestamp: i64 = row.get(2)?;
        out.insert((repo_dir, checkout_dir), timestamp);
    }
    Ok(out)
}

/// Shared read-only open used by every `.global-cache` reader.
fn open_read_only(path: &Path) -> SqlResult<Connection> {
    // Read-only open: never mutates cargo's database. `NO_MUTEX` lets
    // multiple soldr invocations open the DB concurrently — cargo's
    // own writes coordinate through its package-cache lock above the
    // SQLite layer.
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    // Read-only busy-timeout: a brief wait while cargo finishes a
    // write is preferable to an immediate SQLITE_BUSY error.
    conn.busy_timeout(std::time::Duration::from_millis(500))?;
    Ok(conn)
}

fn read_registry_src_last_used_from_path(path: &Path) -> SqlResult<HashMap<RegistrySrcKey, i64>> {
    let conn = open_read_only(path)?;
    // Schema verified against a live cargo 1.94 `.global-cache`:
    //   registry_index(id, name UNIQUE, timestamp)
    //   registry_src(registry_id, name, size, timestamp,
    //                PRIMARY KEY (registry_id, name))
    // `registry_src.name` is the `<crate>-<version>` directory name —
    // there is NO separate `version` column (#1569; the previous query
    // referenced one and errored on every real installation).
    let mut stmt = conn.prepare(
        "SELECT registry_index.name, registry_src.name, registry_src.timestamp \
         FROM registry_src \
         JOIN registry_index ON registry_src.registry_id = registry_index.id",
    )?;

    let mut rows = stmt.query([])?;
    let mut out: HashMap<RegistrySrcKey, i64> = HashMap::new();
    while let Some(row) = rows.next()? {
        let registry_hash: String = row.get(0)?;
        let src_dir: String = row.get(1)?;
        let timestamp: i64 = row.get(2)?;
        out.insert((registry_hash, src_dir), timestamp);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::tempdir;

    /// Build a SQLite database whose registry tables match the REAL
    /// schema a live cargo 1.94 writes (CREATE TABLE statements copied
    /// verbatim from a cargo-produced `~/.cargo/.global-cache`).
    /// `registry_src.name` is the `<crate>-<version>` directory name;
    /// there is no `version` column (#1569 — the old synthetic fixture
    /// invented one and hid the schema mismatch).
    fn make_global_cache_db(cargo_home: &Path) -> PathBuf {
        let path = global_cache_db_path(cargo_home);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE registry_index (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL,
                timestamp INTEGER NOT NULL
            );
            CREATE TABLE registry_src (
                registry_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                size INTEGER,
                timestamp INTEGER NOT NULL,
                PRIMARY KEY (registry_id, name),
                FOREIGN KEY (registry_id) REFERENCES registry_index (id) ON DELETE CASCADE
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

    fn insert_registry_src(conn: &Connection, registry_id: i64, dir_name: &str, ts: i64) {
        conn.execute(
            "INSERT INTO registry_src (registry_id, name, size, timestamp) VALUES (?, ?, ?, ?)",
            params![registry_id, dir_name, 0i64, ts],
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
        insert_registry_src(&conn, crates_io, "serde-1.0.0", 1000);
        insert_registry_src(&conn, crates_io, "serde-1.1.0", 2000);
        insert_registry_src(&conn, alt, "private-thing-0.1.0", 3000);
        drop(conn);

        let map = read_registry_src_last_used(tmp.path()).expect("populated db must return Some");
        assert_eq!(
            map.get(&(
                "index.crates.io-6f17d22bba15001f".to_string(),
                "serde-1.0.0".to_string()
            )),
            Some(&1000)
        );
        assert_eq!(
            map.get(&(
                "index.crates.io-6f17d22bba15001f".to_string(),
                "serde-1.1.0".to_string()
            )),
            Some(&2000)
        );
        assert_eq!(
            map.get(&(
                "alt-registry-deadbeef".to_string(),
                "private-thing-0.1.0".to_string()
            )),
            Some(&3000)
        );
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn registry_src_read_never_mutates_db_bytes() {
        // Same read-only guarantee as the git reader: compare the raw
        // file bytes before and after a successful read.
        let tmp = tempdir().unwrap();
        let db_path = make_global_cache_db(tmp.path());
        let conn = Connection::open(&db_path).unwrap();
        let crates_io = insert_registry(&conn, "index.crates.io-6f17d22bba15001f", 1700);
        insert_registry_src(&conn, crates_io, "serde-1.0.0", 1000);
        drop(conn);

        let before = std::fs::read(&db_path).unwrap();
        let map = read_registry_src_last_used(tmp.path()).unwrap();
        assert_eq!(map.len(), 1);
        let after = std::fs::read(&db_path).unwrap();
        assert_eq!(before, after, "read-only reader mutated the DB bytes");
    }

    // ---------------------------------------------------------------
    // git_checkout reader (issue #1544).
    // ---------------------------------------------------------------

    /// Build a minimal SQLite database matching the git tables cargo
    /// 1.94 actually writes (verified against a live cargo-produced
    /// `.global-cache`).
    fn make_git_global_cache_db(cargo_home: &Path) -> PathBuf {
        let path = global_cache_db_path(cargo_home);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE git_db (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL,
                timestamp INTEGER NOT NULL
            );
            CREATE TABLE git_checkout (
                git_id INTEGER NOT NULL,
                name TEXT UNIQUE NOT NULL,
                size INTEGER,
                timestamp INTEGER NOT NULL,
                PRIMARY KEY (git_id, name),
                FOREIGN KEY (git_id) REFERENCES git_db (id) ON DELETE CASCADE
            );
            "#,
        )
        .unwrap();
        path
    }

    fn insert_git_db(conn: &Connection, name: &str, ts: i64) -> i64 {
        conn.execute(
            "INSERT INTO git_db (name, timestamp) VALUES (?, ?)",
            params![name, ts],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_git_checkout(conn: &Connection, git_id: i64, name: &str, ts: i64) {
        conn.execute(
            "INSERT INTO git_checkout (git_id, name, size, timestamp) VALUES (?, ?, NULL, ?)",
            params![git_id, name, ts],
        )
        .unwrap();
    }

    #[test]
    fn git_checkout_returns_none_when_db_missing() {
        let tmp = tempdir().unwrap();
        assert!(read_git_checkout_last_used(tmp.path()).is_none());
    }

    #[test]
    fn git_checkout_returns_none_when_file_is_not_sqlite() {
        let tmp = tempdir().unwrap();
        std::fs::write(global_cache_db_path(tmp.path()), b"not a database").unwrap();
        assert!(read_git_checkout_last_used(tmp.path()).is_none());
    }

    #[test]
    fn git_checkout_returns_none_when_schema_is_unrelated() {
        let tmp = tempdir().unwrap();
        let path = global_cache_db_path(tmp.path());
        let conn = Connection::open(&path).unwrap();
        // No `git_db` / `git_checkout` — cargo schema drift. Falling
        // back to mtime is correct here.
        conn.execute("CREATE TABLE unrelated (id INTEGER)", [])
            .unwrap();
        assert!(read_git_checkout_last_used(tmp.path()).is_none());
    }

    #[test]
    fn git_checkout_returns_joined_map_for_well_formed_db() {
        let tmp = tempdir().unwrap();
        let db_path = make_git_global_cache_db(tmp.path());
        let conn = Connection::open(&db_path).unwrap();
        let dep = insert_git_db(&conn, "dep-76b10f3504cf35a4", 1700);
        let other = insert_git_db(&conn, "other-1122334455667788", 1800);
        insert_git_checkout(&conn, dep, "3381ba4", 1000);
        insert_git_checkout(&conn, other, "abc1234", 3000);
        drop(conn);

        let map = read_git_checkout_last_used(tmp.path()).expect("populated db must return Some");
        assert_eq!(
            map.get(&("dep-76b10f3504cf35a4".to_string(), "3381ba4".to_string())),
            Some(&1000)
        );
        assert_eq!(
            map.get(&("other-1122334455667788".to_string(), "abc1234".to_string())),
            Some(&3000)
        );
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn git_checkout_empty_tables_yield_empty_map_not_none() {
        let tmp = tempdir().unwrap();
        make_git_global_cache_db(tmp.path());
        let map = read_git_checkout_last_used(tmp.path()).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn git_checkout_read_never_mutates_db_bytes() {
        // Gate (b) of #1544: the reader must never mutate cargo's
        // database. Compare the raw file bytes before and after a
        // successful read.
        let tmp = tempdir().unwrap();
        let db_path = make_git_global_cache_db(tmp.path());
        let conn = Connection::open(&db_path).unwrap();
        let dep = insert_git_db(&conn, "dep-76b10f3504cf35a4", 1700);
        insert_git_checkout(&conn, dep, "3381ba4", 1000);
        drop(conn);

        let before = std::fs::read(&db_path).unwrap();
        let map = read_git_checkout_last_used(tmp.path()).unwrap();
        assert_eq!(map.len(), 1);
        let after = std::fs::read(&db_path).unwrap();
        assert_eq!(before, after, "read-only reader mutated the DB bytes");
    }

    #[test]
    fn registry_src_query_does_not_error_on_real_cargo_schema() {
        // Canary distinguishing "row absent" from "query invalid"
        // (#1569): the raw SqlResult must be Ok against the REAL cargo
        // schema. Before the fix the query referenced a nonexistent
        // `registry_src.version` column — "no such column" — and the
        // public wrapper silently degraded every real installation to
        // the mtime fallback. A populated fixture with a row would
        // also pass an Ok-with-empty-map bug, so assert the row too.
        let tmp = tempdir().unwrap();
        let db_path = make_global_cache_db(tmp.path());
        let conn = Connection::open(&db_path).unwrap();
        let crates_io = insert_registry(&conn, "index.crates.io-1949cf8c6b5b557f", 1_783_723_771);
        insert_registry_src(&conn, crates_io, "serde-1.0.219", 1_783_723_771);
        drop(conn);

        let res = read_registry_src_last_used_from_path(&db_path);
        let map = match res {
            Ok(map) => map,
            Err(e) => {
                panic!("registry_src query must not error against the real cargo schema: {e:?}")
            }
        };
        assert_eq!(
            map.get(&(
                "index.crates.io-1949cf8c6b5b557f".to_string(),
                "serde-1.0.219".to_string()
            )),
            Some(&1_783_723_771)
        );
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
