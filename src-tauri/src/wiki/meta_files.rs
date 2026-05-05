//! Karpathy-style human-readable catalog (`00_meta/index.md`) and
//! append-only changelog (`00_meta/log.md`).
//!
//! These files are derived from the SQLite index and the auto-commit log.
//! They live in `00_meta/`, **outside** the wiki Git repo, so writes here
//! never trigger the wiki watcher (no commit-loop). They're idempotent:
//! `index.md` is hash-compared before writing; `log.md` is append-only.

use std::path::Path;

use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::db::DbHandle;
use crate::vault::layout::meta_dir;

const INDEX_FILENAME: &str = "index.md";
const LOG_FILENAME: &str = "log.md";
const LOG_ROTATION_LINE_LIMIT: usize = 10_000;

#[derive(Debug, thiserror::Error)]
pub enum MetaFilesError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("db: {0}")]
    Db(#[from] crate::db::DbError),
}

pub type MetaFilesResult<T> = Result<T, MetaFilesError>;

/// Re-renders `00_meta/index.md` from the current SQLite state. No-op if the
/// rendered content is byte-identical to the existing file.
pub fn refresh_index(vault: &Path, db: &DbHandle) -> MetaFilesResult<bool> {
    let body = render_index(db)?;
    let target = meta_dir(vault).join(INDEX_FILENAME);
    if target.exists() {
        if let Ok(existing) = std::fs::read_to_string(&target) {
            if hash(&existing) == hash(&body) {
                return Ok(false);
            }
        }
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, body)?;
    Ok(true)
}

/// Appends a new entry to `00_meta/log.md`. If the file exceeds the
/// rotation threshold, the older 10 % is moved into `log.archive-YYYY.md`.
pub fn append_log(vault: &Path, entry: &LogEntry) -> MetaFilesResult<()> {
    let dir = meta_dir(vault);
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(LOG_FILENAME);

    let mut existing = if target.exists() {
        std::fs::read_to_string(&target).unwrap_or_default()
    } else {
        String::from("# Brain Log — append-only, do not edit\n\n")
    };

    if !existing.ends_with("\n\n") {
        existing.push('\n');
    }
    existing.push_str(&entry.render());
    existing.push('\n');

    if existing.lines().count() > LOG_ROTATION_LINE_LIMIT {
        rotate_log(&dir, &existing)?;
        // After rotation we keep only the most recent 90 %.
        let lines: Vec<&str> = existing.lines().collect();
        let keep_from = lines.len() / 10;
        let kept = lines[keep_from..].join("\n");
        std::fs::write(&target, format!("# Brain Log — append-only, do not edit\n\n{kept}\n"))?;
    } else {
        std::fs::write(&target, existing)?;
    }
    Ok(())
}

fn rotate_log(dir: &Path, existing: &str) -> MetaFilesResult<()> {
    let year = Utc::now().format("%Y").to_string();
    let archive_path = dir.join(format!("log.archive-{year}.md"));
    let lines: Vec<&str> = existing.lines().collect();
    let archive_to = lines.len() / 10;
    let archive_chunk = lines[..archive_to].join("\n");
    let mut existing_archive = if archive_path.exists() {
        std::fs::read_to_string(&archive_path).unwrap_or_default()
    } else {
        format!("# Brain Log Archive {year}\n\n")
    };
    if !existing_archive.ends_with("\n\n") {
        existing_archive.push('\n');
    }
    existing_archive.push_str(&archive_chunk);
    existing_archive.push('\n');
    std::fs::write(archive_path, existing_archive)?;
    Ok(())
}

fn hash(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: String,
    pub commit_sha: Option<String>,
    pub kind: String,
    pub summary: String,
    pub touched: Vec<String>,
}

impl LogEntry {
    pub fn render(&self) -> String {
        let mut out = String::new();
        let header = match &self.commit_sha {
            Some(sha) => format!(
                "## {} · commit `{}` · {}",
                self.timestamp,
                &sha[..8.min(sha.len())],
                self.kind
            ),
            None => format!("## {} · {}", self.timestamp, self.kind),
        };
        out.push_str(&header);
        out.push('\n');
        out.push_str(&self.summary);
        out.push('\n');
        if !self.touched.is_empty() {
            out.push_str("Touched: ");
            out.push_str(&self.touched.join(", "));
            out.push('\n');
        }
        out
    }
}

