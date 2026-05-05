//! Tauri commands exposed to the frontend for the tray (S07).

use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tauri::State;

use crate::error::BrainResult;
use crate::mount::{lifecycle, MountError};

use super::state_machine::{derive, TrayState};

#[derive(Debug, Clone, Serialize)]
pub struct TrayStatus {
    pub state: String,
    pub tooltip: String,
    pub vault_path: Option<String>,
    pub active_operations: u32,
    pub message: Option<String>,
}

#[tauri::command]
pub fn tray_status(state: State<Arc<crate::state::AppState>>) -> BrainResult<TrayStatus> {
    let now = Instant::now();
    let derived = derive(&state, None, now);
    let (tag, tooltip, message) = match derived {
        TrayState::Error(msg) => (
            "error".to_string(),
            format!("BRAIN error – {msg}"),
            Some(msg),
        ),
        other => (
            other.tag().to_string(),
            other.tooltip(),
            None,
        ),
    };
    Ok(TrayStatus {
        state: tag,
        tooltip,
        vault_path: state.vault_path().map(|p| p.display().to_string()),
        active_operations: state.active_ops(),
        message,
    })
}

#[tauri::command]
pub fn eject_brain(
    state: State<Arc<crate::state::AppState>>,
    force: bool,
) -> BrainResult<()> {
    match lifecycle::unmount(&state, force) {
        Ok(()) => {
            let _ = crate::mcp::registration::unregister_brain_from_supported_clients();
            Ok(())
        }
        Err(MountError::NotMounted) => Ok(()),
        Err(other) => Err(other.into()),
    }
}
