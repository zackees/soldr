//! Filtered crash history and cross-class rollups (S12 / #641).
//!
//! Read-only over the S10 crash store. Two questions an operator actually
//! asks, answered separately:
//!
//! - "show me the recent `clud` crashes" — [`CrashStore::query`], a bounded,
//!   newest-first page of records.
//! - "how often does this crash, and since when" — [`CrashStore::stats`], a
//!   group-by-signature rollup.
//!
//! They are separate because `limit` truncates. A caller who counted rows out
//! of a limited page would report "10 crashes" for any bucket with more than
//! ten, and would report it confidently. [`CrashStats::total`] is computed by
//! the database over the whole match set, so it is the real number.
//!
//! # Redaction
//!
//! Everything returned here is redacted metadata: class, name, version,
//! instance, pid, signature, timing, fault kind, size. Never the inline
//! report JSON, never an environment, and never the artifact *path* — that
//! discloses the owner's directory layout, so callers get the opaque row id
//! and fetch bytes through the artifact endpoint, which resolves the id
//! itself. See [`crate::crash_store::CrashStore::begin_fetch`].

use std::num::NonZeroU32;

use rusqlite::{Connection, ToSql};

use crate::crash_store::{CrashRecord, CrashStore, CrashStoreError};

/// Largest page of crash records one query may return.
///
/// The store retains up to `DEFAULT_MAX_ROWS` (10k) rows, and a caller that
/// asked for all of them would materialize every one in daemon memory and
/// then in a single reply frame. Paging is by time window.
pub const MAX_CRASH_LIMIT: u32 = 1024;

/// Longest accepted LIKE pattern or exact-match string.
pub const MAX_FILTER_BYTES: usize = 512;

/// Escape character used with every `LIKE` in this module.
///
/// Without it, a class literally containing `%` or `_` would match far more
/// than the caller wrote — `_` alone is a single-character wildcard, so the
/// perfectly ordinary class `my_app` would also match `myXapp`.
const LIKE_ESCAPE: char = '\\';

/// Why a crash query was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CrashQueryError {
    /// `limit` was absent or zero.
    #[error("crash query requires a non-zero limit")]
    MissingLimit,
    /// `limit` exceeded [`MAX_CRASH_LIMIT`].
    #[error("crash query limit {actual} exceeds the maximum of {max}")]
    LimitTooLarge {
        /// Requested limit.
        actual: u32,
        /// Permitted limit.
        max: u32,
    },
    /// A filter string exceeded [`MAX_FILTER_BYTES`].
    #[error("crash query {field} filter is {actual} bytes; maximum is {max}")]
    FilterTooLong {
        /// Which filter was too long.
        field: &'static str,
        /// Observed length.
        actual: usize,
        /// Permitted length.
        max: usize,
    },
    /// The time window was empty or inverted.
    #[error("crash query window is empty: since {since} is not before until {until}")]
    EmptyWindow {
        /// Inclusive lower bound.
        since: u64,
        /// Exclusive upper bound.
        until: u64,
    },
}

/// Which crashes a query is about.
///
/// Every field is optional and they combine with AND. An all-default filter
/// (apart from `limit`) matches the whole retained history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CrashFilter {
    /// Exact application class.
    pub app_class: Option<String>,
    /// `LIKE` pattern on application class, e.g. `clud%`.
    pub app_class_like: Option<String>,
    /// Exact application name.
    pub app_name: Option<String>,
    /// Exact instance name.
    pub instance_name: Option<String>,
    /// Exact crash signature.
    pub signature: Option<String>,
    /// Inclusive lower bound on crash time, in unix milliseconds.
    pub since_unix_ms: Option<u64>,
    /// Exclusive upper bound on crash time, in unix milliseconds.
    pub until_unix_ms: Option<u64>,
}

