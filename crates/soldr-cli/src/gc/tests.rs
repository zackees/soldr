//! Unit tests for [`crate::gc`].

use super::*;

#[test]
fn gc_purge_prompt_defaults_enter_to_yes() {
    for input in ["", "\n", "y", "Y", "yes", " YES "] {
        assert!(parse_gc_purge_answer(input), "expected {input:?} to accept");
    }
    for input in ["n", "no", "anything else"] {
        assert!(!parse_gc_purge_answer(input), "expected {input:?} to skip");
    }
}

#[test]
fn gc_purge_worker_count_is_bounded() {
    assert_eq!(gc_purge_worker_count_for(0), 1);
    assert_eq!(gc_purge_worker_count_for(1), 1);
    assert_eq!(gc_purge_worker_count_for(2), 2);
    assert_eq!(gc_purge_worker_count_for(16), 4);
}

// -------------------------------------------------------------------
// last-used resolution for cargo_registry_src entries (issue #349,
// key contract fixed in #1569). The precedence rule is: prefer
// cargo's `.global-cache` SQLite row when one exists for
// `(registry, <crate>-<version> dirname)`; otherwise fall back to the
// directory's filesystem mtime.
// -------------------------------------------------------------------

fn fake_metadata_with_mtime(temp_path: &std::path::Path, mtime_unix: u64) -> std::fs::Metadata {
    // Construct a synthetic file with a controlled mtime so the test
    // exercises real `Metadata::modified()` rather than mocking the
    // OS layer. `filetime::set_file_mtime` is in dev-deps.
    std::fs::write(temp_path, b"x").unwrap();
    let ft = filetime::FileTime::from_unix_time(mtime_unix as i64, 0);
    filetime::set_file_mtime(temp_path, ft).unwrap();
    std::fs::metadata(temp_path).unwrap()
}

#[test]
fn last_used_prefers_global_cache_when_key_present() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("probe");
    let meta = fake_metadata_with_mtime(&f, 1_700_000_000);
    let mut map: std::collections::HashMap<
        crate::cache_lib::cargo_global_cache::RegistrySrcKey,
        i64,
    > = std::collections::HashMap::new();
    map.insert(
        (
            "index.crates.io-abc123".to_string(),
            "serde-1.0.0".to_string(),
        ),
        1_800_000_000,
    );
    let (ts, source) =
        resolve_registry_src_last_used(Some(&map), "index.crates.io-abc123", "serde-1.0.0", &meta);
    assert_eq!(ts, 1_800_000_000);
    assert_eq!(source, "global_cache");
}

#[test]
fn last_used_falls_back_to_mtime_when_tracker_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("probe");
    let meta = fake_metadata_with_mtime(&f, 1_700_000_000);
    let (ts, source) =
        resolve_registry_src_last_used(None, "index.crates.io-abc123", "serde-1.0.0", &meta);
    assert_eq!(ts, 1_700_000_000);
    assert_eq!(source, "fs_mtime");
}

#[test]
fn last_used_falls_back_to_mtime_when_key_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("probe");
    let meta = fake_metadata_with_mtime(&f, 1_700_000_000);
    // Tracker exists (Some), but doesn't have a row for this crate.
    // The fallback must be per-crate, not per-tracker.
    let map: std::collections::HashMap<crate::cache_lib::cargo_global_cache::RegistrySrcKey, i64> =
        std::collections::HashMap::new();
    let (ts, source) =
        resolve_registry_src_last_used(Some(&map), "index.crates.io-abc123", "serde-1.0.0", &meta);
    assert_eq!(ts, 1_700_000_000);
    assert_eq!(source, "fs_mtime");
}

