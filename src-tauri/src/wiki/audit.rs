//! A5 — scheduled wiki audit.
//!
//! Once per mount (shortly after it, once the initial re-index is done)
//! and then every [`AUDIT_INTERVAL`], the GUI runs the index-backed lint
//! ([`super::lint::lint_with_index`]) and writes the result as Markdown
//! to `00_meta/audit/<YYYY-MM-DD>.md`. The vault's AGENTS.md tells a
//! wiki-maintaining agent to start a maintenance session by working
//! through the newest report — consolidation as a planned operation,
//! without an LLM inside BRAIN.
//!
//! Each run also refreshes the dream queue `00_meta/dream-queue.md`
//! ([`super::dream`], H1 — the "Tiefschlaf" half of dreaming).
//!
//! Findings are never toasted (user decision): the report file and the
//! Integrity page are the only places they show up. The audit is a
//! BACKGROUND task: its label shows in the status bar, but it does not
//! count as an active op, so it never holds up an unmount. Reports older
//! than [`AUDIT_RETENTION_DAYS`] are deleted when a new one is written.
//!
//! `00_meta/audit/` is deliberately outside every sync/commit path:
//!  - it is not in [`super::encryption::MIRRORED_META_FILES`] (a fixed
//!    list of file names: AGENTS.md, CLAUDE.md), so it is never mirrored
//!    into the wiki repository or pushed;
//!  - the watcher only watches `00_meta` non-recursively and drops every
//!    event whose file name is not in that list, so writing a report
//!    does not trigger an auto-commit.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::db::DbHandle;
use crate::state::AppState;
use crate::vault::layout::meta_dir;

use super::lint::{lint_with_index, LintReport};
use super::WikiResult;

/// Name of the report directory inside `00_meta/`.
pub const AUDIT_DIR_NAME: &str = "audit";

/// How often the audit runs while a vault stays mounted.
pub const AUDIT_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Delay before the first audit after a mount, so the mount's background
/// re-index has started and the audit then waits for it (via the DB
/// handle's rebuild lock) instead of linting a half-built index.
const FIRST_RUN_DELAY: Duration = Duration::from_secs(60);

/// How often the scheduler wakes to check "unmounted?" and "24 h over?".
/// Wall-clock based, so a laptop that slept through the night still runs
/// the audit soon after waking.
const WAKE_INTERVAL: Duration = Duration::from_secs(10 * 60);

const OP_LABEL: &str = "Running the wiki audit";

/// Reports older than this many days are deleted when a new one is
/// written, so `00_meta/audit/` does not grow forever.
pub const AUDIT_RETENTION_DAYS: i64 = 30;

/// `00_meta/audit/` of `vault`.
pub fn audit_dir(vault: &Path) -> PathBuf {
    meta_dir(vault).join(AUDIT_DIR_NAME)
}

/// Write `report` to `00_meta/audit/<today>.md` (local date) and return
/// the file's path. Running again on the same day overwrites the file;
/// reports older than [`AUDIT_RETENTION_DAYS`] are deleted.
pub fn write_audit_report(vault: &Path, report: &LintReport) -> std::io::Result<PathBuf> {
    write_audit_report_for_date(vault, report, chrono::Local::now().date_naive())
}

/// [`write_audit_report`] for an explicit date (tests).
pub fn write_audit_report_for_date(
    vault: &Path,
    report: &LintReport,
    date: chrono::NaiveDate,
) -> std::io::Result<PathBuf> {
    let dir = audit_dir(vault);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", date.format("%Y-%m-%d")));
    std::fs::write(&path, render_audit_report(vault, report, date))?;
    prune_old_reports(&dir, date);
    Ok(path)
}

