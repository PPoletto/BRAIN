//! Persistent graph node coordinates so the viewer's Tier-3 graph
//! re-opens with the user's last-arranged layout instead of running
//! the fcose force-directed pass every time. Three operations:
//!
//!  - `save`: bulk upsert of `(page_id, x, y)` triples. Called when
//!    the user drags a node and after the initial fcose run completes
//!    (so the next mount goes straight to a `preset` layout).
//!  - `load`: read all stored positions. Empty vec on first run.
//!  - `clear`: wipe all positions, used by the Tier-3 "Re-layout"
//!    button so a single click reverts to fcose.
//!
//! The table is created in migration v4 (see `migrations.rs`).
//! Stale rows (positions for pages that no longer exist on disk)
//! cost almost nothing — the loader filters them out by id-existence
//! at use-time, and they get overwritten on the next save.

use chrono::Utc;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{DbHandle, DbResult};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodePosition {
    pub page_id: String,
    pub x: f64,
    pub y: f64,
}

/// Inserts or updates positions for the given pages. Each call is one
/// transaction so a partial failure leaves the table in a consistent
/// state.
pub fn save(handle: &DbHandle, positions: &[NodePosition]) -> DbResult<()> {
    if positions.is_empty() {
        return Ok(());
    }
    handle.with(|conn| {
        let tx = conn.unchecked_transaction()?;
        let now = Utc::now().to_rfc3339();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO node_positions(page_id, x, y, updated_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(page_id) DO UPDATE SET \
                    x = excluded.x, y = excluded.y, updated_at = excluded.updated_at",
            )?;
            for p in positions {
                stmt.execute(params![&p.page_id, p.x, p.y, &now])?;
            }
        }
        tx.commit()?;
        Ok(())
    })
}

/// Reads every stored position. Returned in arbitrary order — the
/// caller (the graph viewer) keys by `page_id` so order is irrelevant.
pub fn load(handle: &DbHandle) -> DbResult<Vec<NodePosition>> {
    handle.with(|conn| {
        let mut stmt = conn.prepare("SELECT page_id, x, y FROM node_positions")?;
        let rows = stmt.query_map([], |row| {
            Ok(NodePosition {
                page_id: row.get(0)?,
                x: row.get(1)?,
                y: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    })
}

/// Wipes all stored positions. The next graph mount falls back to
/// fcose; if the user wants the new fcose layout to persist, the
/// usual save-after-layout path will fill the table back up.
pub fn clear(handle: &DbHandle) -> DbResult<()> {
    handle.with(|conn| {
        conn.execute("DELETE FROM node_positions", [])?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_db() -> (TempDir, DbHandle) {
        let tmp = TempDir::new().unwrap();
        let handle = DbHandle::open(tmp.path()).unwrap();
        (tmp, handle)
    }

    #[test]
    fn load_on_a_fresh_database_returns_an_empty_vec() {
        let (_tmp, db) = fresh_db();
        let got = load(&db).unwrap();
        assert!(got.is_empty(), "fresh DB must have no stored positions");
    }

    #[test]
    fn save_then_load_roundtrips_a_set_of_positions() {
        let (_tmp, db) = fresh_db();
        let mut input = vec![
            NodePosition { page_id: "entities/a".into(), x: 12.5, y: -3.0 },
            NodePosition { page_id: "concepts/b".into(), x: 100.0, y: 200.0 },
        ];
        save(&db, &input).unwrap();
        let mut got = load(&db).unwrap();
        // SQLite doesn't guarantee return order; sort both sides by id
        // before comparing so the test asserts content-equality, not
        // an accidental ordering coincidence.
        input.sort_by(|a, b| a.page_id.cmp(&b.page_id));
        got.sort_by(|a, b| a.page_id.cmp(&b.page_id));
        assert_eq!(got, input);
    }

    #[test]
    fn save_is_an_upsert_keeping_only_the_latest_coordinates() {
        // The drag-save path on the frontend re-fires for every new
        // resting position, so the second call with the same id must
        // overwrite — not duplicate, not error.
        let (_tmp, db) = fresh_db();
        save(
            &db,
            &[NodePosition { page_id: "x".into(), x: 1.0, y: 1.0 }],
        )
        .unwrap();
        save(
            &db,
            &[NodePosition { page_id: "x".into(), x: 9.0, y: 9.0 }],
        )
        .unwrap();
        let got = load(&db).unwrap();
        assert_eq!(got.len(), 1, "upsert must not duplicate the row");
        assert_eq!(got[0].x, 9.0);
        assert_eq!(got[0].y, 9.0);
    }

    #[test]
    fn save_with_an_empty_input_slice_is_a_no_op() {
        // The frontend may call save with nothing in flight (e.g. when
        // a layout completes on an empty graph). Skipping the
        // transaction altogether avoids a wasted commit and is also
        // the only sane semantics.
        let (_tmp, db) = fresh_db();
        save(&db, &[]).unwrap();
        assert!(load(&db).unwrap().is_empty());
    }

    #[test]
    fn clear_removes_every_stored_position() {
        let (_tmp, db) = fresh_db();
        save(
            &db,
            &[
                NodePosition { page_id: "x".into(), x: 1.0, y: 1.0 },
                NodePosition { page_id: "y".into(), x: 2.0, y: 2.0 },
            ],
        )
        .unwrap();
        clear(&db).unwrap();
        assert!(load(&db).unwrap().is_empty());
    }

    #[test]
    fn save_handles_multiple_pages_in_a_single_transaction() {
        // Sanity check that bulk save does what it says — the frontend
        // calls save with the full set of currently-displayed nodes
        // after fcose finishes, which can be hundreds of rows.
        let (_tmp, db) = fresh_db();
        let input: Vec<NodePosition> = (0..50)
            .map(|i| NodePosition {
                page_id: format!("topics/t{i}"),
                x: i as f64,
                y: (i * 2) as f64,
            })
            .collect();
        save(&db, &input).unwrap();
        let got = load(&db).unwrap();
        assert_eq!(got.len(), 50);
    }
}
