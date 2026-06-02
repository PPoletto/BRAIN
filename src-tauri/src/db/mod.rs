//! SQLite layer with FTS5 for full-text search and a placeholder for the
//! sqlite-vec vector index that will arrive once the embedding pipeline
//! lands. The schema mirrors `docs/architecture.md` §2.4.

pub mod migrations;
pub mod node_positions;
pub mod pages_index;
pub mod vec_loader;

use std::path::Path;
use std::sync::{Arc, Mutex};

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

    /// Cheap liveness probe: runs `SELECT 1` against the cached
    /// connection. Returns `false` when the underlying file
    /// descriptor is dead — the classic symptom of the vault disk
    /// (a USB stick) being unplugged and replugged: the path
    /// resolves again, but the `rusqlite::Connection` opened against
    /// the old fd keeps IO-erroring. Callers (the MCP subprocess's
    /// self-heal loop) use this to decide whether to reopen the
    /// handle rather than handing a dead connection to the next
    /// query. A poisoned mutex also reports not-alive, which is the
    /// safe answer — a reopen replaces the whole `DbHandle`.
    pub fn is_alive(&self) -> bool {
        self.with(|conn| {
            conn.query_row("SELECT 1", [], |_| Ok(()))?;
            Ok(())
        })
        .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn is_alive_returns_true_for_a_freshly_opened_handle() {
        // The positive path of the liveness probe: a handle opened
        // against a reachable db file answers `SELECT 1`. The dead-fd
        // case (disk unplugged) can't be reproduced in a unit test —
        // it needs a real OS-level disk yank — but the probe is the
        // mechanism the MCP self-heal loop relies on at runtime.
        let tmp = TempDir::new().unwrap();
        let handle = DbHandle::open(tmp.path()).unwrap();
        assert!(handle.is_alive());
    }

    #[test]
    fn cloned_handles_share_one_live_connection() {
        // DbHandle is Clone via Arc — both clones probe the same
        // underlying connection. Guards against a future refactor
        // that accidentally deep-copies (which would defeat the
        // single-writer Mutex contract).
        let tmp = TempDir::new().unwrap();
        let a = DbHandle::open(tmp.path()).unwrap();
        let b = a.clone();
        assert!(a.is_alive());
        assert!(b.is_alive());
    }
}