/// Delete `<YYYY-MM-DD>.md` reports dated more than
/// [`AUDIT_RETENTION_DAYS`] before `today`. Other files are left alone;
/// failures are logged, never fatal.
fn prune_old_reports(dir: &Path, today: chrono::NaiveDate) {
    let Some(oldest_kept) = today.checked_sub_days(chrono::Days::new(AUDIT_RETENTION_DAYS as u64))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".md")) else {
            continue;
        };
        let Ok(day) = chrono::NaiveDate::parse_from_str(stem, "%Y-%m-%d") else {
            continue;
        };
        if day < oldest_kept {
            if let Err(err) = std::fs::remove_file(entry.path()) {
                tracing::warn!(?err, report = stem, "could not delete an old audit report");
            }
        }
    }
}

/// The Markdown report: a count per kind, then the findings grouped by
/// kind (errors first, then warnings, then notes). Page paths are shown
/// relative to the vault.
pub fn render_audit_report(vault: &Path, report: &LintReport, date: chrono::NaiveDate) -> String {
    let groups = group_by_kind(report);
    let mut out = String::new();
    let _ = writeln!(out, "# Wiki audit — {}", date.format("%Y-%m-%d"));
    out.push('\n');
    out.push_str(
        "Written by BRAIN's scheduled audit. In a maintenance session, work through the \
         findings below: merge duplicate candidates (`brain_merge_pages`), link, merge or \
         delete orphans (`brain_delete_page`), fix broken links or rename wrongly named \
         pages (`brain_rename_page`). Errors block auto-commits; everything else is advice. \
         Running the audit again on the same day replaces this file.\n",
    );
    out.push('\n');
    out.push_str("## Summary\n\n");
    if groups.is_empty() {
        out.push_str("No findings — the wiki is clean.\n");
        return out;
    }
    out.push_str("| Kind | Severity | Count |\n|---|---|---|\n");
    for group in &groups {
        let _ = writeln!(
            out,
            "| {} | {} | {} |",
            group.kind,
            group.severity,
            group.items.len()
        );
    }
    for group in &groups {
        let _ = write!(out, "\n## {} ({})\n\n", group.kind, group.items.len());
        for (path, message) in &group.items {
            let shown = relative_path(vault, path);
            if shown.is_empty() {
                let _ = writeln!(out, "- {message}");
            } else {
                let _ = writeln!(out, "- `{shown}` — {message}");
            }
        }
    }
    out
}

struct KindGroup<'a> {
    kind: &'a str,
    severity: &'static str,
    items: Vec<(&'a str, &'a str)>,
}

/// `(kind, path, message)` of one finding.
type Finding<'a> = (&'a str, &'a str, &'a str);

fn group_by_kind(report: &LintReport) -> Vec<KindGroup<'_>> {
    let sections: [(&'static str, Vec<Finding<'_>>); 3] = [
        (
            "error",
            report
                .errors
                .iter()
                .map(|e| (e.kind.as_str(), e.path.as_str(), e.message.as_str()))
                .collect(),
        ),
        (
            "warning",
            report
                .warnings
                .iter()
                .map(|w| (w.kind.as_str(), w.path.as_str(), w.message.as_str()))
                .collect(),
        ),
        (
            "note",
            report
                .notes
                .iter()
                .map(|n| (n.kind.as_str(), n.path.as_str(), n.message.as_str()))
                .collect(),
        ),
    ];
    let mut groups: Vec<KindGroup<'_>> = Vec::new();
    for (severity, issues) in sections {
        let mut by_kind: std::collections::BTreeMap<&str, Vec<(&str, &str)>> =
            std::collections::BTreeMap::new();
        for (kind, path, message) in issues {
            by_kind.entry(kind).or_default().push((path, message));
        }
        groups.extend(by_kind.into_iter().map(|(kind, items)| KindGroup {
            kind,
            severity,
            items,
        }));
    }
    groups
}

/// `path` relative to `vault` with forward slashes; unchanged (slashes
/// normalised) when it is not under the vault.
fn relative_path(vault: &Path, path: &str) -> String {
    let p = Path::new(path);
    let rel = p.strip_prefix(vault).unwrap_or(p);
    rel.to_string_lossy().replace('\\', "/")
}

