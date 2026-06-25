//! SQLite layer with FTS5 for full-text search and a placeholder for the
//! sqlite-vec vector index that will arrive once the embedding pipeline
//! lands. The schema mirrors `docs/architecture.md` §2.4.

pub mod migrations;
pub mod node_positions;
pub mod pages_index;
pub mod vec_loader;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use thiserror::Error;

use crate::vault::layout::db_dir;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("rusqlite: {0}")]
    Rusqlite(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type DbResult<T> = Result<T, DbError>;

/// Returned by [`DbHandle::with_timeout`] when the database operation did
/// not finish within the allotted time. The defining symptom we protect
/// against: a stale file handle on the vault disk (USB unplug, resume
/// from standby, a network mount that went away) where the underlying
/// read/write syscall blocks in the kernel indefinitely rather than
/// returning `SQLITE_IOERR`. SQLite's own `busy_timeout` does NOT cover
/// this — it only bounds lock contention, not a wedged I/O syscall — so
/// we bound it ourselves by running the op on a worker thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("db operation timed out")]
pub struct DbTimeout;

/// True when `err` indicates the connection itself is dead and must be
/// reopened (rather than a logical error in the SQL we ran). We reopen
/// only on these codes so a genuine bug (bad SQL, missing table,
/// constraint violation) does not trigger an endless reopen+retry loop:
///
///   - `SystemIoFailure` (`SQLITE_IOERR`) — the classic stale-handle
///     symptom after a disk unplug / resume.
///   - `NotADatabase` (`SQLITE_NOTADB`) — the file came back but its
///     header read garbage (half-remounted volume).
///   - `CannotOpen` (`SQLITE_CANTOPEN`) — the path resolved but the
///     file could not be opened (transient during remount).
pub fn is_connection_fatal(err: &DbError) -> bool {
    match err {
        DbError::Rusqlite(rusqlite::Error::SqliteFailure(e, _)) => matches!(
            e.code,
            rusqlite::ErrorCode::SystemIoFailure
                | rusqlite::ErrorCode::NotADatabase
                | rusqlite::ErrorCode::CannotOpen
        ),
        _ => false,
    }
}

pub const DB_FILENAME: &str = "brain.db";

/// Thread-safe handle around a single SQLite connection. SQLite is
/// process-local; for MVP we serialize writes through a Mutex which is
/// plenty for personal-scale wiki sizes.
#[derive(Clone)]
pub struct DbHandle {
    inner: Arc<Mutex<Connection>>,
}

impl DbHandle {
    pub fn open(vault: &Path) -> DbResult<Self> {
        let dir = db_dir(vault);
        std::fs::create_dir_all(&dir)?;
        // Register sqlite-vec as a process-wide auto-extension BEFORE the
        // first Connection::open call — auto-extensions only fire on
        // connections opened *after* registration. This matters for unit
        // tests that don't go through `lib::run` first.
        vec_loader::ensure_loaded();
        let path = dir.join(DB_FILENAME);
        let conn = Connection::open(&path)?;
        // Enable WAL for crash safety + better concurrent reads.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Wait up to 5 s for a lock held by the GUI writer instead of
        // failing instantly with SQLITE_BUSY. Orthogonal to the
        // stale-handle hang (that needs `with_timeout`), but it stops
        // spurious BUSY errors when the GUI is mid-commit on the WAL.
        // Kept strictly below the MCP `DB_OP_TIMEOUT` (8 s) so a
        // legitimate lock wait is never misread as a wedged disk.
        conn.busy_timeout(Duration::from_millis(5_000))?;
        migrations::apply(&conn)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn with<F, T>(&self, f: F) -> DbResult<T>
    where
        F: FnOnce(&Connection) -> DbResult<T>,
    {
        let guard = self
            .inner
            .lock()
            .map_err(|_| DbError::Io(std::io::Error::other("db lock poisoned")))?;
        f(&guard)
    }

    /// Runs `f` against the connection on a short-lived worker thread,
    /// returning `Err(DbTimeout)` if it does not complete within
    /// `timeout`. This is the only thing that bounds a *wedged* I/O
    /// syscall (stale handle after a disk unplug / resume): SQLite's
    /// `busy_timeout` bounds lock waits, not a kernel read that never
    /// returns, and a synchronous `with()` on a dead handle blocks the
    /// caller forever.
    ///
    /// The outer `Result` is the timeout boundary; the inner
    /// `DbResult<T>` is the operation's own success/failure. So:
    /// - `Ok(Ok(v))` — completed, succeeded.
    /// - `Ok(Err(e))` — completed, but the SQL errored (inspect with
    ///   [`is_connection_fatal`] to decide reopen).
    /// - `Err(DbTimeout)` — did not finish in time.
    ///
    /// On timeout the worker thread is left running (it is blocked in
    /// the kernel and cannot be cancelled) and is therefore detached —
    /// it holds the `Arc<Mutex<Connection>>` lock of *this* handle until
    /// its syscall finally returns. **The caller MUST NOT touch this
    /// handle again after a timeout**: drop it (`*db = None`) so the next
    /// operation opens a brand-new connection. Re-locking the abandoned
    /// handle would block on the still-held lock (a live lock, not a
    /// `PoisonError`). One orphaned thread leaks per timeout event; it
    /// exits cleanly once the OS finally errors the syscall (e.g. the
    /// mount-timeout fires). For a personal-scale local server this is a
    /// rare, self-clearing event — see the MCP `db_op` policy.
    pub fn with_timeout<F, T>(&self, timeout: Duration, f: F) -> Result<DbResult<T>, DbTimeout>
    where
        F: FnOnce(&Connection) -> DbResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let handle = self.clone(); // Arc bump — worker holds its own ref.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = handle.with(f);
            // Send can fail only if the main thread already timed out and
            // dropped the receiver — that's the orphaned-thread case, and
            // there's nothing to deliver to, so ignore it.
            let _ = tx.send(result);
        });
        match rx.recv_timeout(timeout) {
            Ok(result) => Ok(result),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(DbTimeout),
            // Worker panicked before sending — surface as an internal
            // error rather than a timeout so the caller doesn't reopen.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(Err(DbError::Io(
                std::io::Error::other("db worker thread terminated unexpectedly"),
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn with_timeout_returns_the_result_for_a_fast_operation() {
        let tmp = TempDir::new().unwrap();
        let handle = DbHandle::open(tmp.path()).unwrap();
        let out: Result<DbResult<i64>, DbTimeout> =
            handle.with_timeout(Duration::from_secs(5), |conn| {
                Ok(conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?)
            });
        assert_eq!(out.unwrap().unwrap(), 1);
    }

    #[test]
    fn with_timeout_reports_timeout_when_the_operation_overruns() {
        // Deterministic stand-in for a wedged kernel syscall: the op
        // sleeps past the deadline. Proves recv_timeout fires and the
        // caller is handed back control promptly instead of blocking.
        let tmp = TempDir::new().unwrap();
        let handle = DbHandle::open(tmp.path()).unwrap();
        let out: Result<DbResult<()>, DbTimeout> =
            handle.with_timeout(Duration::from_millis(50), |_conn| {
                std::thread::sleep(Duration::from_secs(2));
                Ok(())
            });
        // `matches!` rather than `assert_eq!` — the Ok side carries a
        // `DbError` which is not `PartialEq`.
        assert!(matches!(out, Err(DbTimeout)));
    }

    #[test]
    fn a_fresh_handle_works_after_a_prior_handle_timed_out() {
        // The abandon-and-reopen contract: after a timeout the caller
        // drops the wedged handle and opens a new one. The new handle
        // must be fully usable — we never re-lock the abandoned mutex.
        let tmp = TempDir::new().unwrap();
        let stale = DbHandle::open(tmp.path()).unwrap();
        let _ = stale.with_timeout(Duration::from_millis(50), |_conn| {
            std::thread::sleep(Duration::from_secs(1));
            Ok::<(), DbError>(())
        });
        // Simulate `*db = None` + reopen.
        drop(stale);
        let fresh = DbHandle::open(tmp.path()).unwrap();
        let out: Result<DbResult<i64>, DbTimeout> =
            fresh.with_timeout(Duration::from_secs(5), |conn| {
                Ok(conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?)
            });
        assert_eq!(out.unwrap().unwrap(), 1);
    }

    #[test]
    fn is_connection_fatal_classifies_io_and_notadb_as_fatal_but_not_logic_errors() {
        use rusqlite::ffi::{Error as FfiError, ErrorCode};
        let ioerr = DbError::Rusqlite(rusqlite::Error::SqliteFailure(
            FfiError {
                code: ErrorCode::SystemIoFailure,
                extended_code: 0,
            },
            Some("disk I/O error".into()),
        ));
        let notadb = DbError::Rusqlite(rusqlite::Error::SqliteFailure(
            FfiError {
                code: ErrorCode::NotADatabase,
                extended_code: 0,
            },
            None,
        ));
        let cantopen = DbError::Rusqlite(rusqlite::Error::SqliteFailure(
            FfiError {
                code: ErrorCode::CannotOpen,
                extended_code: 0,
            },
            None,
        ));
        // A constraint violation is a logic error, NOT a dead connection.
        let constraint = DbError::Rusqlite(rusqlite::Error::SqliteFailure(
            FfiError {
                code: ErrorCode::ConstraintViolation,
                extended_code: 0,
            },
            None,
        ));
        assert!(is_connection_fatal(&ioerr));
        assert!(is_connection_fatal(&notadb));
        assert!(is_connection_fatal(&cantopen));
        assert!(!is_connection_fatal(&constraint));
        // A non-rusqlite error is never a reopen trigger.
        assert!(!is_connection_fatal(&DbError::Io(std::io::Error::other("x"))));
    }

    #[test]
    fn open_sets_busy_timeout_to_five_seconds() {
        let tmp = TempDir::new().unwrap();
        let handle = DbHandle::open(tmp.path()).unwrap();
        let ms: i64 = handle
            .with(|conn| Ok(conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(ms, 5000);
    }
}
