//! Tauri commands exposed to the frontend for the onboarding flow.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::error::{BrainError, BrainResult};
use crate::mcp::registration as mcp_register;
use crate::mount::lifecycle;
use crate::vault::{self, VaultMarker};

/// Walks the supplied paths and returns the first one that looks
/// like a BRAIN vault. Used by `bootstrap_app` as a fallback when
/// no `last_active_vault_path` is saved (or it was self-healed away
/// because it pointed into the OS temp dir): if the user's BRAIN
/// drive is plugged in, we'd rather auto-mount it than dump them
/// back into the welcome wizard.
///
/// Temp-dir safety lives at the call site (bootstrap only persists
/// the result when it is NOT in the OS temp dir) rather than here,
/// so this helper stays a pure "is there a marker?" check that's
/// trivial to test.
fn find_attached_vault(
    paths: impl Iterator<Item = PathBuf>,
) -> Option<PathBuf> {
    paths.into_iter().find(|p| crate::vault::layout::is_vault(p))
}

/// Returns true when `path` is somewhere inside the OS's temp directory.
/// Used to refuse persisting an obviously-bad vault path that the user
/// might have picked through the folder dialog by accident — temp dirs
/// get wiped by the OS, which is what made the post-update offline
/// loop self-perpetuating: each setup wrote a temp path, the next OS
/// reboot/update wiped it, BRAIN saw "vault gone" and kicked the user
/// back into setup with the same broken default location.
///
/// Three layers because a *stale* temp path may not exist on disk
/// (so `canonicalize` fails) and may have been written in 8.3
/// short-name form (so a direct prefix check against `env::temp_dir()`
/// long-form misses it):
///
/// 1. Direct prefix match — covers the common "current user, current
///    process" case on every OS.
/// 2. Canonicalised match — handles short/long-name discrepancy on
///    Windows for paths that do exist.
/// 3. `…\AppData\Local\Temp\…` component-pattern heuristic — catches
///    stale paths from a different OS user (`PASCAL~1.POL\AppData\
///    Local\Temp\…`) that no longer exist on disk and so fail
///    canonicalisation. Targeted enough to avoid false-positives on
///    arbitrary directories named `temp` outside that chain.
fn is_under_temp_dir(path: &Path) -> bool {
    let temp = std::env::temp_dir();

    if path.starts_with(&temp) {
        return true;
    }

    if let (Ok(p), Ok(t)) = (path.canonicalize(), temp.canonicalize()) {
        if p.starts_with(&t) {
            return true;
        }
    }

    // Component-pattern heuristic for stale paths the OS has wiped
    // (so canonicalize() can't help) and for paths whose separator
    // doesn't match the host's. The CI runs on Linux where
    // `Path::components()` does not split on `\\` — a config file
    // saved on Windows then read on Linux would look like a single
    // opaque component and the heuristic would miss. Working on the
    // raw lower-cased string with both separators normalised to `/`
    // keeps the check OS-agnostic.
    let lower = path.to_string_lossy().to_lowercase();
    let normalised = lower.replace('\\', "/");
    let parts: Vec<&str> = normalised
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    parts
        .windows(3)
        .any(|w| w == ["appdata", "local", "temp"])
}

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

    // Step 1: try the persisted path. Three sub-cases — temp-dir
    // self-heal, stale-but-not-temp (offline screen), live (auto-mount).
    if let Some(last) = snapshot.last_active_vault_path.clone() {
        if is_under_temp_dir(&last) {
            // Self-heal: the persisted path lives inside the OS temp
            // dir, almost certainly written by an earlier broken
            // setup. Forget it and fall through to step 2 (auto-detect)
            // so we still try to find an attached BRAIN drive instead
            // of dumping the user back into the wizard.
            tracing::warn!(
                path = %last.display(),
                "discarding persisted vault path inside OS temp dir; trying disk-scan fallback"
            );
            let _ = state.config.update(|s| {
                s.last_active_vault_path = None;
            });
            // Intentional fall-through — drop out of the `if let` below
            // by NOT returning here.
        } else if !crate::vault::layout::is_vault(&last) {
            // Path looks legit but the disk isn't here right now.
            // Show the offline screen so the user gets the explicit
            // "your saved path points at <X>, plug it in" feedback
            // rather than a silent re-detection that might pick a
            // *different* attached vault.
            return Ok(BootstrapResult {
                auto_mounted: false,
                vault_path: Some(last.to_string_lossy().to_string()),
                last_known_vault_missing: true,
            });
        } else if let Some(result) = complete_auto_mount(state.inner(), &app, last.clone()) {
            return Ok(result);
        } else {
            tracing::warn!("auto-mount of remembered vault failed");
            return Ok(BootstrapResult {
                auto_mounted: false,
                vault_path: Some(last.to_string_lossy().to_string()),
                last_known_vault_missing: false,
            });
        }
    }

    // Step 2: scan attached disks for a vault marker. Catches the
    // "BRAIN drive is plugged in but bootstrap has no saved path
    // for it" case — fresh install, post-self-heal of the temp-path
    // bug, post-reset_brain. Without this fallback the user lands
    // in the welcome wizard even though there's literally a BRAIN
    // disk sitting one connector away.
    let candidates = disks::list_disks()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|d| d.mount_path.map(PathBuf::from));
    if let Some(found) = find_attached_vault(candidates) {
        tracing::info!(
            path = %found.display(),
            "auto-detected attached BRAIN vault; mounting and persisting"
        );
        // Persist so the *next* startup finds it the cheap way (no
        // disk-scan). Defensive temp-dir guard mirrors the
        // finish_onboarding write-side check; in practice list_disks
        // never returns temp paths but the cost of the check is
        // negligible.
        if !is_under_temp_dir(&found) {
            let _ = state.config.update(|s| {
                s.last_active_vault_path = Some(found.clone());
            });
        }
        if let Some(result) = complete_auto_mount(state.inner(), &app, found) {
            return Ok(result);
        }
    }

    // Step 3: nothing saved, nothing detected. Welcome wizard.
    Ok(BootstrapResult {
        auto_mounted: false,
        vault_path: None,
        last_known_vault_missing: false,
    })
}

