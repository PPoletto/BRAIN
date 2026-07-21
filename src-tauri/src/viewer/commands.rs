//! Tauri commands for the viewer (S08–S10).

use std::sync::Arc;

use tauri::State;

use crate::error::{BrainError, BrainResult};
use crate::vault::layout::page_path_for_id;

use super::graph::{self, GraphData, GraphFilters};
use super::search::{self, BacklinkInfo, SearchHit};
use super::tree::{self, PageView, WikiTree};

fn current_vault(state: &crate::state::AppState) -> BrainResult<std::path::PathBuf> {
    state
        .vault_path()
        .ok_or_else(|| BrainError::Internal("no vault is currently mounted".into()))
}

#[tauri::command]
pub fn list_wiki_tree(state: State<Arc<crate::state::AppState>>) -> BrainResult<WikiTree> {
    let vault = current_vault(&state)?;
    tree::list_tree(&vault).map_err(BrainError::from)
}

#[tauri::command]
pub fn read_page(
    state: State<Arc<crate::state::AppState>>,
    id: String,
) -> BrainResult<PageView> {
    let vault = current_vault(&state)?;
    tree::read_page(&vault, &id).map_err(BrainError::from)
}

#[tauri::command]
pub fn search_pages(
    state: State<Arc<crate::state::AppState>>,
    query: String,
) -> BrainResult<Vec<SearchHit>> {
    let vault = current_vault(&state)?;
    let db = state.db();
    search::search_with_db(&vault, &query, db.as_ref()).map_err(BrainError::from)
}

#[tauri::command]
pub fn get_backlinks(
    state: State<Arc<crate::state::AppState>>,
    id: String,
) -> BrainResult<Vec<BacklinkInfo>> {
    let vault = current_vault(&state)?;
    search::backlinks(&vault, &id).map_err(BrainError::from)
}

#[tauri::command]
pub fn get_graph(
    state: State<Arc<crate::state::AppState>>,
    filters: GraphFilters,
) -> BrainResult<GraphData> {
    let vault = current_vault(&state)?;
    graph::build_graph(&vault, &filters).map_err(BrainError::from)
}

/// Runs a Dataview-style query (e.g. `type:source AND tag:customer AND updated:>2026-04-01`)
/// against the SQLite index. Returns matching pages sorted by updated_at DESC.
#[tauri::command]
pub fn query_pages(
    state: State<Arc<crate::state::AppState>>,
    query: String,
) -> BrainResult<Vec<super::query::executor::QueryHit>> {
    let db = state
        .db()
        .ok_or_else(|| BrainError::Internal("no SQLite index is open".into()))?;
    super::query::executor::run(&db, &query)
        .map_err(|err| BrainError::Internal(err.to_string()))
}

/// Forces a full rebuild of the SQLite page index from the filesystem.
/// Mostly a fallback for users on older index format versions — the
/// next bootstrap auto-detects the version mismatch and re-indexes
/// automatically. Surfaced as a Settings button so a manual rebuild is
/// possible after editing pages outside the watcher (rare).
///
/// Resets the stored index_format_version to 0 inside the rebuild
/// transaction's view, which makes the indexer treat every page as
/// stale-by-format and bypass the file_hash skip-fast-path for this run.
#[tauri::command]
pub async fn rebuild_index(
    state: State<'_, Arc<crate::state::AppState>>,
) -> BrainResult<u32> {
    let vault = current_vault(&state)?;
    let db = state
        .db()
        .ok_or_else(|| BrainError::Internal("no SQLite index is open".into()))?;
    state.begin_op();
    let result = tokio::task::spawn_blocking(move || {
        // Force-bypass the format version skip-fast-path by clearing the
        // stored marker before the rebuild. The rebuild itself writes
        // back the current version on success.
        let _ = db.with(|conn| {
            let _ = conn.execute(
                "UPDATE schema_meta SET value='0' WHERE key='index_format_version'",
                [],
            );
            Ok(())
        });
        crate::db::pages_index::rebuild(&db, &vault)
    })
    .await
    .map_err(|e| BrainError::Internal(format!("rebuild task panicked: {e}")))?;
    state.end_op();
    result.map_err(|e| BrainError::Internal(format!("rebuild failed: {e}")))?;
    // Page count for the toast confirmation.
    let count = state
        .db()
        .map(|db| {
            db.with(|conn| {
                conn.query_row::<i64, _, _>(
                    "SELECT COUNT(*) FROM pages",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| {
                    crate::db::DbError::Io(std::io::Error::other(e.to_string()))
                })
            })
            .unwrap_or(0)
        })
        .unwrap_or(0);
    Ok(count as u32)
}

/// Reads every persisted graph-node coordinate. The Tier-3 viewer
/// calls this on mount; if the response is non-empty, it skips the
/// fcose force-directed pass and renders straight from the saved
/// positions — instant load on subsequent opens.
#[tauri::command]
pub fn load_graph_positions(
    state: State<Arc<crate::state::AppState>>,
) -> BrainResult<Vec<crate::db::node_positions::NodePosition>> {
    let db = state
        .db()
        .ok_or_else(|| BrainError::Internal("no SQLite index is open".into()))?;
    crate::db::node_positions::load(&db)
        .map_err(|e| BrainError::Internal(format!("load_graph_positions: {e}")))
}

/// Bulk-saves graph-node coordinates. Called from the Tier-3 viewer
/// after fcose finishes (so the next mount skips fcose) and after a
/// drag (so the user's hand-tuning sticks). The frontend batches
/// multiple drags into one call to avoid hammering SQLite.
#[tauri::command]
pub fn save_graph_positions(
    state: State<Arc<crate::state::AppState>>,
    positions: Vec<crate::db::node_positions::NodePosition>,
) -> BrainResult<()> {
    let db = state
        .db()
        .ok_or_else(|| BrainError::Internal("no SQLite index is open".into()))?;
    crate::db::node_positions::save(&db, &positions)
        .map_err(|e| BrainError::Internal(format!("save_graph_positions: {e}")))
}

/// Wipes all stored graph-node coordinates. Backs the Tier-3
/// "Re-layout" button — one click reverts to fcose, and the next
/// drag/save will re-populate the table.
#[tauri::command]
pub fn clear_graph_positions(
    state: State<Arc<crate::state::AppState>>,
) -> BrainResult<()> {
    let db = state
        .db()
        .ok_or_else(|| BrainError::Internal("no SQLite index is open".into()))?;
    crate::db::node_positions::clear(&db)
        .map_err(|e| BrainError::Internal(format!("clear_graph_positions: {e}")))
}

/// Opens the Markdown file backing a wiki page in the OS-default editor.
/// S08 calls this from the viewer toolbar's "Open in editor" button.
#[tauri::command]
pub fn open_page_in_external_editor(
    state: State<Arc<crate::state::AppState>>,
    id: String,
) -> BrainResult<()> {
    let vault = current_vault(&state)?;
    let path = page_path_for_id(&vault, &id);
    if !path.exists() {
        return Err(BrainError::Internal(format!("page not found: {id}")));
    }
    open::that_detached(&path)
        .map_err(|err| BrainError::Internal(format!("could not open editor: {err}")))
}
