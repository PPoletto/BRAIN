//! Tauri commands exposed to the frontend for the onboarding flow.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::error::{BrainError, BrainResult};
use crate::mcp::registration as mcp_register;
use crate::mount::lifecycle;
use crate::vault::{self, VaultMarker};

use super::disks::{self, DiskInfo};
use super::format;
use super::init;

#[tauri::command]
pub fn list_disks(state: State<Arc<crate::state::AppState>>) -> BrainResult<Vec<DiskInfo>> {
    if let Some(cached) = state.disk_cache() {
        return Ok(cached);
    }
    let disks = disks::list_disks().map_err(BrainError::from)?;
    state.set_disk_cache(disks.clone());
    Ok(disks)
}

/// Force-refreshes the disk listing — call this when the user clicks
/// "Refresh" or after plugging a new device in.
#[tauri::command]
pub fn refresh_disks(state: State<Arc<crate::state::AppState>>) -> BrainResult<Vec<DiskInfo>> {
    state.clear_disk_cache();
    let disks = disks::list_disks().map_err(BrainError::from)?;
    state.set_disk_cache(disks.clone());
    Ok(disks)
}

#[derive(Debug, Clone, Serialize)]
pub struct FormatDiskResult {
    pub mount_path: String,
}

#[tauri::command]
pub fn format_disk(disk_id: String) -> BrainResult<FormatDiskResult> {
    let result = format::format_as_brain(&disk_id).map_err(BrainError::from)?;
    Ok(FormatDiskResult {
        mount_path: result.mount_path.to_string_lossy().to_string(),
    })
}

#[tauri::command]
pub fn init_vault(
    state: State<Arc<crate::state::AppState>>,
    path: String,
) -> BrainResult<VaultMarker> {
    let p = PathBuf::from(path);
    let marker = init::initialize(&p).map_err(BrainError::from)?;
    state.set_vault_path(Some(p));
    Ok(marker)
}

#[tauri::command]
pub fn populate_template(path: String) -> BrainResult<()> {
    let p = PathBuf::from(path);
    crate::onboarding::template::populate(&p).map_err(BrainError::from)
}

/// Downloads the bge-m3 model files (~2.3 GB) from HuggingFace into
/// `04_models/bge-m3/`. Idempotent — files that already exist on disk are
/// skipped, so re-running the wizard is cheap.
///
/// Runs synchronously inside `tokio::spawn_blocking` so the Tauri runtime
/// stays responsive during the download. Progress is emitted as a
/// `model-download-progress` event on the AppHandle, with payload
/// `{ file, current, total }` for the frontend to wire to a progress bar.
#[tauri::command]
pub async fn download_embedding_model(app: AppHandle, path: String) -> BrainResult<()> {
    let p = PathBuf::from(path);
    let models = crate::vault::layout::models_dir(&p).join("bge-m3");

    let app_for_cb = app.clone();
    tokio::task::spawn_blocking(move || {
        let cb = move |file: &str, current: usize, total: usize| {
            let _ = app_for_cb.emit(
                "model-download-progress",
                serde_json::json!({
                    "file": file,
                    "current": current,
                    "total": total,
                }),
            );
        };
        crate::embedding::download::download_bge_m3(&models, Some(&cb))
            .map_err(|e| BrainError::Internal(format!("model download failed: {e}")))
    })
    .await
    .map_err(|e| BrainError::Internal(format!("download task panicked: {e}")))??;

    Ok(())
}

#[tauri::command]
pub fn read_marker(path: String) -> BrainResult<Option<VaultMarker>> {
    let p = PathBuf::from(path);
    vault::read_marker(&p).map_err(BrainError::from)
}