#[test]
fn last_used_falls_back_when_dir_name_is_not_versioned() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("probe");
    let meta = fake_metadata_with_mtime(&f, 1_700_000_000);
    // A dir name that doesn't match any tracker row (e.g. an aberrant
    // unversioned layout) must fall back to mtime rather than
    // crashing or returning 0.
    let map: std::collections::HashMap<crate::cache_lib::cargo_global_cache::RegistrySrcKey, i64> =
        std::collections::HashMap::new();
    let (ts, source) =
        resolve_registry_src_last_used(Some(&map), "index.crates.io-abc123", "bare-serde", &meta);
    assert_eq!(ts, 1_700_000_000);
    assert_eq!(source, "fs_mtime");
}

// -------------------------------------------------------------------
// last-used resolution for cargo_git_checkouts entries (issue #1544).
// Mirrors the registry-src precedence above: prefer cargo's
// `.global-cache` git_checkout row when one exists for
// `(git-db-dirname, checkout-dirname)`; otherwise fall back to the
// checkout directory's filesystem mtime.
//
// The repro that motivated this: cargo sets the checkout directory
// mtime once at checkout time and never touches it again — every
// later `cargo build` refreshes only the SQLite usage row. An
// actively-used checkout older than any age threshold therefore
// looks stale to an mtime-only walker and gets purge-selected,
// forcing a re-checkout (and the loop repeats once the fresh mtime
// ages past the threshold again).
// -------------------------------------------------------------------

/// Synthesize `$CARGO_HOME/git/checkouts/<repo>/<commit>/` with a
/// controlled directory mtime. Returns the commit dir path.
fn make_git_checkout_dir(
    cargo_home: &std::path::Path,
    repo_dir: &str,
    commit_dir: &str,
    mtime_unix: i64,
) -> std::path::PathBuf {
    let commit_path = cargo_home
        .join("git")
        .join("checkouts")
        .join(repo_dir)
        .join(commit_dir);
    std::fs::create_dir_all(&commit_path).unwrap();
    std::fs::write(commit_path.join("lib.rs"), b"pub fn f() {}").unwrap();
    // Set the mtime last — writing the file above touches the dir.
    let ft = filetime::FileTime::from_unix_time(mtime_unix, 0);
    filetime::set_file_mtime(&commit_path, ft).unwrap();
    commit_path
}

/// Create a `.global-cache` SQLite DB with the exact schema cargo
/// 1.94 writes for the git tables (verified against a live
/// cargo-produced DB; see cargo's `global_cache_tracker.rs`).
fn make_global_cache_git_db(cargo_home: &std::path::Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(cargo_home.join(".global-cache")).unwrap();
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
    conn
}

fn insert_git_checkout_row(conn: &rusqlite::Connection, repo_dir: &str, commit_dir: &str, ts: i64) {
    conn.execute(
        "INSERT OR IGNORE INTO git_db (name, timestamp) VALUES (?, ?)",
        rusqlite::params![repo_dir, ts],
    )
    .unwrap();
    let git_id: i64 = conn
        .query_row(
            "SELECT id FROM git_db WHERE name = ?",
            rusqlite::params![repo_dir],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO git_checkout (git_id, name, size, timestamp) VALUES (?, ?, NULL, ?)",
        rusqlite::params![git_id, commit_dir, ts],
    )
    .unwrap();
}

#[test]
fn git_checkout_last_used_prefers_global_cache_when_key_present() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("probe");
    let meta = fake_metadata_with_mtime(&f, 1_700_000_000);
    let mut map: std::collections::HashMap<
        crate::cache_lib::cargo_global_cache::GitCheckoutKey,
        i64,
    > = std::collections::HashMap::new();
    map.insert(
        ("dep-76b10f3504cf35a4".to_string(), "3381ba4".to_string()),
        1_800_000_000,
    );
    let (ts, source) =
        resolve_git_checkout_last_used(Some(&map), "dep-76b10f3504cf35a4", "3381ba4", &meta);
    assert_eq!(ts, 1_800_000_000);
    assert_eq!(source, "global_cache");
}

