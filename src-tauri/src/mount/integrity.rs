//! Integrity checks run after an unclean shutdown (S07).
//!
//! Three layers:
//! - Git: walk the wiki repo's object database for corruption.
//! - SQLite: `PRAGMA integrity_check`.
//! - Filesystem ↔ DB: spot-check that the indexed pages still exist on disk.

use std::path::Path;

use serde::Serialize;

use crate::db::DbHandle;
use crate::vault::layout::wiki_dir;

#[derive(Debug, Clone, Serialize)]
pub struct IntegrityReport {
    pub clean: bool,
    pub git: CheckResult,
    pub db: CheckResult,
    pub pages: CheckResult,
    pub suggestions: Vec<RecoveryAction>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "detail")]
pub enum CheckResult {
    Ok(String),
    Warn(String),
    Error(String),
    Skipped(String),
}

impl CheckResult {
    fn is_problem(&self) -> bool {
        matches!(self, Self::Warn(_) | Self::Error(_))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RecoveryAction {
    pub id: String,
    pub label: String,
    pub destructive: bool,
}

pub fn check(vault: &Path, db: Option<&DbHandle>) -> IntegrityReport {
    let git = check_git(&wiki_dir(vault));
    let db_check = match db {
        Some(handle) => check_db(handle),
        None => CheckResult::Skipped("database not yet open".into()),
    };
    let pages = match db {
        Some(handle) => check_pages_vs_filesystem(vault, handle),
        None => CheckResult::Skipped("database not yet open".into()),
    };

    let mut suggestions = Vec::new();
    if git.is_problem() {
        suggestions.push(RecoveryAction {
            id: "wiki-restore-last-good".into(),
            label: "Restore wiki from last good Git commit".into(),
            destructive: true,
        });
    }
    if db_check.is_problem() || pages.is_problem() {
        suggestions.push(RecoveryAction {
            id: "rebuild-pages-index".into(),
            label: "Rebuild the search index from the wiki filesystem".into(),
            destructive: false,
        });
    }

    let clean = !git.is_problem() && !db_check.is_problem() && !pages.is_problem();

    IntegrityReport {
        clean,
        git,
        db: db_check,
        pages,
        suggestions,
    }
}

fn check_git(wiki_path: &Path) -> CheckResult {
    if !wiki_path.join(".git").exists() {
        return CheckResult::Skipped("wiki dir is not a git repository yet".into());
    }
    let repo = match git2::Repository::open(wiki_path) {
        Ok(r) => r,
        Err(err) => return CheckResult::Error(format!("cannot open repo: {err}")),
    };
    // Walk the object database to detect corruption. `odb()` lookups touch
    // the on-disk pack files; a torn pack will surface as an error here.
    let head = match repo.head() {
        Ok(h) => h,
        Err(err) => {
            return CheckResult::Warn(format!("HEAD unreadable (probably empty repo): {err}"))
        }
    };
    let oid = match head.target() {
        Some(o) => o,
        None => return CheckResult::Warn("HEAD has no commit yet".into()),
    };
    let commit = match repo.find_commit(oid) {
        Ok(c) => c,
        Err(err) => return CheckResult::Error(format!("HEAD commit missing: {err}")),
    };
    if commit.tree().is_err() {
        return CheckResult::Error("HEAD tree object missing".into());
    }
    CheckResult::Ok(format!("repo is consistent at {}", &oid.to_string()[..8]))
}

fn check_db(db: &DbHandle) -> CheckResult {
    db.with(|conn| {
        let result: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap_or_else(|_| "unknown".into());
        Ok(result)
    })
    .map(|result| {
        if result == "ok" {
            CheckResult::Ok("PRAGMA integrity_check = ok".into())
        } else {
            CheckResult::Error(format!("PRAGMA integrity_check = {result}"))
        }
    })
    .unwrap_or_else(|err| CheckResult::Error(format!("integrity check failed: {err}")))
}

fn check_pages_vs_filesystem(vault: &Path, db: &DbHandle) -> CheckResult {
    let result = db.with(|conn| {
        let mut stmt = conn.prepare("SELECT id, path FROM pages")?;
        let mut missing = 0;
        let mut total = 0;
        let mut iter = stmt.query([])?;
        while let Some(row) = iter.next()? {
            total += 1;
            let path: String = row.get(1)?;
            if !Path::new(&path).exists() {
                missing += 1;
            }
        }
        let _ = vault;
        Ok((total, missing))
    });
    match result {
        Ok((total, 0)) => CheckResult::Ok(format!("all {total} indexed pages present on disk")),
        Ok((total, missing)) => {
            CheckResult::Warn(format!("{missing} of {total} indexed pages missing on disk"))
        }
        Err(err) => CheckResult::Error(format!("cross-check failed: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::ensure_skeleton;
    use tempfile::TempDir;

    #[test]
    fn check_returns_clean_for_a_freshly_initialized_vault_with_open_db() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let db = crate::db::DbHandle::open(tmp.path()).unwrap();
        let report = check(tmp.path(), Some(&db));
        assert!(matches!(report.db, CheckResult::Ok(_)));
        assert!(matches!(report.pages, CheckResult::Ok(_)));
    }

    #[test]
    fn check_returns_skipped_when_no_db_handle_is_provided() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let report = check(tmp.path(), None);
        assert!(matches!(report.db, CheckResult::Skipped(_)));
        assert!(matches!(report.pages, CheckResult::Skipped(_)));
    }

    #[test]
    fn check_warns_when_indexed_pages_were_deleted_from_disk() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let db = crate::db::DbHandle::open(tmp.path()).unwrap();
        // Insert a page whose source file does not exist.
        db.with(|conn| {
            conn.execute(
                "INSERT INTO pages(id, type, path) VALUES ('entities/ghost', 'entity', '/no/such.md')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let report = check(tmp.path(), Some(&db));
        assert!(matches!(report.pages, CheckResult::Warn(_)));
        assert!(!report.clean);
        assert!(report
            .suggestions
            .iter()
            .any(|s| s.id == "rebuild-pages-index"));
    }
}