impl CrashFilter {
    /// Check the filter's own bounds.
    ///
    /// Called by both [`CrashStore::query`] and [`CrashStore::stats`], because
    /// a rollup over an unbounded string filter is just as expensive as a page
    /// of one.
    pub fn validate(&self) -> Result<(), CrashQueryError> {
        let fields: [(&'static str, Option<&String>); 5] = [
            ("app_class", self.app_class.as_ref()),
            ("app_class_like", self.app_class_like.as_ref()),
            ("app_name", self.app_name.as_ref()),
            ("instance_name", self.instance_name.as_ref()),
            ("signature", self.signature.as_ref()),
        ];
        for (field, value) in fields {
            if let Some(value) = value {
                if value.len() > MAX_FILTER_BYTES {
                    return Err(CrashQueryError::FilterTooLong {
                        field,
                        actual: value.len(),
                        max: MAX_FILTER_BYTES,
                    });
                }
            }
        }
        // An inverted window is a caller bug that would otherwise return an
        // empty set indistinguishable from "nothing crashed", which is the
        // single most misleading answer this surface can give.
        if let (Some(since), Some(until)) = (self.since_unix_ms, self.until_unix_ms) {
            if since >= until {
                return Err(CrashQueryError::EmptyWindow { since, until });
            }
        }
        Ok(())
    }

    /// Render the shared `WHERE` clause and its bound parameters.
    ///
    /// Every value is a bound parameter. Nothing user-supplied is ever
    /// concatenated into the SQL — only the fixed clause fragments below are,
    /// and those are compile-time literals.
    fn where_clause(&self) -> (String, Vec<Box<dyn ToSql>>) {
        let mut sql = String::from(" WHERE 1=1");
        let mut params: Vec<Box<dyn ToSql>> = Vec::new();

        if let Some(value) = &self.app_class {
            sql.push_str(" AND app_class = ?");
            params.push(Box::new(value.clone()));
        }
        if let Some(pattern) = &self.app_class_like {
            sql.push_str(" AND app_class LIKE ? ESCAPE '\\'");
            params.push(Box::new(pattern.clone()));
        }
        if let Some(value) = &self.app_name {
            sql.push_str(" AND app_name = ?");
            params.push(Box::new(value.clone()));
        }
        if let Some(value) = &self.instance_name {
            sql.push_str(" AND instance_name = ?");
            params.push(Box::new(value.clone()));
        }
        if let Some(value) = &self.signature {
            sql.push_str(" AND signature = ?");
            params.push(Box::new(value.clone()));
        }
        if let Some(since) = self.since_unix_ms {
            sql.push_str(" AND crashed_at_ms >= ?");
            params.push(Box::new(clamp_to_i64(since)));
        }
        if let Some(until) = self.until_unix_ms {
            sql.push_str(" AND crashed_at_ms < ?");
            params.push(Box::new(clamp_to_i64(until)));
        }
        (sql, params)
    }
}

/// SQLite integers are signed, and every timestamp this store writes fits.
///
/// A caller supplying a millisecond value above `i64::MAX` means "no upper
/// bound in practice", so saturating is the honest translation — the
/// alternative, wrapping to a negative bound, would match nothing.
fn clamp_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Escape `%`, `_`, and the escape character itself for use in a `LIKE`.
///
/// Use this on a user-supplied *prefix*; do not use it on a pattern the caller
/// deliberately wrote as a wildcard.
pub fn escape_like(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    for ch in literal.chars() {
        if ch == '%' || ch == '_' || ch == LIKE_ESCAPE {
            out.push(LIKE_ESCAPE);
        }
        out.push(ch);
    }
    out
}

/// One crash signature and how it behaved inside the queried window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureStat {
    /// The crash signature.
    pub signature: String,
    /// How many matching crashes carried it.
    pub count: u64,
    /// Earliest matching occurrence, unix milliseconds.
    pub first_unix_ms: u64,
    /// Latest matching occurrence, unix milliseconds.
    pub last_unix_ms: u64,
    /// Distinct application classes that produced it, sorted.
    ///
    /// A signature spanning classes usually means shared library code, which
    /// is the most useful thing a rollup can tell an operator.
    pub app_classes: Vec<String>,
}

/// A rollup over everything a [`CrashFilter`] matched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CrashStats {
    /// Per-signature rollups, most frequent first.
    pub signatures: Vec<SignatureStat>,
    /// Total matching crashes. Not the sum of a truncated record page.
    pub total: u64,
    /// Earliest matching crash, or zero when there are none.
    pub first_unix_ms: u64,
    /// Latest matching crash, or zero when there are none.
    pub last_unix_ms: u64,
    /// Distinct application classes among matches.
    pub distinct_classes: u64,
}