#[test]
fn git_checkout_last_used_falls_back_to_mtime_when_tracker_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("probe");
    let meta = fake_metadata_with_mtime(&f, 1_700_000_000);
    let (ts, source) =
        resolve_git_checkout_last_used(None, "dep-76b10f3504cf35a4", "3381ba4", &meta);
    assert_eq!(ts, 1_700_000_000);
    assert_eq!(source, "fs_mtime");
}

#[test]
fn git_checkout_last_used_falls_back_to_mtime_when_key_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("probe");
    let meta = fake_metadata_with_mtime(&f, 1_700_000_000);
    // Tracker exists (Some), but doesn't have a row for this
    // checkout. The fallback must be per-checkout, not per-tracker.
    let map: std::collections::HashMap<crate::cache_lib::cargo_global_cache::GitCheckoutKey, i64> =
        std::collections::HashMap::new();
    let (ts, source) =
        resolve_git_checkout_last_used(Some(&map), "dep-76b10f3504cf35a4", "3381ba4", &meta);
    assert_eq!(ts, 1_700_000_000);
    assert_eq!(source, "fs_mtime");
}

#[test]
fn active_git_checkout_with_fresh_db_usage_is_not_purge_selected() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo_home = tmp.path();
    let now: i64 = 2_000_000_000;
    let threshold_seconds: i64 = 30 * 86_400;
    let old_mtime = now - 100 * 86_400;
    let fresh_ts = now - 3_600;

    // Active checkout: dir mtime 100 days old (set at checkout
    // time), but cargo's tracker says it was used an hour ago.
    make_git_checkout_dir(cargo_home, "dep-76b10f3504cf35a4", "3381ba4", old_mtime);
    // Genuinely stale checkout: old mtime AND old tracker row.
    make_git_checkout_dir(cargo_home, "olddep-aabbccddeeff0011", "9f8e7d6", old_mtime);
    let conn = make_global_cache_git_db(cargo_home);
    insert_git_checkout_row(&conn, "dep-76b10f3504cf35a4", "3381ba4", fresh_ts);
    insert_git_checkout_row(
        &conn,
        "olddep-aabbccddeeff0011",
        "9f8e7d6",
        now - 200 * 86_400,
    );
    drop(conn);

    let entries = walk_cargo_git_checkouts(cargo_home, now);
    assert_eq!(entries.len(), 2);
    let active = entries
        .iter()
        .find(|e| e.path.contains("dep-76b10f3504cf35a4"))
        .unwrap();
    let stale = entries
        .iter()
        .find(|e| e.path.contains("olddep-aabbccddeeff0011"))
        .unwrap();

    // The active checkout must surface cargo's usage recency, not
    // the checkout-time mtime.
    assert_eq!(active.last_used_unix, fresh_ts);
    assert_eq!(active.last_used_source, Some("global_cache"));
    // Age-threshold purge selection (what `gc list` JSON consumers
    // and age-gated purge passes do): the active checkout must NOT
    // be selected; the genuinely stale one still must be.
    let selected: Vec<&str> = entries
        .iter()
        .filter(|e| e.age_seconds > threshold_seconds)
        .map(|e| e.path.as_str())
        .collect();
    assert!(
        !selected.iter().any(|p| p.contains("dep-76b10f3504cf35a4")),
        "actively-used git checkout was purge-selected: {selected:?}"
    );
    assert!(
        selected
            .iter()
            .any(|p| p.contains("olddep-aabbccddeeff0011")),
        "genuinely stale git checkout must remain purge-selected"
    );
    assert_eq!(stale.last_used_source, Some("global_cache"));
}

