//! S07 — Tray UI and Status Communication.

pub mod commands;
pub mod icon;
pub mod state_loop;
pub mod state_machine;

use std::sync::Arc;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Runtime, WindowEvent};

use crate::state::AppState;

pub fn setup<R: Runtime>(
    app: AppHandle<R>,
    state: Arc<AppState>,
) -> tauri::Result<()> {
    let menu = build_menu(&app)?;

    let mut builder = TrayIconBuilder::with_id("brain-tray")
        .tooltip("BRAIN disconnected")
        .menu(&menu);
    if let Ok(img) = icon::IconKind::Disconnected.image() {
        builder = builder.icon(img).icon_as_template(false);
    }
    let _tray = builder
        .on_menu_event(move |app, event| handle_menu_event(app, event.id.as_ref()))
        .on_tray_icon_event(|tray, event| {
            // Left-click on the tray brings the main window forward — typical
            // tray-app affordance and a recovery path when the user closes
            // the window via the OS chrome.
            if let TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                if let Some(win) = tray.app_handle().get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.unminimize();
                    let _ = win.set_focus();
                }
            }
        })
        .build(&app)?;

    // Window close → hide instead of quit. The tray keeps Brain alive in the
    // background so MCP requests, the wiki watcher, and the mount lifecycle
    // continue until the user explicitly chooses Quit from the tray.
    if let Some(win) = app.get_webview_window("main") {
        let win_handle = win.clone();
        win.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = win_handle.hide();
            }
        });
    }

    state_loop::spawn(app.clone(), state);
    Ok(())
}

fn handle_menu_event<R: Runtime>(app: &AppHandle<R>, id: &str) {
    match id {
        "quit" => {
            app.exit(0);
        }
        "show_window" | "viewer" | "settings" | "history" => {
            let route = match id {
                "viewer" => "/viewer",
                "settings" => "/settings",
                "history" => "/wiki-history",
                _ => "/",
            };
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
            // The frontend uses `createBrowserRouter`, which does NOT react
            // to `window.location.hash` changes. Emit a Tauri event that
            // the React layer listens for and forwards to `router.navigate`.
            let _ = app.emit("navigate-to", route);
        }
        "eject" => {
            if let Some(state) = app.try_state::<Arc<crate::state::AppState>>() {
                let force = state.active_ops() == 0;
                let _ = crate::mount::lifecycle::unmount(&state, force);
                let _ = tauri::Emitter::emit(
                    app,
                    "mount-state",
                    serde_json::json!({"state": "disconnected", "vault_path": null}),
                );
            }
        }
        "reregister_mcp" => {
            // Re-write Brain into every supported LLM client's config with
            // the **current** binary path. Useful when the user switched
            // between debug/release builds, moved the install, or just
            // wants a clean retry. We then surface the result by sending
            // them to Settings.
            if let Some(state) = app.try_state::<Arc<crate::state::AppState>>() {
                if let Some(vault) = state.vault_path() {
                    match crate::mcp::registration::register_brain_in_supported_clients(&vault) {
                        Ok(report) => {
                            state.set_last_registration(Some(report.clone()));
                            let _ = app.emit("mcp-registration-status", &report);
                            tracing::info!(?report, "MCP re-registered via tray");
                        }
                        Err(err) => {
                            tracing::error!(?err, "tray Re-register MCP failed");
                        }
                    }
                } else {
                    tracing::warn!("Re-register MCP requested with no vault mounted");
                }
            }
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
            let _ = app.emit("navigate-to", "/settings");
        }
        _ => {}
    }
}

fn build_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let show = MenuItem::with_id(app, "show_window", "Open window", true, None::<&str>)?;
    let viewer = MenuItem::with_id(app, "viewer", "Open BRAIN Viewer", true, None::<&str>)?;
    let history = MenuItem::with_id(app, "history", "Wiki history…", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let sep_mcp = PredefinedMenuItem::separator(app)?;
    let reregister = MenuItem::with_id(
        app,
        "reregister_mcp",
        "Re-register MCP (after binary moved)",
        true,
        None::<&str>,
    )?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let eject = MenuItem::with_id(app, "eject", "Eject BRAIN", true, None::<&str>)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit BRAIN", true, None::<&str>)?;
    Menu::with_items(
        app,
        &[
            &show, &viewer, &history, &settings, &sep_mcp, &reregister, &sep1, &eject, &sep2,
            &quit,
        ],
    )
}