fn render_index(db: &DbHandle) -> MetaFilesResult<String> {
    let pages: Vec<(String, String, String, Option<String>)> = db.with(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, type, COALESCE(title, id), updated_at FROM pages ORDER BY type, id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })?;

    let total = pages.len();
    let mut by_type: std::collections::BTreeMap<String, Vec<(String, String, Option<String>)>> =
        std::collections::BTreeMap::new();
    for (id, page_type, title, updated) in pages {
        by_type
            .entry(page_type)
            .or_default()
            .push((id, title, updated));
    }

    // Use the most recent page's `updated_at` as the index timestamp, not
    // `Utc::now()` — that way running `refresh_index` twice without any
    // page change produces byte-identical output and we don't churn the
    // file mtime on every commit.
    let latest_updated = db
        .with(|conn| {
            let v: Option<String> = conn
                .query_row(
                    "SELECT MAX(updated_at) FROM pages WHERE updated_at IS NOT NULL AND updated_at != ''",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(None);
            Ok(v)
        })
        .ok()
        .flatten()
        .unwrap_or_else(|| "—".to_string());
    let mut out = String::new();
    out.push_str("# Brain Index — auto-generated, do not edit\n\n");
    out.push_str(&format!(
        "Latest page update: {latest_updated} · {total} page{plural}\n\n",
        plural = if total == 1 { "" } else { "s" }
    ));

    for (page_type, entries) in &by_type {
        out.push_str(&format!(
            "## {} ({})\n\n",
            capitalize(page_type),
            entries.len()
        ));
        for (id, title, updated) in entries {
            let updated_marker = updated
                .as_deref()
                .map(|u| format!(" · _{u}_"))
                .unwrap_or_default();
            out.push_str(&format!("- [[{}]] — {}{}\n", id, title, updated_marker));
        }
        out.push('\n');
    }
    Ok(out)
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::pages_index;
    use crate::vault::layout::{ensure_skeleton, wiki_dir};
    use tempfile::TempDir;

    fn write_page(vault: &Path, sub: &str, slug: &str) {
        let dir = wiki_dir(vault).join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{slug}.md")),
            format!(
                "---\nid: {sub}/{slug}\ntype: {ty}\ntitle: T-{slug}\ncreated: 2026-04-29\nupdated: 2026-04-29\n---\n\nbody\n",
                ty = match sub {
                    "entities" => "entity",
                    "concepts" => "concept",
                    "sources" => "source",
                    "topics" => "topic",
                    _ => "entity",
                }
            ),
        )
        .unwrap();
    }

    #[test]
    fn refresh_index_creates_a_human_readable_catalog_grouped_by_type() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice");
        write_page(tmp.path(), "concepts", "nlspec");
        let db = DbHandle::open(tmp.path()).unwrap();
        pages_index::rebuild(&db, tmp.path()).unwrap();

        let wrote = refresh_index(tmp.path(), &db).unwrap();
        assert!(wrote);

        let path = meta_dir(tmp.path()).join(INDEX_FILENAME);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("auto-generated"));
        assert!(body.contains("entities/alice"));
        assert!(body.contains("concepts/nlspec"));
        assert!(body.contains("Entity"));
        assert!(body.contains("Concept"));
    }

    #[test]
    fn refresh_index_is_byte_identical_when_run_twice_without_changes() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice");
        let db = DbHandle::open(tmp.path()).unwrap();
        pages_index::rebuild(&db, tmp.path()).unwrap();

        let wrote_first = refresh_index(tmp.path(), &db).unwrap();
        let wrote_second = refresh_index(tmp.path(), &db).unwrap();
        assert!(wrote_first);
        assert!(!wrote_second, "second refresh must be a no-op");
    }

    #[test]
    fn append_log_creates_log_md_and_appends_entries_in_order() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let entry1 = LogEntry {
            timestamp: "2026-04-30T10:00:00Z".into(),
            commit_sha: Some("abc12345".into()),
            kind: "MCP write".into(),
            summary: "Added: entities/alice".into(),
            touched: vec!["entities/alice".into()],
        };
        let entry2 = LogEntry {
            timestamp: "2026-04-30T10:01:00Z".into(),
            commit_sha: Some("def67890".into()),
            kind: "auto-commit".into(),
            summary: "Updated: entities/alice".into(),
            touched: vec!["entities/alice".into()],
        };
        append_log(tmp.path(), &entry1).unwrap();
        append_log(tmp.path(), &entry2).unwrap();

        let body = std::fs::read_to_string(meta_dir(tmp.path()).join(LOG_FILENAME)).unwrap();
        let pos1 = body.find("Added: entities/alice").unwrap();
        let pos2 = body.find("Updated: entities/alice").unwrap();
        assert!(pos1 < pos2, "log entries must be appended in order");
    }

    #[test]
    fn log_render_uses_iso8601_timestamp_and_short_sha_for_diff_friendliness() {
        let entry = LogEntry {
            timestamp: "2026-04-30T10:00:00Z".into(),
            commit_sha: Some("abcdef1234567890".into()),
            kind: "MCP write".into(),
            summary: "x".into(),
            touched: vec![],
        };
        let rendered = entry.render();
        assert!(rendered.contains("2026-04-30T10:00:00Z"));
        assert!(rendered.contains("abcdef12"));
        assert!(!rendered.contains("abcdef123456"));
    }
}