#[test]
fn git_checkout_walk_falls_back_to_mtime_when_db_garbled() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo_home = tmp.path();
    let now: i64 = 2_000_000_000;
    let old_mtime = now - 100 * 86_400;

    make_git_checkout_dir(cargo_home, "dep-76b10f3504cf35a4", "3381ba4", old_mtime);
    // Not a SQLite database at all — the reader must fail closed and
    // the walker must fall back to mtime, never error out.
    std::fs::write(cargo_home.join(".global-cache"), b"definitely not sqlite").unwrap();

    let entries = walk_cargo_git_checkouts(cargo_home, now);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].last_used_unix, old_mtime);
    assert_eq!(entries[0].last_used_source, Some("fs_mtime"));
}

#[test]
fn git_checkout_walk_falls_back_to_mtime_when_row_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo_home = tmp.path();
    let now: i64 = 2_000_000_000;
    let old_mtime = now - 100 * 86_400;

    make_git_checkout_dir(cargo_home, "dep-76b10f3504cf35a4", "3381ba4", old_mtime);
    // Tracker exists with valid schema but has no row for this
    // checkout — the fallback must be per-checkout.
    let conn = make_global_cache_git_db(cargo_home);
    insert_git_checkout_row(&conn, "other-1122334455667788", "abc1234", now);
    drop(conn);

    let entries = walk_cargo_git_checkouts(cargo_home, now);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].last_used_unix, old_mtime);
    assert_eq!(entries[0].last_used_source, Some("fs_mtime"));
}

// -------------------------------------------------------------------
// End-to-end registry-src walk against a REAL-schema `.global-cache`
// fixture (issue #1569). Before the fix, the reader queried a
// nonexistent `registry_src.version` column, errored on every real
// cargo database, and silently degraded to the mtime fallback — so an
// actively-used source with an old dir mtime was purge-selected.
// -------------------------------------------------------------------

/// Synthesize `$CARGO_HOME/registry/src/<registry>/<crate>-<vers>/`
/// with a controlled directory mtime. Returns the crate dir path.
fn make_registry_src_dir(
    cargo_home: &std::path::Path,
    registry_dir: &str,
    crate_dir: &str,
    mtime_unix: i64,
) -> std::path::PathBuf {
    let crate_path = cargo_home
        .join("registry")
        .join("src")
        .join(registry_dir)
        .join(crate_dir);
    std::fs::create_dir_all(&crate_path).unwrap();
    std::fs::write(crate_path.join("lib.rs"), b"pub fn f() {}").unwrap();
    // Set the mtime last — writing the file above touches the dir.
    let ft = filetime::FileTime::from_unix_time(mtime_unix, 0);
    filetime::set_file_mtime(&crate_path, ft).unwrap();
    crate_path
}

/// Create a `.global-cache` SQLite DB with the exact registry schema
/// cargo 1.94 writes (CREATE TABLE statements copied verbatim from a
/// live cargo-produced DB): `registry_src.name` is the
/// `<crate>-<version>` dirname; there is no `version` column.
fn make_global_cache_registry_db(cargo_home: &std::path::Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(cargo_home.join(".global-cache")).unwrap();
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
    conn
}

fn insert_registry_src_row(
    conn: &rusqlite::Connection,
    registry_dir: &str,
    crate_dir: &str,
    ts: i64,
) {
    conn.execute(
        "INSERT OR IGNORE INTO registry_index (name, timestamp) VALUES (?, ?)",
        rusqlite::params![registry_dir, ts],
    )
    .unwrap();
    let registry_id: i64 = conn
        .query_row(
            "SELECT id FROM registry_index WHERE name = ?",
            rusqlite::params![registry_dir],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO registry_src (registry_id, name, size, timestamp) VALUES (?, ?, NULL, ?)",
        rusqlite::params![registry_id, crate_dir, ts],
    )
    .unwrap();
}