/// Wraps the whole "we have a vault path that looks legit, set up
/// the running app around it" sequence — mount, open DB, start
/// watcher, emit `mount-state`, kick off background indexing /
/// MCP-registration. Returns `Some(BootstrapResult { auto_mounted:
/// true, ... })` on success; `None` if `mount_source` rejects the
/// path so the caller can decide what to surface.
///
/// Extracted out of `bootstrap_app` so both the persisted-path
/// branch and the auto-detect-fallback branch reuse the same
/// post-mount machinery — divergence between the two paths would
/// be subtle (different watcher, different background work) and
/// the wrong source of bugs to invite.
fn complete_auto_mount(
    app_state: &Arc<crate::state::AppState>,
    app: &AppHandle,
    vault_path: PathBuf,
) -> Option<BootstrapResult> {
    if lifecycle::mount_source(app_state, &vault_path).is_err() {
        return None;
    }

    if let Ok(db) = crate::db::DbHandle::open(&vault_path) {
        app_state.set_db(Some(db));
    }

    let _ = crate::wiki::watcher::spawn(app.clone(), app_state.clone(), vault_path.clone());

    let path_str = vault_path.to_string_lossy().to_string();
    let _ = app.emit(
        "mount-state",
        serde_json::json!({
            "state": "mounted-idle",
            "vault_path": path_str.clone(),
        }),
    );

    // The two pieces that used to block bootstrap — pages-index rebuild
    // (which can take 30-60 s when bge-m3 needs to re-embed) and MCP
    // re-registration (5+ subprocess spawns on Windows) — run in a
    // background thread. The window navigates straight to the viewer,
    // active-op counters tick the tray pill yellow while they run, and
    // search/MCP become available the moment they finish.
    spawn_bootstrap_background_work(app.clone(), app_state.clone(), vault_path);

    Some(BootstrapResult {
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

    // Defend against the post-update offline loop: if the user picked a
    // temp directory through the folder dialog (or somehow ended up with
    // one), refuse it before persisting. Without this guard the same
    // broken path would survive into `last_active_vault_path`, the OS
    // would wipe it on next reboot/update, and the next launch would
    // again show "BRAIN is offline" — exactly the cycle we observed.
    if is_under_temp_dir(&vault_path) {
        return Err(BrainError::Internal(format!(
            "vault path '{}' is inside the system temp directory and would be \
             wiped by the OS — pick a permanent location like an external SSD \
             or a folder under your home directory.",
            vault_path.display()
        )));
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn is_under_temp_dir_detects_paths_under_the_current_user_temp() {
        // Layer 1: direct prefix match against `std::env::temp_dir()`.
        // Picks up the case where the running process and the saved
        // path are on the same machine with the same user form.
        let inside = std::env::temp_dir().join("brain-test-vault");
        assert!(
            is_under_temp_dir(&inside),
            "path inside temp_dir() must be flagged: {inside:?}"
        );
    }

    #[test]
    fn is_under_temp_dir_accepts_paths_outside_temp() {
        let outside = if cfg!(target_os = "windows") {
            PathBuf::from("D:/my-vault")
        } else {
            PathBuf::from("/home/user/my-vault")
        };
        assert!(
            !is_under_temp_dir(&outside),
            "permanent path must NOT be flagged: {outside:?}"
        );
    }

    #[test]
    fn is_under_temp_dir_catches_stale_windows_appdata_local_temp_paths() {
        // Layer 3 (the actually-bug-relevant one): on the affected user's
        // Windows machine, the saved path was
        // `C:\Users\PASCAL~1.POL\AppData\Local\Temp\.tmp2oZT2Y` —
        // 8.3 short-name form pointing into Local\Temp\ for a stale user.
        // The path no longer exists on disk, so canonicalize() can't
        // help. The component-pattern heuristic must still flag it.
        let stale = PathBuf::from(
            r"C:\Users\PASCAL~1.POL\AppData\Local\Temp\.tmp2oZT2Y",
        );
        assert!(
            is_under_temp_dir(&stale),
            "stale Windows AppData\\Local\\Temp path must be flagged"
        );
    }

    #[test]
    fn is_under_temp_dir_catches_appdata_local_temp_paths_with_forward_slashes() {
        // Cross-platform regression: this same heuristic must work on
        // both Windows (`\`-separated) and Linux/macOS (`/`-separated).
        // The CI runs on Linux, where Path::components() does not
        // split on `\` — without explicit separator-normalisation the
        // earlier all-backslash test passed only on Windows. Asserting
        // the forward-slash form locks in the cross-platform contract.
        let stale = PathBuf::from(
            "C:/Users/PASCAL~1.POL/AppData/Local/Temp/.tmp2oZT2Y",
        );
        assert!(
            is_under_temp_dir(&stale),
            "stale AppData/Local/Temp path with forward slashes must also be flagged"
        );
    }

    #[test]
    fn find_attached_vault_returns_none_when_no_paths_have_a_marker() {
        // Empty input + paths that don't exist on disk both yield
        // None — the bootstrap caller falls through to the onboarding
        // wizard exactly as before, no behaviour change.
        let none = find_attached_vault(std::iter::empty()).is_none();
        assert!(none);

        let nonexistent = find_attached_vault(
            vec![PathBuf::from("D:/never-existed"), PathBuf::from("/tmp/no")]
                .into_iter(),
        );
        assert!(nonexistent.is_none());
    }

    #[test]
    fn find_attached_vault_returns_first_path_with_a_brain_marker() {
        // Simulates "user plugged in their BRAIN drive but
        // bootstrap has no saved path for it" — the fallback finds
        // it via the marker file and returns its mount path so the
        // caller can mount + persist.
        use crate::vault::layout::ensure_skeleton;
        use crate::vault::marker::{write_marker, VaultMarker};
        let with_vault = tempfile::TempDir::new().unwrap();
        ensure_skeleton(with_vault.path()).unwrap();
        write_marker(with_vault.path(), &VaultMarker::new("test")).unwrap();

        let without_vault = tempfile::TempDir::new().unwrap();
        // Deliberately no skeleton + marker.

        // Pass the no-vault one first so we can see the fallback
        // *iterates* rather than just picking [0].
        let found = find_attached_vault(
            vec![
                without_vault.path().to_path_buf(),
                with_vault.path().to_path_buf(),
            ]
            .into_iter(),
        );
        assert_eq!(found.as_deref(), Some(with_vault.path()));
    }

    #[test]
    fn is_under_temp_dir_does_not_flag_unrelated_directories_named_temp() {
        // Defensive: a user's own folder happening to contain a "temp"
        // segment outside the canonical AppData\Local\ chain must NOT
        // be rejected. Avoid surprising users with legitimate vault
        // locations like `D:\MyDocs\temp-notes`.
        let mine = PathBuf::from(r"D:\MyDocs\temp-notes");
        assert!(
            !is_under_temp_dir(&mine),
            "unrelated 'temp'-named folder must NOT be flagged: {mine:?}"
        );
    }
}
