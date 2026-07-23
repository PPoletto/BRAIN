//! Tauri commands for wiki history (S03) and remote sync (S11 phase 6).

use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use crate::error::{BrainError, BrainResult};
use crate::vault::layout::wiki_dir;

use super::history::{self, CommitDetail, CommitInfo};
use super::sync::{self, MergeOutcome};

fn current_wiki_dir(state: &crate::state::AppState) -> BrainResult<std::path::PathBuf> {
    state
        .vault_path()
        .map(|v| wiki_dir(&v))
        .ok_or_else(|| BrainError::Internal("no vault is currently mounted".into()))
}

/// Remote-sync configuration snapshot for the Settings UI.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteStatus {
    /// Whether content encryption is enabled (required before a network
    /// remote can be attached).
    pub encrypted: bool,
    /// The configured remote URL, if any.
    pub remote_url: Option<String>,
    /// Whether a credential (PAT) is stored for the remote on this machine.
    pub has_credential: bool,
}

/// Result of a sync, for the UI toast.
#[derive(Debug, Clone, Serialize)]
pub struct SyncReport {
    /// `"up-to-date" | "fast-forward" | "merged"`.
    pub outcome: String,
    /// Repo-relative paths whose plaintext genuinely conflicted and now
    /// carry conflict markers for the user to resolve (empty otherwise).
    pub conflicted_pages: Vec<String>,
    /// Whether the working tree changed and the index was rebuilt.
    pub reindexed: bool,
}

#[tauri::command]
pub fn git_remote_status(state: State<Arc<crate::state::AppState>>) -> BrainResult<RemoteStatus> {
    let vault = state
        .vault_path()
        .ok_or_else(|| BrainError::Internal("no vault is currently mounted".into()))?;
    let wiki = wiki_dir(&vault);
    Ok(RemoteStatus {
        encrypted: crate::wiki::encryption::is_encrypted(&vault),
        remote_url: sync::remote_url(&wiki),
        has_credential: sync::has_credential(&wiki),
    })
}

/// Attach (or update) the sync remote. Enforces the encryption coupling
/// (a network URL requires an encrypted vault).
#[tauri::command]
pub fn set_git_remote(
    state: State<Arc<crate::state::AppState>>,
    url: String,
) -> BrainResult<()> {
    let wiki = current_wiki_dir(&state)?;
    sync::set_remote(&wiki, &url).map_err(BrainError::from)
}

/// Store the remote credential (PAT) in the OS keychain for this vault.
#[tauri::command]
pub fn set_git_credential(
    state: State<Arc<crate::state::AppState>>,
    pat: String,
) -> BrainResult<()> {
    let wiki = current_wiki_dir(&state)?;
    sync::set_remote_credential(&wiki, &pat).map_err(BrainError::from)
}

/// fetch → merge → push, then rebuild the index if the working tree
/// changed (the merge/FF re-materialises pages). Returns a report for the
/// UI.
#[tauri::command]
pub async fn sync_now(
    state: State<'_, Arc<crate::state::AppState>>,
) -> BrainResult<SyncReport> {
    let vault = state
        .vault_path()
        .ok_or_else(|| BrainError::Internal("no vault is currently mounted".into()))?;
    let wiki = wiki_dir(&vault);

    state.begin_op();
    let sync_wiki = wiki.clone();
    let outcome = tokio::task::spawn_blocking(move || sync::sync(&sync_wiki)).await;
    state.end_op();
    let outcome = outcome
        .map_err(|e| BrainError::Internal(format!("sync task panicked: {e}")))?
        .map_err(BrainError::from)?;

    let (tag, conflicted, changed) = match &outcome {
        MergeOutcome::UpToDate => ("up-to-date", Vec::new(), false),
        MergeOutcome::FastForward(_) => ("fast-forward", Vec::new(), true),
        MergeOutcome::Merged { conflicted_pages, .. } => {
            ("merged", conflicted_pages.clone(), true)
        }
    };

    // The merge/FF re-materialised the working tree; refresh the index so
    // search/graph/query reflect the pulled changes. `rebuild` uses the
    // file_hash fast-path, so unchanged pages are skipped.
    if changed {
        if let Some(db) = state.db() {
            let reindex_vault = vault.clone();
            state.begin_op();
            let r = tokio::task::spawn_blocking(move || {
                crate::db::pages_index::rebuild(&db, &reindex_vault)
            })
            .await;
            state.end_op();
            r.map_err(|e| BrainError::Internal(format!("reindex task panicked: {e}")))?
                .map_err(|e| BrainError::Internal(format!("reindex after sync failed: {e}")))?;
        }
    }

    Ok(SyncReport {
        outcome: tag.to_string(),
        conflicted_pages: conflicted,
        reindexed: changed,
    })
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