/// Lint `vault` with the index rules and write today's report. Waits for
/// a running page-index rebuild first (the DB handle's rebuild lock), so
/// the orphan and duplicate rules see a complete index.
pub fn run_audit(vault: &Path, db: Option<&DbHandle>) -> WikiResult<PathBuf> {
    let _no_rebuild_meanwhile = db.map(DbHandle::rebuild_guard);
    let report = lint_with_index(vault, db)?;
    let path = write_audit_report(vault, &report)?;
    if let Some(db) = db {
        write_dream_queue(vault, db);
    }
    Ok(path)
}

/// H1: the daily audit ("Tiefschlaf") also refreshes
/// `00_meta/dream-queue.md`. A failure is logged, never fatal — the
/// audit report is already written.
fn write_dream_queue(vault: &Path, db: &DbHandle) {
    if let Err(err) = super::dream::refresh_dream_queue(vault, db, chrono::Utc::now()) {
        tracing::warn!(?err, "could not write the dream queue");
    }
}

/// Abortable handle to the running audit scheduler, stored in
/// [`AppState`] (replaced — and the old one aborted — on every mount).
pub struct AuditHandle {
    handle: tauri::async_runtime::JoinHandle<()>,
}

impl AuditHandle {
    pub fn abort(self) {
        self.handle.abort();
    }

    /// Handle around an arbitrary task — lets tests check that replacing
    /// the scheduler aborts the old one.
    #[cfg(test)]
    pub(crate) fn from_future<F>(future: F) -> Self
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        Self {
            handle: tauri::async_runtime::spawn(future),
        }
    }
}

/// Start the scheduler for `vault`: first audit [`FIRST_RUN_DELAY`] after
/// the call, then every [`AUDIT_INTERVAL`] (wall clock). Never blocks the
/// caller; the scheduler ends by itself once `vault` is no longer the
/// mounted vault.
pub fn spawn(state: Arc<AppState>, vault: PathBuf) -> AuditHandle {
    AuditHandle {
        handle: tauri::async_runtime::spawn(run_loop(state, vault)),
    }
}

async fn run_loop(state: Arc<AppState>, vault: PathBuf) {
    tokio::time::sleep(FIRST_RUN_DELAY).await;
    loop {
        if state.vault_path().as_deref() != Some(vault.as_path()) {
            return;
        }
        let run_state = state.clone();
        let run_vault = vault.clone();
        let outcome = tokio::task::spawn_blocking(move || audit_once(&run_state, &run_vault)).await;
        match outcome {
            Ok(Ok(Some(path))) => {
                tracing::info!(path = %path.display(), "wiki audit report written")
            }
            Ok(Ok(None)) => return,
            Ok(Err(err)) => tracing::warn!(?err, "wiki audit failed"),
            Err(err) => tracing::warn!(?err, "wiki audit task failed"),
        }

        let last_run = SystemTime::now();
        loop {
            tokio::time::sleep(WAKE_INTERVAL).await;
            if state.vault_path().as_deref() != Some(vault.as_path()) {
                return;
            }
            let elapsed = SystemTime::now()
                .duration_since(last_run)
                .unwrap_or(Duration::ZERO);
            if elapsed >= AUDIT_INTERVAL {
                break;
            }
        }
    }
}

