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
}
