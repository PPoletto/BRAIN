//! Tauri commands for mount-side integrity checks and recovery (S07).

use std::sync::Arc;

use tauri::State;

use crate::error::{BrainError, BrainResult};

use super::integrity::{self, IntegrityReport};
use super::lifecycle::UncleanFlag;

#[tauri::command]
pub fn unclean_shutdown_pending(
    state: State<Arc<crate::state::AppState>>,
) -> BrainResult<bool> {
    let Some(vault) = state.vault_path() else {
        return Ok(false);
    };
    Ok(UncleanFlag::is_set(&vault))
}

#[tauri::command]
pub fn run_integrity_check(
    state: State<Arc<crate::state::AppState>>,
) -> BrainResult<IntegrityReport> {
    let vault = state.vault_path().ok_or_else(|| {
        BrainError::Internal("no vault is currently mounted".into())
    })?;
    let db = state.db();
    Ok(integrity::check(&vault, db.as_ref()))
}

#[tauri::command]
pub fn run_recovery_action(
    state: State<Arc<crate::state::AppState>>,
    id: String,
) -> BrainResult<String> {
    let vault = state.vault_path().ok_or_else(|| {
        BrainError::Internal("no vault is currently mounted".into())
    })?;
    match id.as_str() {
        "rebuild-pages-index" => {
            let db = state.db().ok_or_else(|| {
                BrainError::Internal("database is not open".into())
            })?;
            crate::db::pages_index::rebuild(&db, &vault)
                .map_err(|err| BrainError::Internal(err.to_string()))?;
            UncleanFlag::clear(&vault).ok();
            Ok("Search index rebuilt from filesystem.".into())
        }
        "wiki-restore-last-good" => {
            // Reset the wiki tree to the most recent commit. Anything in the
            // working tree but not in HEAD is discarded — this is destructive
            // by design and the frontend warns the user before invoking us.
            let wiki = crate::vault::layout::wiki_dir(&vault);
            let repo = git2::Repository::open(&wiki).map_err(|err| {
                BrainError::Internal(format!("cannot open wiki repo: {err}"))
            })?;
            let head = repo.head().map_err(|err| {
                BrainError::Internal(format!("HEAD missing: {err}"))
            })?;
            let oid = head.target().ok_or_else(|| {
                BrainError::Internal("HEAD has no commit yet".into())
            })?;
            let object = repo
                .find_object(oid, Some(git2::ObjectType::Commit))
                .map_err(|err| BrainError::Internal(err.to_string()))?;
            repo.reset(&object, git2::ResetType::Hard, None)
                .map_err(|err| BrainError::Internal(err.to_string()))?;
            UncleanFlag::clear(&vault).ok();
            Ok(format!("Wiki reset to {} (HARD).", &oid.to_string()[..8]))
        }
        other => Err(BrainError::Internal(format!("unknown recovery action: {other}"))),
    }
}

#[tauri::command]
pub fn dismiss_unclean_flag(
    state: State<Arc<crate::state::AppState>>,
) -> BrainResult<()> {
    let Some(vault) = state.vault_path() else {
        return Ok(());
    };
    UncleanFlag::clear(&vault).ok();
    Ok(())
}