/// One audit run, labelled as a background task only while the audit
/// itself runs — not while it waits for a mount's re-index to finish.
/// `Ok(None)` when `vault` was unmounted meanwhile (nothing is written).
fn audit_once(state: &AppState, vault: &Path) -> WikiResult<Option<PathBuf>> {
    let db = state.db();
    let _no_rebuild_meanwhile = db.as_ref().map(DbHandle::rebuild_guard);
    state.begin_background_op(OP_LABEL);
    struct OpGuard<'a>(&'a AppState);
    impl Drop for OpGuard<'_> {
        fn drop(&mut self) {
            self.0.end_background_op(OP_LABEL);
        }
    }
    let _op = OpGuard(state);
    let report = lint_with_index(vault, db.as_ref())?;
    // The lint can take a while; never write into a vault that was
    // unmounted (or swapped for another one) in the meantime.
    if state.vault_path().as_deref() != Some(vault) {
        return Ok(None);
    }
    let path = write_audit_report(vault, &report)?;
    if let Some(db) = db.as_ref() {
        deep_sleep_housekeeping(db);
        write_dream_queue(vault, db);
    }
    Ok(Some(path))
}

/// H1 Tiefschlaf: FTS `optimize` and, if worthwhile, `VACUUM`
/// ([`super::dream::deep_sleep_housekeeping`]). Best-effort: logged,
/// never fatal.
fn deep_sleep_housekeeping(db: &DbHandle) {
    match db.with(super::dream::deep_sleep_housekeeping) {
        Ok(vacuumed) => tracing::info!(vacuumed, "index housekeeping done"),
        Err(err) => tracing::warn!(?err, "index housekeeping failed — retried with the next audit"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::ensure_skeleton;
    use crate::wiki::lint::{LintError, LintWarning};
    use tempfile::TempDir;

    fn date() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 6).unwrap()
    }

    fn warning(vault: &Path, kind: &str, page: &str, message: &str) -> LintWarning {
        LintWarning {
            path: vault.join("02_wiki").join(page).to_string_lossy().to_string(),
            kind: kind.into(),
            message: message.into(),
        }
    }

    fn sample_report(vault: &Path) -> LintReport {
        LintReport {
            errors: vec![LintError {
                path: vault.join("02_wiki/entities/a.md").to_string_lossy().to_string(),
                kind: "broken-link".into(),
                message: "wiki link '[[entities/gone]]' has no target page".into(),
            }],
            warnings: vec![
                warning(vault, "orphan", "entities/old.md", "no other page links to 'entities/old'"),
                warning(
                    vault,
                    "duplicate-candidate",
                    "entities/x.md",
                    "'entities/x' and 'entities/y' may be duplicates (similarity 0.95)",
                ),
                warning(
                    vault,
                    "duplicate-candidate",
                    "entities/p.md",
                    "'entities/p' and 'entities/q' may be duplicates (similarity 0.93)",
                ),
            ],
            notes: Vec::new(),
        }
    }

    fn vault() -> TempDir {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        tmp
    }

    #[test]
    fn the_report_is_written_to_00_meta_audit_named_after_the_date() {
        let tmp = vault();
        let path =
            write_audit_report_for_date(tmp.path(), &sample_report(tmp.path()), date()).unwrap();
        assert_eq!(path, meta_dir(tmp.path()).join("audit").join("2026-10-06.md"));
    }

    #[test]
    fn the_report_summary_counts_the_findings_per_kind() {
        let tmp = vault();
        let text = render_audit_report(tmp.path(), &sample_report(tmp.path()), date());
        assert!(text.contains("| duplicate-candidate | warning | 2 |"), "{text}");
    }

    #[test]
    fn the_report_lists_each_finding_under_its_kind_with_the_vault_relative_path() {
        let tmp = vault();
        let text = render_audit_report(tmp.path(), &sample_report(tmp.path()), date());
        assert!(
            text.contains(
                "## orphan (1)\n\n- `02_wiki/entities/old.md` — no other page links to 'entities/old'"
            ),
            "{text}"
        );
    }

    #[test]
    fn the_report_lists_errors_before_warnings() {
        let tmp = vault();
        let text = render_audit_report(tmp.path(), &sample_report(tmp.path()), date());
        let error_at = text.find("## broken-link").unwrap();
        let warning_at = text.find("## duplicate-candidate").unwrap();
        assert!(error_at < warning_at, "{text}");
    }

    #[test]
    fn a_clean_report_says_the_wiki_is_clean() {
        let tmp = vault();
        let clean = LintReport {
            errors: Vec::new(),
            warnings: Vec::new(),
            notes: Vec::new(),
        };
        let text = render_audit_report(tmp.path(), &clean, date());
        assert!(text.contains("No findings — the wiki is clean."), "{text}");
    }

    #[test]
    fn a_note_without_a_path_is_listed_by_its_message_alone() {
        let tmp = vault();
        let report = LintReport {
            errors: Vec::new(),
            warnings: Vec::new(),
            notes: vec![LintWarning {
                path: String::new(),
                kind: "duplicate-detection-skipped".into(),
                message: "duplicate detection needs the embedding model".into(),
            }],
        };
        let text = render_audit_report(tmp.path(), &report, date());
        assert!(text.contains("\n- duplicate detection needs the embedding model\n"), "{text}");
    }

    #[test]
    fn writing_the_report_twice_on_the_same_day_keeps_only_the_latest_content() {
        let tmp = vault();
        write_audit_report_for_date(tmp.path(), &sample_report(tmp.path()), date()).unwrap();
        let clean = LintReport {
            errors: Vec::new(),
            warnings: Vec::new(),
            notes: Vec::new(),
        };
        let path = write_audit_report_for_date(tmp.path(), &clean, date()).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text, render_audit_report(tmp.path(), &clean, date()));
    }

    #[test]
    fn writing_the_report_twice_on_the_same_day_leaves_a_single_file() {
        let tmp = vault();
        write_audit_report_for_date(tmp.path(), &sample_report(tmp.path()), date()).unwrap();
        write_audit_report_for_date(tmp.path(), &sample_report(tmp.path()), date()).unwrap();
        let files = std::fs::read_dir(audit_dir(tmp.path())).unwrap().count();
        assert_eq!(files, 1);
    }

    #[test]
    fn run_audit_writes_todays_report_from_the_index_lint() {
        let tmp = vault();
        let db = DbHandle::open(tmp.path()).unwrap();
        let path = run_audit(tmp.path(), Some(&db)).unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn run_audit_also_writes_the_dream_queue() {
        let tmp = vault();
        let db = DbHandle::open(tmp.path()).unwrap();
        run_audit(tmp.path(), Some(&db)).unwrap();
        assert!(super::super::dream::dream_queue_path(tmp.path()).is_file());
    }

    #[test]
    fn writing_a_report_deletes_reports_older_than_the_retention_period() {
        let tmp = vault();
        let dir = audit_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-08-01.md"), "old").unwrap();
        write_audit_report_for_date(tmp.path(), &sample_report(tmp.path()), date()).unwrap();
        assert!(!dir.join("2026-08-01.md").exists());
    }

    #[test]
    fn writing_a_report_keeps_reports_within_the_retention_period() {
        let tmp = vault();
        let dir = audit_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-09-10.md"), "recent").unwrap();
        write_audit_report_for_date(tmp.path(), &sample_report(tmp.path()), date()).unwrap();
        assert!(dir.join("2026-09-10.md").exists());
    }

    #[test]
    fn writing_a_report_leaves_files_that_are_not_dated_reports_alone() {
        let tmp = vault();
        let dir = audit_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.md"), "mine").unwrap();
        write_audit_report_for_date(tmp.path(), &sample_report(tmp.path()), date()).unwrap();
        assert!(dir.join("notes.md").exists());
    }

    #[test]
    fn an_audit_that_finishes_after_an_unmount_writes_no_report() {
        let tmp = vault();
        let state = AppState::new();
        // No vault mounted: the run must not write into `tmp`.
        let written = audit_once(&state, tmp.path()).unwrap();
        assert!(written.is_none() && !audit_dir(tmp.path()).exists());
    }

}
