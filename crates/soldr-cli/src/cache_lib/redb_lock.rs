//! Process-wide serialization for opening the soldr `state.redb` file.
//!
//! redb refuses concurrent in-process `Database::open` against the same
//! file with `Database already open. Cannot acquire lock.`. The daemon's
//! per-request handlers each call `open_db` on `state.redb`, so two
//! handlers landing on different tokio worker threads can race and one
//! of them silently fails — see issue #608 for the regression this
//! mutex fixes (`db::set_linked_zccache` silently dropped writes when a
//! concurrent `Status` request had the same file open via
//! `cook_index::stats`).
//!
//! The lock is held for the full lifetime of the redb [`Database`]
//! handle, not just the open call: the file lock redb holds is released
//! when the `Database` is dropped, so callers must keep this guard alive
//! at least that long. The [`StateDbHandle`] wrapper enforces drop order
//! by declaring `db` before `_guard`.

use redb::Database;
use std::ops::Deref;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Shared global lock guarding every in-process `Database::open` on
/// `state.redb`. Both `daemon::db` and `cache_lib::cook_index` go
/// through this lock — they open the same file.
pub(crate) fn state_db_open_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Owns a redb [`Database`] handle together with the
/// [`state_db_open_lock`] guard that gates concurrent opens. Field
/// order is load-bearing: `db` is declared first so it is dropped
/// (releasing redb's file lock) before `_guard` is dropped (letting
/// the next opener proceed).
pub(crate) struct StateDbHandle {
    db: Database,
    _guard: MutexGuard<'static, ()>,
}

impl StateDbHandle {
    pub(crate) fn new(db: Database, guard: MutexGuard<'static, ()>) -> Self {
        Self { db, _guard: guard }
    }
}

impl Deref for StateDbHandle {
    type Target = Database;
    fn deref(&self) -> &Database {
        &self.db
    }
}
