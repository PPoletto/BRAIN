//! Tauri commands wrapping the update flow (S04).
//!
//! The actual download + signature verification + install is delegated to
//! `tauri-plugin-updater`, which embeds the same minisign primitives we'd
//! call by hand. We layer the channel + skip-list + user-prompt policy on
//! top so the UX matches the spec.

use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_updater::UpdaterExt;

use crate::error::{BrainError, BrainResult};

use super::skip_list;

#[derive(Debug, Clone, Serialize)]
pub struct UpdateAvailability {
    pub available: bool,
    pub version: Option<String>,
    pub current_version: String,
    pub notes: Option<String>,
    pub date: Option<String>,
    pub skipped: bool,
}

#[tauri::command]
pub async fn check_update(
    app: AppHandle,
    state: State<'_, Arc<crate::state::AppState>>,
) -> BrainResult<UpdateAvailability> {
    let updater = app
        .updater()
        .map_err(|err| BrainError::Internal(format!("updater unavailable: {err}")))?;
    let current = env!("CARGO_PKG_VERSION").to_string();
    let info = match updater.check().await {
        Ok(info) => info,
        Err(err) => {
            // Offline / repository unreachable / no release yet → spec S04
            // says we silently report "no update". Log for diagnostics.
            tracing::debug!(?err, "update check returned no info");
            return Ok(UpdateAvailability {
                available: false,
                version: None,
                current_version: current,
                notes: None,
                date: None,
                skipped: false,
            });
        }
    };
    let Some(update) = info else {
        return Ok(UpdateAvailability {
            available: false,
            version: None,
            current_version: current,
            notes: None,
            date: None,
            skipped: false,
        });
    };
    let skipped = skip_list::is_skipped(&state.config, &update.version);
    Ok(UpdateAvailability {
        available: !skipped,
        version: Some(update.version),
        current_version: current,
        notes: update.body,
        date: update.date.map(|d| d.to_string()),
        skipped,
    })
}

#[tauri::command]
pub async fn apply_update(app: AppHandle) -> BrainResult<()> {
    let updater = app
        .updater()
        .map_err(|err| BrainError::Internal(format!("updater unavailable: {err}")))?;
    let info = updater
        .check()
        .await
        .map_err(|err| BrainError::Internal(format!("update check failed: {err}")))?;
    let Some(update) = info else {
        return Err(BrainError::Internal("no update is currently available".into()));
    };
    update
        .download_and_install(|_chunk_size, _total_size| {}, || {})
        .await
        .map_err(|err| BrainError::Internal(format!("install failed: {err}")))?;
    // tauri-plugin-updater triggers the restart itself on the next
    // `app.restart()`; the frontend is expected to call a separate command
    // or we can do it here.
    app.restart();
}

#[tauri::command]
pub fn skip_update(
    state: State<'_, Arc<crate::state::AppState>>,
    version: String,
) -> BrainResult<()> {
    skip_list::skip(&state.config, &version)?;
    Ok(())
}