#[tauri::command]
pub fn detect_existing_vault(path: String) -> BrainResult<bool> {
    Ok(vault::layout::is_vault(&PathBuf::from(path)))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BootstrapResult {
    pub auto_mounted: bool,
    pub vault_path: Option<String>,
    pub last_known_vault_missing: bool,
}

/// Resets Brain to the pre-onboarding state: ejects the current vault,
/// unregisters MCP from every supported client, forgets the persisted
/// vault path, and clears the in-memory DB / registration handles. The
/// vault data on disk is untouched — the user can re-open the same path
/// later and pick up exactly where they left off.
#[tauri::command]
pub fn reset_brain(
    state: State<Arc<crate::state::AppState>>,
    app: AppHandle,
) -> BrainResult<()> {
    let _ = lifecycle::unmount(&state, true);
    let _ = mcp_register::unregister_brain_from_supported_clients();
    state.set_db(None);
    state.set_last_registration(None);
    let _ = state.config.update(|s| {
        s.last_active_vault_path = None;
    });
    let _ = app.emit(
        "mount-state",
        serde_json::json!({
            "state": "disconnected",
            "vault_path": null,
        }),
    );
    Ok(())
}

/// Called once at app startup. If the user has a remembered vault that
/// still looks like one on disk, we silently mount it and skip the
/// onboarding wizard. Otherwise the frontend should show the wizard.
#[tauri::command]
pub fn bootstrap_app(
    state: State<Arc<crate::state::AppState>>,
    app: AppHandle,
) -> BrainResult<BootstrapResult> {
    let snapshot = state.config.snapshot();
    let Some(last) = snapshot.last_active_vault_path.clone() else {
        return Ok(BootstrapResult {
            auto_mounted: false,
            vault_path: None,
            last_known_vault_missing: false,
        });
    };

    if !crate::vault::layout::is_vault(&last) {
        // Path is stale (disk unplugged, vault deleted) — let the user
        // decide via the welcome screen.
        return Ok(BootstrapResult {
            auto_mounted: false,
            vault_path: Some(last.to_string_lossy().to_string()),
            last_known_vault_missing: true,
        });
    }

    if let Err(err) = lifecycle::mount_source(&state, &last) {
        tracing::warn!(?err, "auto-mount of remembered vault failed");
        return Ok(BootstrapResult {
            auto_mounted: false,
            vault_path: Some(last.to_string_lossy().to_string()),
            last_known_vault_missing: false,
        });
    }

    if let Ok(db) = crate::db::DbHandle::open(&last) {
        state.set_db(Some(db));
    }

    let inner_state: Arc<crate::state::AppState> = state.inner().clone();
    let _ = crate::wiki::watcher::spawn(app.clone(), inner_state, last.clone());

    let path_str = last.to_string_lossy().to_string();
    let _ = app.emit(
        "mount-state",
        serde_json::json!({
            "state": "mounted-idle",
            "vault_path": path_str,
        }),
    );

    // The two pieces that used to block bootstrap — pages-index rebuild
    // (which can take 30-60 s when bge-m3 needs to re-embed) and MCP
    // re-registration (5+ subprocess spawns on Windows) — now run in a
    // background thread. The window navigates straight to the viewer,
    // active-op counters tick the tray pill yellow while they run, and
    // search/MCP become available the moment they finish. This trades a
    // black-on-startup window for a "ready to read existing pages
    // immediately, fully indexed shortly after" feel.
    spawn_bootstrap_background_work(app.clone(), state.inner().clone(), last);

    Ok(BootstrapResult {
        auto_mounted: true,
        vault_path: Some(path_str),
        last_known_vault_missing: false,
    })
}

/// Off-thread runner for the slow parts of mount: pages-index rebuild
/// (with optional bge-m3 calls for any page whose file_hash changed) and
/// MCP re-registration. Bumps the active-ops counter so the tray pill
/// goes yellow while the work runs and back to green when it's done.
fn spawn_bootstrap_background_work(
    app: AppHandle,
    state: Arc<crate::state::AppState>,
    vault: PathBuf,
) {
    std::thread::spawn(move || {
        state.begin_op();
        struct OpGuard<'a> { state: &'a crate::state::AppState }
        impl Drop for OpGuard<'_> {
            fn drop(&mut self) { self.state.end_op(); }
        }
        let _guard = OpGuard { state: &state };

        if let Some(db) = state.db() {
            if let Err(err) = crate::db::pages_index::rebuild(&db, &vault) {
                tracing::warn!(?err, "background rebuild after bootstrap failed");
            }
        }

        match mcp_register::register_brain_in_supported_clients(&vault) {
            Ok(report) => {
                state.set_last_registration(Some(report.clone()));
                let _ = app.emit("mcp-registration-status", &report);
            }
            Err(err) => tracing::warn!(?err, "background MCP re-registration failed"),
        }
    });
}

/// Finishes onboarding: mounts the vault, opens the SQLite index, starts the
/// wiki watcher, registers MCP in supported clients, and emits a
/// `mount-state` event so the tray and viewer pick up the new state.
/// Idempotent.
#[tauri::command]
pub fn finish_onboarding(
    state: State<Arc<crate::state::AppState>>,
    app: AppHandle,
    path: String,
) -> BrainResult<()> {
    let vault_path = PathBuf::from(&path);
    lifecycle::mount_source(&state, &vault_path).map_err(BrainError::from)?;

    // Persist the active vault path so the next Brain start auto-mounts
    // it and skips the wizard. Failure to persist is non-fatal.
    let path_clone = vault_path.clone();
    let _ = state.config.update(|s| {
        s.last_active_vault_path = Some(path_clone);
    });

    // Open SQLite + run migrations synchronously so the DB handle exists
    // when the background worker tries to rebuild against it.
    if let Ok(db) = crate::db::DbHandle::open(&vault_path) {
        state.set_db(Some(db));
    }

    // Start the wiki watcher so edits via external editors or MCP get
    // automatically committed.
    let inner_state: Arc<crate::state::AppState> = state.inner().clone();
    let _ = crate::wiki::watcher::spawn(app.clone(), inner_state, vault_path.clone());

    let _ = app.emit(
        "mount-state",
        serde_json::json!({
            "state": "mounted-idle",
            "vault_path": path,
        }),
    );

    // Defer the slow steps (pages-index rebuild + MCP registration) to a
    // background thread. The Completion screen renders straight away;
    // the registration-status card on it populates from the
    // `mcp-registration-status` event a few seconds later.
    spawn_bootstrap_background_work(app, state.inner().clone(), vault_path);

    Ok(())
}
