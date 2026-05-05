//! `sqlite-vec` extension wired up via the Rust crate (vendored, statically
//! linked — no separate `.dll`/`.so` to ship).
//!
//! Strategy: register the `sqlite_vec_init` C function as an auto-extension
//! exactly once at process start, BEFORE any `Connection::open` call. Every
//! subsequent connection then has the `vec0` virtual table and `vec_*`
//! functions available.
//!
//! The auto-extension has process-global state, so we guard registration
//! with a `OnceLock` and tolerate repeated calls (each `run()` /
//! `run_mcp_stdio()` calls `ensure_loaded()` defensively).

use std::sync::OnceLock;

use rusqlite::ffi::sqlite3_auto_extension;
use sqlite_vec::sqlite3_vec_init;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VecExtensionStatus {
    Loaded,
    Failed,
}

static REGISTERED: OnceLock<VecExtensionStatus> = OnceLock::new();

/// Idempotently register `sqlite-vec` as a SQLite auto-extension. Returns
/// the result of the first registration; subsequent calls are no-ops.
pub fn ensure_loaded() -> VecExtensionStatus {
    *REGISTERED.get_or_init(|| {
        // SAFETY: `sqlite3_vec_init` is a C function pointer with the
        // SQLite extension entry-point signature. The `transmute` casts it
        // to the no-arg `unsafe extern "C" fn()` shape that
        // `sqlite3_auto_extension` declares — SQLite invokes the entry
        // point itself with the proper arguments at connection time.
        let rc = unsafe {
            // Cast `sqlite3_vec_init`'s real signature to the
            // `xEntryPoint`-shaped pointer that `sqlite3_auto_extension`
            // wants. Both signatures are `unsafe extern "C"` and SQLite
            // invokes the entry point with the real arguments at
            // connection-open time, so the cast is sound.
            #[allow(clippy::missing_transmute_annotations)]
            sqlite3_auto_extension(Some(std::mem::transmute(sqlite3_vec_init as *const ())))
        };
        if rc == 0 {
            tracing::info!("sqlite-vec auto-extension registered");
            VecExtensionStatus::Loaded
        } else {
            tracing::warn!(
                rc,
                "sqlite-vec auto-extension registration returned non-zero; KNN sub-queries will be unavailable"
            );
            VecExtensionStatus::Failed
        }
    })
}

/// True when the current process can use the `vec0` virtual table.
pub fn is_loaded() -> bool {
    matches!(REGISTERED.get().copied(), Some(VecExtensionStatus::Loaded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_loaded_returns_loaded_when_called_in_a_test_process() {
        let status = ensure_loaded();
        assert_eq!(status, VecExtensionStatus::Loaded);
        assert!(is_loaded());
    }

    fn f32s_to_le_bytes(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    #[test]
    fn vec0_virtual_table_can_be_created_after_ensure_loaded() {
        ensure_loaded();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE temp_vecs USING vec0(embedding float[4]);")
            .expect("vec0 virtual table must be creatable");
        conn.execute(
            "INSERT INTO temp_vecs(rowid, embedding) VALUES (1, ?1)",
            rusqlite::params![f32s_to_le_bytes(&[0.1, 0.2, 0.3, 0.4])],
        )
        .expect("vec0 insert must succeed");
        let count: i64 = conn
            .query_row("SELECT count(*) FROM temp_vecs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn vec_distance_cosine_returns_zero_for_identical_vectors() {
        ensure_loaded();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let v: f64 = conn
            .query_row(
                "SELECT vec_distance_cosine(?1, ?2)",
                rusqlite::params![
                    f32s_to_le_bytes(&[1.0, 0.0, 0.0, 0.0]),
                    f32s_to_le_bytes(&[1.0, 0.0, 0.0, 0.0]),
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert!(v.abs() < 1e-5);
    }

    #[test]
    fn vec_knn_match_returns_nearest_first() {
        ensure_loaded();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE v USING vec0(e float[4]);")
            .unwrap();
        conn.execute(
            "INSERT INTO v(rowid, e) VALUES (1, ?1)",
            rusqlite::params![f32s_to_le_bytes(&[1.0, 0.0, 0.0, 0.0])],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO v(rowid, e) VALUES (2, ?1)",
            rusqlite::params![f32s_to_le_bytes(&[0.0, 1.0, 0.0, 0.0])],
        )
        .unwrap();
        let row: i64 = conn
            .query_row(
                "SELECT rowid FROM v WHERE e MATCH ?1 ORDER BY distance LIMIT 1",
                rusqlite::params![f32s_to_le_bytes(&[0.99, 0.01, 0.0, 0.0])],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(row, 1);
    }
}
