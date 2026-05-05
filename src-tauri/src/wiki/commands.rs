//! Tauri commands for wiki history (S03).

use std::sync::Arc;

use tauri::State;

use crate::error::{BrainError, BrainResult};
use crate::vault::layout::wiki_dir;

use super::history::{self, CommitDetail, CommitInfo};

fn current_wiki_dir(state: &crate::state::AppState) -> BrainResult<std::path::PathBuf> {
    state
        .vault_path()
        .map(|v| wiki_dir(&v))
        .ok_or_else(|| BrainError::Internal("no vault is currently mounted".into()))
}

#[tauri::command]
pub fn wiki_history(
    state: State<Arc<crate::state::AppState>>,
    limit: usize,
) -> BrainResult<Vec<CommitInfo>> {
    let dir = current_wiki_dir(&state)?;
    history::list_commits(&dir, limit).map_err(BrainError::from)
}

#[tauri::command]
pub fn wiki_restore_page(
    state: State<Arc<crate::state::AppState>>,
    sha: String,
    page: String,
) -> BrainResult<()> {
    let dir = current_wiki_dir(&state)?;
    history::restore_page(&dir, &sha, &page).map_err(BrainError::from)
}

#[tauri::command]
pub fn wiki_commit_detail(
    state: State<Arc<crate::state::AppState>>,
    sha: String,
) -> BrainResult<CommitDetail> {
    let dir = current_wiki_dir(&state)?;
    history::commit_detail(&dir, &sha).map_err(BrainError::from)
}

#[tauri::command]
pub fn wiki_hard_reset(
    state: State<Arc<crate::state::AppState>>,
    sha: String,
) -> BrainResult<()> {
    let dir = current_wiki_dir(&state)?;
    history::hard_reset(&dir, &sha).map_err(BrainError::from)
}
