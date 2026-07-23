//! Background task that updates the tray tooltip + icon and emits
//! `mount-state` events so the frontend stays in sync with the backend
//! mount/active-op state. Polls every 500 ms — light enough to be invisible,
//! fast enough to make idle/busy transitions feel instant.
//!
//! Also runs a vault-availability probe at a slower cadence (every
//! 2 seconds): if the vault marker file disappears while we still think
//! we're mounted (typical cause: user pulled the SSD without ejecting),
//! we transition the state machine to `Disconnected` so the tray turns
//! grey, the StatusBar shows "No vault mounted", and any new MCP tool
//! call surfaces a `BRAIN_VAULT_DISCONNECTED` error to the LLM.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Runtime};

use crate::state::AppState;

use super::icon::IconKind;
use super::state_machine::derive;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How often to probe `is_vault(vault_path)` to catch a yanked-disk
/// scenario. Slower than the tray-state poll because the check hits the
/// filesystem and we don't want to thrash a slow USB device.
const DISAPPEARANCE_PROBE_INTERVAL: Duration = Duration::from_secs(2);

pub fn spawn<R: Runtime>(app: AppHandle<R>, state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        let mut last_busy_at: Option<Instant> = None;
        let mut last_tag: String = String::new();
        let mut last_active_ops: u32 = u32::MAX;
        let mut last_disappearance_check = Instant::now();
        loop {
            let now = Instant::now();
            if state.active_ops() > 0 {
                last_busy_at = Some(now);
            }

            // Throttle the vault-availability probe so the rest of the
            // loop stays at 500 ms cadence. The same probe handles both
            // directions: disappearance (mounted → disconnected) and
            // reappearance (disconnected → mounted).
            if now.duration_since(last_disappearance_check) >= DISAPPEARANCE_PROBE_INTERVAL {
                last_disappearance_check = now;
                if crate::mount::lifecycle::handle_vault_disappearance(&state) {
                    tracing::warn!(
                        "vault marker disappeared while mounted — \
                         transitioning to Disconnected (likely SSD unplugged)"
                    );
                    let _ = app.emit(
                        "toast",
                        serde_json::json!({
                            "kind": "warning",
                            "message": "BRAIN disk disconnected",
                            "detail": "The disk holding the vault was unplugged \
                                       without ejecting. Reconnect it to continue.",
                        }),
                    );
                    last_tag = String::new();
                } else if let Some(path) =
                    crate::mount::lifecycle::try_auto_reconnect(&state)
                {
                    // Disk is back online. `try_auto_reconnect` already
                    // ran `mount_source` (which sets state +
                    // vault_path); we still need to (re-)open the DB,
                    // (re-)spawn the watcher, refresh MCP registration
                    // in case the binary path changed, and emit a
                    // `mount-state` event so the frontend snaps out of
                    // the "No vault" view.
                    finish_auto_reconnect(&app, &state, path.clone());
                    let _ = app.emit(
                        "toast",
                        serde_json::json!({
                            "kind": "success",
                            "message": "BRAIN disk reconnected",
                            "detail": format!("Mounted {} again.", path.display()),
                        }),
                    );
                    last_tag = String::new();
                }
            }

            let tray_state = derive(&state, last_busy_at, now);
            let tag = tray_state.tag().to_string();
            let active = state.active_ops();

            if tag != last_tag || active != last_active_ops {
                if let Some(tray) = app.tray_by_id("brain-tray") {
                    let _ = tray.set_tooltip(Some(tray_state.tooltip()));
                    if let Ok(img) = IconKind::from_tag(&tag).image() {
                        let _ = tray.set_icon(Some(img));
                    }
                }
                let _ = app.emit(
                    "mount-state",
                    serde_json::json!({
                        "state": tag,
                        "tooltip": tray_state.tooltip(),
                        "vault_path": state.vault_path().map(|p| p.display().to_string()),
                        "active_operations": active,
                        "active_operation_labels": state.active_op_labels(),
                    }),
                );
                last_tag = tag;
                last_active_ops = active;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

/// Side-effects after `try_auto_reconnect` succeeds. The lifecycle
/// helper just flips the in-memory mount state; this puts the rest of
/// the app's state machinery back into "mounted" mode the same way
/// `bootstrap_app` and `finish_onboarding` do.
fn finish_auto_reconnect<R: Runtime>(
    app: &AppHandle<R>,
    state: &Arc<AppState>,
    path: PathBuf,
) {
    if let Ok(db) = crate::db::DbHandle::open(&path) {
        state.set_db(Some(db));
    }
    let inner_state = state.clone();
    let _ = crate::wiki::watcher::spawn(app.clone(), inner_state, path.clone());

    // Re-register MCP. The binary path may have changed since the user
    // last connected the disk (e.g. they upgraded BRAIN), so refreshing
    // here keeps Claude/Codex/etc. pointing at the live exe.
    let path_for_register = path.clone();
    let app_for_register = app.clone();
    let state_for_register = state.clone();
    std::thread::spawn(move || {
        match crate::mcp::registration::register_brain_in_supported_clients(
            &path_for_register,
        ) {
            Ok(report) => {
                state_for_register.set_last_registration(Some(report.clone()));
                let _ = app_for_register.emit("mcp-registration-status", &report);
            }
            Err(err) => {
                tracing::warn!(?err, "MCP re-registration on auto-reconnect failed");
            }
        }
    });

    // Background pages-index rebuild — same pattern as bootstrap_app,
    // so the user sees the viewer immediately and any indexing work
    // (file_hash skip-fast-path catches most pages) ticks the active-op
    // counter while it runs.
    let path_for_rebuild = path.clone();
    let state_for_rebuild = state.clone();
    std::thread::spawn(move || {
        const OP: &str = "Rebuilding the index";
        state_for_rebuild.begin_op(OP);
        struct Guard<'a> { s: &'a AppState }
        impl Drop for Guard<'_> { fn drop(&mut self) { self.s.end_op(OP); } }
        let _g = Guard { s: &state_for_rebuild };
        if let Some(db) = state_for_rebuild.db() {
            if let Err(err) = crate::db::pages_index::rebuild(&db, &path_for_rebuild) {
                tracing::warn!(?err, "pages_index rebuild on auto-reconnect failed");
            }
        }
    });
}
