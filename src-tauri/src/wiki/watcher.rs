//! Wiki filesystem watcher that drives the auto-commit pipeline (S03).
//!
//! Wraps `notify-debouncer-full` with a 5 s idle window. On debounce: runs
//! `lint::lint`. If lint passes, calls `encryption::commit_wiki` with a structured
//! message. If lint fails, increments an error counter and emits a
//! `wiki-lint-error` event so the frontend / tray can surface it.
//!
//! The watcher tracks active operations via the shared `AppState` so the
//! tray can show a busy state while a debounce window is open.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher as _};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use tauri::{AppHandle, Emitter, Runtime};
use tokio::sync::mpsc;

use crate::state::AppState;
use crate::vault::layout::wiki_dir;

use super::{git, history::CommitInfo, lint};

const DEBOUNCE_IDLE: Duration = Duration::from_secs(5);

pub struct WikiWatcher {
    handle: tauri::async_runtime::JoinHandle<()>,
}

impl WikiWatcher {
    pub fn abort(self) {
        self.handle.abort();
    }
}

pub fn spawn<R: Runtime>(
    app: AppHandle<R>,
    state: Arc<AppState>,
    vault_path: PathBuf,
) -> WikiWatcher {
    let handle = tauri::async_runtime::spawn(run_loop(app, state, vault_path));
    WikiWatcher { handle }
}

async fn run_loop<R: Runtime>(
    app: AppHandle<R>,
    state: Arc<AppState>,
    vault_path: PathBuf,
) {
    let wiki = wiki_dir(&vault_path);
    if let Err(err) = std::fs::create_dir_all(&wiki) {
        tracing::error!(?err, "wiki watcher: cannot create wiki dir");
        return;
    }
    if let Err(err) = git::init_repo(&wiki) {
        tracing::error!(?err, "wiki watcher: cannot init git repo");
        return;
    }

    let (tx, mut rx) = mpsc::channel::<()>(64);

    let watcher_tx = tx.clone();
    let mut debouncer = match new_debouncer(
        DEBOUNCE_IDLE,
        None,
        move |result: DebounceEventResult| {
            if let Ok(events) = result {
                // Ignore events under `.git/`. Our own commits rewrite
                // `.git/index` (and refs/objects), and on an encrypted
                // vault the index holds ciphertext while the working tree
                // is plaintext — so git status is perpetually "dirty" and
                // every commit's index write would re-trigger the watcher,
                // producing an endless commit loop. Only real page-file
                // changes (outside `.git/`) should wake the committer.
                let touches_wiki = events
                    .iter()
                    .flat_map(|ev| ev.paths.iter())
                    .any(|p| !p.components().any(|c| c.as_os_str() == std::ffi::OsStr::new(".git")));
                if touches_wiki {
                    let _ = watcher_tx.try_send(());
                }
            }
        },
    ) {
        Ok(d) => d,
        Err(err) => {
            tracing::error!(?err, "wiki watcher: cannot start debouncer");
            return;
        }
    };

    if let Err(err) = debouncer
        .watcher()
        .watch(&wiki, RecursiveMode::Recursive)
    {
        tracing::error!(?err, "wiki watcher: cannot watch wiki dir");
        return;
    }

    while rx.recv().await.is_some() {
        // Drain any further pending notifications that arrived before the
        // debouncer fully quiesced.
        while rx.try_recv().is_ok() {}
        process_idle_window(&app, &state, &wiki).await;
    }

    drop(debouncer);
}

async fn process_idle_window<R: Runtime>(app: &AppHandle<R>, state: &Arc<AppState>, wiki: &Path) {
    state.begin_op("Saving wiki changes");
    let result = run_lint_and_commit(wiki);
    // Refresh the SQLite index regardless of commit outcome — even a lint
    // failure leaves the working tree usable for search until the user fixes
    // the offending page.
    if let (Some(db), Some(vault)) = (state.db(), wiki.parent()) {
        let _ = crate::db::pages_index::rebuild(&db, vault);
        // Refresh the human-readable Karpathy-style index.md.
        // No-op when the rendered content is byte-identical to disk.
        if let Err(err) = crate::wiki::meta_files::refresh_index(vault, &db) {
            tracing::warn!(?err, "could not refresh 00_meta/index.md");
        }
    }
    state.end_op("Saving wiki changes");

    match result {
        Ok(LintCommit::Committed { commit, warnings }) => {
            // Append a log entry alongside the commit so log.md reflects the
            // wiki history without the user having to dig into git.
            if let Some(vault) = wiki.parent() {
                let summary = commit
                    .message
                    .lines()
                    .next()
                    .unwrap_or(&commit.message)
                    .to_string();
                let entry = crate::wiki::meta_files::LogEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    commit_sha: Some(commit.sha.clone()),
                    kind: "auto-commit".into(),
                    summary,
                    touched: Vec::new(),
                };
                if let Err(err) = crate::wiki::meta_files::append_log(vault, &entry) {
                    tracing::warn!(?err, "could not append to 00_meta/log.md");
                }
            }
            let _ = app.emit(
                "wiki-changed",
                serde_json::json!({
                    "commit_sha": commit.sha,
                    "files_changed": commit.files_changed,
                    "message": commit.message,
                }),
            );
            // Surface lint warnings (non-blocking) so the user knows about
            // pages that should be cleaned up — non-canonical wiki links,
            // missing titles, etc. — even though the commit went through.
            if !warnings.is_empty() {
                let _ = app.emit(
                    "wiki-lint-error",
                    serde_json::json!({
                        "errors": Vec::<lint::LintError>::new(),
                        "warnings": warnings,
                    }),
                );
            }
        }
        Ok(LintCommit::NoChanges) => {}
        Ok(LintCommit::LintFailed(report)) => {
            let _ = app.emit(
                "wiki-lint-error",
                serde_json::json!({
                    "errors": report.errors,
                    "warnings": report.warnings,
                }),
            );
        }
        Err(err) => {
            tracing::error!(?err, "wiki watcher: pipeline failed");
        }
    }
}

