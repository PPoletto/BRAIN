//! S11 phase 6 — background auto-sync scheduler.
//!
//! When enabled (per the `auto_sync` client setting) and a remote is
//! configured, this periodically runs `sync::sync` (fetch → merge → push)
//! and reindexes when the working tree changed. It is spawned on mount and
//! aborted on unmount or when auto-sync is toggled off. Failures (offline,
//! auth) are logged and retried on the next tick — never fatal.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Runtime};

use crate::state::AppState;
use crate::vault::layout::wiki_dir;

use super::sync::{self, MergeOutcome};

/// How often to attempt a background sync.
const INTERVAL: Duration = Duration::from_secs(120);
const OP_LABEL: &str = "Auto-syncing with the remote";

/// Abortable handle to the running scheduler, stored in [`AppState`].
pub struct AutoSyncHandle {
    handle: tauri::async_runtime::JoinHandle<()>,
}

impl AutoSyncHandle {
    pub fn abort(self) {
        self.handle.abort();
    }
}

/// Spawn the scheduler for `vault`. Store the returned handle so it can be
/// aborted (unmount / toggle-off).
pub fn spawn<R: Runtime>(app: AppHandle<R>, state: Arc<AppState>, vault: PathBuf) -> AutoSyncHandle {
    AutoSyncHandle {
        handle: tauri::async_runtime::spawn(run_loop(app, state, vault)),
    }
}

async fn run_loop<R: Runtime>(app: AppHandle<R>, state: Arc<AppState>, vault: PathBuf) {
    let wiki = wiki_dir(&vault);
    loop {
        tokio::time::sleep(INTERVAL).await;

        // The task is aborted when auto-sync is toggled off, but re-check
        // the setting and that a remote is still configured before doing
        // any network work.
        if !state.config.snapshot().auto_sync || sync::remote_url(&wiki).is_none() {
            continue;
        }

        state.begin_op(OP_LABEL);
        let sync_wiki = wiki.clone();
        let outcome = tokio::task::spawn_blocking(move || sync::sync(&sync_wiki)).await;
        state.end_op(OP_LABEL);

        match outcome {
            Ok(Ok(MergeOutcome::UpToDate)) => {}
            Ok(Ok(other)) => {
                // The merge/FF changed the working tree — refresh the index.
                if let Some(db) = state.db() {
                    let reindex_vault = vault.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        crate::db::pages_index::rebuild(&db, &reindex_vault)
                    })
                    .await;
                }
                if let MergeOutcome::Merged { conflicted_pages, .. } = &other {
                    if !conflicted_pages.is_empty() {
                        // Surface conflicts so the UI can prompt the user
                        // to resolve them (same as a manual Sync now).
                        let _ = app.emit("sync-conflicts", conflicted_pages.clone());
                    }
                }
            }
            Ok(Err(err)) => {
                // Offline / auth / transient — retry on the next tick.
                tracing::warn!(error = %err, "auto-sync attempt failed");
            }
            Err(err) => {
                tracing::warn!(error = %err, "auto-sync task join failed");
            }
        }
    }
}