#[test]
fn active_registry_src_with_fresh_db_usage_is_not_purge_selected() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo_home = tmp.path();
    let now: i64 = 2_000_000_000;
    let threshold_seconds: i64 = 30 * 86_400;
    let old_mtime = now - 100 * 86_400;
    let fresh_ts = now - 3_600;
    let registry = "index.crates.io-1949cf8c6b5b557f";

    // Active source: dir mtime 100 days old (set at extraction
    // time), but cargo's tracker says it was used an hour ago.
    make_registry_src_dir(cargo_home, registry, "serde-1.0.219", old_mtime);
    // Genuinely stale source: old mtime AND old tracker row.
    make_registry_src_dir(cargo_home, registry, "oldcrate-0.1.0", old_mtime);
    let conn = make_global_cache_registry_db(cargo_home);
    insert_registry_src_row(&conn, registry, "serde-1.0.219", fresh_ts);
    insert_registry_src_row(&conn, registry, "oldcrate-0.1.0", now - 200 * 86_400);
    drop(conn);

    // Read-only guarantee: the walk must leave cargo's database
    // byte-identical.
    let db_before = std::fs::read(cargo_home.join(".global-cache")).unwrap();
    let entries = walk_cargo_registry_src(cargo_home, now);
    let db_after = std::fs::read(cargo_home.join(".global-cache")).unwrap();
    assert_eq!(db_before, db_after, "gc walk mutated cargo's .global-cache");

    assert_eq!(entries.len(), 2);
    let active = entries
        .iter()
        .find(|e| e.path.contains("serde-1.0.219"))
        .unwrap();
    let stale = entries
        .iter()
        .find(|e| e.path.contains("oldcrate-0.1.0"))
        .unwrap();

    // The active source must surface cargo's usage recency, not
    // the extraction-time mtime.
    assert_eq!(active.last_used_unix, fresh_ts);
    assert_eq!(active.last_used_source, Some("global_cache"));
    // Age-threshold purge selection: the active source must NOT
    // be selected; the genuinely stale one still must be.
    let selected: Vec<&str> = entries
        .iter()
        .filter(|e| e.age_seconds > threshold_seconds)
        .map(|e| e.path.as_str())
        .collect();
    assert!(
        !selected.iter().any(|p| p.contains("serde-1.0.219")),
        "actively-used registry source was purge-selected: {selected:?}"
    );
    assert!(
        selected.iter().any(|p| p.contains("oldcrate-0.1.0")),
        "genuinely stale registry source must remain purge-selected"
    );
    assert_eq!(stale.last_used_source, Some("global_cache"));
}

// soldr#2199: `gc target --purge` counted an already-missing directory as a
// failure, which inflates failed_count and makes the command exit non-zero
// for work that is already done. GC deletes from a plan built earlier in the
// run, so the window is real.
#[test]
fn remove_target_dir_treats_a_missing_directory_as_success() {
    let temp = tempfile::tempdir().expect("tempdir");
    super::remove_target_dir(&temp.path().join("already-gone"))
        .expect("an absent target/ is not a purge failure");
}

#[test]
fn remove_target_dir_still_removes_a_real_tree() {
    let temp = tempfile::tempdir().expect("tempdir");
    let target = temp.path().join("target");
    std::fs::create_dir_all(target.join("debug")).expect("mkdir");
    std::fs::write(target.join("debug/blob.bin"), b"x").expect("write");

    super::remove_target_dir(&target).expect("a real tree must still be removed");
    assert!(!target.exists(), "tree survived: {}", target.display());
}

// soldr#2134: the block-tier reclaim runs synchronously in front of a build
// that is already blocked, so it must be bounded. The budget itself is the
// contract; a value large enough to be indistinguishable from unbounded on a
// dirty volume would silently reintroduce the stall.
#[test]
fn the_block_tier_prune_budget_is_bounded_and_plausible() {
    let budget = super::auto::BLOCK_TIER_PRUNE_BUDGET;
    assert!(
        budget >= std::time::Duration::from_secs(5),
        "too short to remove even one large target/: {budget:?}",
    );
    assert!(
        budget <= std::time::Duration::from_secs(120),
        "a budget this large is the unbounded stall soldr#2134 asked to avoid: {budget:?}",
    );
}