/// The record columns a query returns.
///
/// `report_json` is deliberately absent — it is the inline crash report, and
/// it is what the redaction rule exists to keep off this surface.
const RECORD_COLUMNS: &str = "id, app_class, app_name, app_version, instance_name, pid, \
                              creation_time_ms, cwd, signature, crashed_at_ms, exit_signal, \
                              artifact_path, artifact_bytes";

impl CrashStore {
    /// Page through crash history, newest first.
    ///
    /// `limit` is mandatory: crash history grows without bound between GC
    /// runs, so an absent limit would mean "return whatever has accumulated",
    /// which is not a size any caller has reasoned about.
    pub fn query(
        &self,
        filter: &CrashFilter,
        limit: NonZeroU32,
    ) -> Result<Vec<CrashRecord>, CrashStoreError> {
        filter.validate()?;
        if limit.get() > MAX_CRASH_LIMIT {
            return Err(CrashQueryError::LimitTooLarge {
                actual: limit.get(),
                max: MAX_CRASH_LIMIT,
            }
            .into());
        }

        let (where_clause, mut params) = filter.where_clause();
        // `id DESC` breaks ties so two crashes recorded in the same
        // millisecond come back in a stable order. Without it the page a
        // caller sees could differ between identical queries.
        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM crashes{where_clause} \
             ORDER BY crashed_at_ms DESC, id DESC LIMIT ?"
        );
        params.push(Box::new(i64::from(limit.get())));

        let conn = self.connection();
        let conn = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map(
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
            |row| crate::crash_store::row_to_record(row, self.artifacts_dir()),
        )?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Roll up crash history by signature.
    ///
    /// No row limit: the result is bounded by the number of distinct
    /// signatures, not by the number of crashes, and collapsing a million
    /// crashes into a handful of buckets is the entire point.
    pub fn stats(&self, filter: &CrashFilter) -> Result<CrashStats, CrashStoreError> {
        filter.validate()?;
        let conn = self.connection();
        let conn = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        stats_on(&conn, filter)
    }
}

/// Run the rollup against an open connection.
fn stats_on(conn: &Connection, filter: &CrashFilter) -> Result<CrashStats, CrashStoreError> {
    let (where_clause, params) = filter.where_clause();

    let totals_sql = format!(
        "SELECT COUNT(*), COALESCE(MIN(crashed_at_ms), 0), COALESCE(MAX(crashed_at_ms), 0), \
         COUNT(DISTINCT app_class) FROM crashes{where_clause}"
    );
    let (total, first_unix_ms, last_unix_ms, distinct_classes): (i64, i64, i64, i64) = conn
        .query_row(
            &totals_sql,
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;

    // `group_concat` with DISTINCT gives the classes per signature in one
    // pass. A second query per signature would be N+1 round-trips against a
    // rollup whose whole purpose is to be cheap.
    let grouped_sql = format!(
        "SELECT signature, COUNT(*), MIN(crashed_at_ms), MAX(crashed_at_ms), \
         GROUP_CONCAT(DISTINCT app_class) FROM crashes{where_clause} \
         GROUP BY signature ORDER BY COUNT(*) DESC, signature ASC"
    );
    let mut statement = conn.prepare(&grouped_sql)?;
    let rows = statement.query_map(
        rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
        |row| {
            let classes: Option<String> = row.get(4)?;
            let mut app_classes: Vec<String> = classes
                .unwrap_or_default()
                .split(',')
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .collect();
            app_classes.sort_unstable();
            app_classes.dedup();
            Ok(SignatureStat {
                signature: row.get(0)?,
                count: row.get::<_, i64>(1)?.max(0) as u64,
                first_unix_ms: row.get::<_, i64>(2)?.max(0) as u64,
                last_unix_ms: row.get::<_, i64>(3)?.max(0) as u64,
                app_classes,
            })
        },
    )?;
    let signatures = rows.collect::<Result<Vec<_>, _>>()?;

    Ok(CrashStats {
        signatures,
        total: total.max(0) as u64,
        first_unix_ms: first_unix_ms.max(0) as u64,
        last_unix_ms: last_unix_ms.max(0) as u64,
        distinct_classes: distinct_classes.max(0) as u64,
    })
}

#[cfg(test)]
mod tests;