enum LintCommit {
    Committed {
        commit: CommitInfo,
        /// Soft warnings carried alongside a successful commit. The
        /// commit happens regardless; the warnings travel up so the
        /// frontend can toast them.
        warnings: Vec<lint::LintWarning>,
    },
    NoChanges,
    LintFailed(lint::LintReport),
}

fn run_lint_and_commit(wiki: &Path) -> super::WikiResult<LintCommit> {
    let vault = wiki.parent().unwrap_or(wiki).to_path_buf();
    let report = lint::lint(&vault)?;
    if !report.is_clean() {
        return Ok(LintCommit::LintFailed(report));
    }
    let warnings = report.warnings;
    let summary = summarize_changes(wiki)?;
    if summary.is_empty() {
        return Ok(LintCommit::NoChanges);
    }
    let message = commit_message(&summary, super::encryption::is_encrypted(&vault));
    // commit_wiki (not git::commit_all) so an encrypted vault stores
    // ciphertext — this is what keeps encryption sticky across edits.
    match super::encryption::commit_wiki(wiki, &message)? {
        Some(sha) => Ok(LintCommit::Committed {
            commit: CommitInfo {
                sha,
                ts: chrono::Utc::now().to_rfc3339(),
                message,
                files_changed: summary.len() as u32,
            },
            warnings,
        }),
        None => Ok(LintCommit::NoChanges),
    }
}

fn summarize_changes(wiki: &Path) -> super::WikiResult<Vec<String>> {
    let repo = git::init_repo(wiki)?;
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let statuses = repo.statuses(Some(&mut opts))?;
    let mut paths = Vec::new();
    for s in statuses.iter() {
        if s.status() == git2::Status::CURRENT {
            continue;
        }
        if let Some(p) = s.path() {
            paths.push(p.to_string());
        }
    }
    Ok(paths)
}

fn build_commit_message(paths: &[String]) -> String {
    let n = paths.len();
    let preview: Vec<&str> = paths.iter().take(3).map(|s| s.as_str()).collect();
    let suffix = if n > preview.len() {
        format!(" (+{} more)", n - preview.len())
    } else {
        String::new()
    };
    format!(
        "wiki: {n} change{plural} — {preview}{suffix}",
        plural = if n == 1 { "" } else { "s" },
        preview = preview.join(", ")
    )
}

/// The commit message for an auto-commit. On an encrypted vault it is
/// path-FREE (a bare change count): commit messages get pushed, and a
/// just-created page is still at its plaintext name at this point (the
/// opaque-rename happens inside `commit_wiki`), so listing paths would
/// leak the name through the message. Plaintext vaults keep the detailed
/// path list.
fn commit_message(paths: &[String], encrypted: bool) -> String {
    if encrypted {
        let n = paths.len();
        format!("wiki: {n} change{}", if n == 1 { "" } else { "s" })
    } else {
        build_commit_message(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_message_is_path_free_on_an_encrypted_vault() {
        // The leak guard: page paths (which can be plaintext for a
        // just-created file) must never reach a pushed commit message.
        let paths = vec![
            "entities/michael-simon.md".to_string(),
            "concepts/nl-spec.md".to_string(),
        ];
        let msg = commit_message(&paths, true);
        assert_eq!(msg, "wiki: 2 changes");
        assert!(!msg.contains("michael"), "no page name may appear: {msg}");
        assert!(!msg.contains(".md"), "no paths at all: {msg}");
        assert_eq!(commit_message(&["entities/a.md".to_string()], true), "wiki: 1 change");
    }

    #[test]
    fn commit_message_keeps_the_detailed_list_on_a_plaintext_vault() {
        let paths = vec!["entities/alice.md".to_string()];
        assert!(
            commit_message(&paths, false).contains("entities/alice.md"),
            "plaintext vault keeps the informative path list"
        );
    }

    #[test]
    fn build_commit_message_lists_first_three_paths_and_total_count() {
        let msg = build_commit_message(&[
            "entities/alice.md".into(),
            "entities/bob.md".into(),
            "concepts/nlspec.md".into(),
            "topics/x.md".into(),
        ]);
        assert!(msg.contains("4 changes"));
        assert!(msg.contains("entities/alice.md"));
        assert!(msg.contains("(+1 more)"));
    }

    #[test]
    fn build_commit_message_uses_singular_when_only_one_change() {
        let msg = build_commit_message(&["entities/alice.md".into()]);
        assert!(msg.contains("1 change "));
        assert!(!msg.contains("changes"));
    }

    #[test]
    fn build_commit_message_omits_plus_more_when_under_threshold() {
        let msg = build_commit_message(&["a".into(), "b".into()]);
        assert!(!msg.contains("(+"));
    }
}
