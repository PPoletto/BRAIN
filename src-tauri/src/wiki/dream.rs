//! H1 — the dream queue ("Tiefschlaf").
//!
//! BRAIN has no LLM (C-04/C-08), so consolidating the wiki is split:
//! BRAIN does the mechanical part — it collects what needs attention into
//! a prioritised work list, `00_meta/dream-queue.md` — and an agent, when
//! the user triggers a dream session ("träum mal"), works through it over
//! MCP (`brain_dream_queue`, then merge / rename / patch / write a
//! summary) and notes what it did in `00_meta/dream-log.md`
//! (`brain_dream_log`).
//!
//! Item kinds, highest priority first:
//!
//! | priority | kind | suggested action |
//! |---|---|---|
//! | 1 | `duplicate-candidate` (hygiene pair, model index only) | `merge` |
//! | 1 | `broken-link` (links to missing pages, per source page) | `fix-link` |
//! | 1 | `broken-source` (a `sources` entry without a page) | `fix-link` |
//! | 2 | `summary-stale` (body changed since the summary was written) | `update-summary` |
//! | 2 | `missing-summary` on a hub (≥ [`HUB_MIN_INBOUND`] inbound links) | `write-summary` |
//! | 3 | `decay-candidate` (never read, no inbound links, unchanged > 90 days) | `archive-or-supersede` |
//! | 3 | `orphan` (no inbound links, unchanged > 90 days, but read) | `review-or-archive` |
//!
//! Every page appears in at most ONE item: an item whose pages are
//! already covered by a higher-ranked item is left out (the next queue
//! brings it back once that item is done). Exception: `merge` and
//! `fix-link` items never hide each other. The summary rules are skipped
//! (with a note) until a complete rebuild has recorded the summaries.
//! At most [`MAX_ITEMS`] items.
//! Nothing here ever deletes: decay is always "archive or supersede".
//!
//! The DB part ([`load_dream_rows`]) runs under the connection lock (the
//! MCP server bounds it with `db_op`); [`build_queue`] is pure.
//!
//! Both files are LOCAL: not in `encryption::MIRRORED_META_FILES`, and
//! the watcher ignores every `00_meta` file outside that list, so
//! writing them never triggers an auto-commit.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::db::{DbHandle, DbResult};
use crate::vault::layout::meta_dir;

use super::hygiene::{HygieneRows, load_rows};

pub const DREAM_QUEUE_FILENAME: &str = "dream-queue.md";
pub const DREAM_LOG_FILENAME: &str = "dream-log.md";

/// Most items in one queue.
pub const MAX_ITEMS: usize = 50;

/// A page with at least this many inbound links is a hub; a hub without
/// `summary` gets a `write-summary` item.
pub const HUB_MIN_INBOUND: usize = 5;

/// A never-read page without inbound links becomes a decay candidate once
/// its file has not changed for this many days.
pub const DECAY_MIN_AGE_DAYS: i64 = 90;

/// `brain_dream_queue` recomputes a queue older than this.
pub const MAX_QUEUE_AGE: chrono::Duration = chrono::Duration::hours(1);

/// Longest `brain_dream_log` entry kept.
const MAX_LOG_ENTRY_CHARS: usize = 2000;

/// Marker of the machine-readable copy of the queue at the end of the
/// Markdown file (an HTML comment, invisible when rendered).
const JSON_MARKER: &str = "<!-- dream-queue-json ";

/// One thing for the dreaming agent to look at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DreamItem {
    /// 1 (most urgent) … 3.
    pub priority: u8,
    pub kind: String,
    pub pages: Vec<String>,
    pub reason: String,
    pub suggested_action: String,
}

/// The prioritised work list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DreamQueue {
    /// RFC 3339, UTC.
    pub generated_at: String,
    pub items: Vec<DreamItem>,
    /// Items left out because of the [`MAX_ITEMS`] cap.
    #[serde(default)]
    pub omitted: usize,
    /// Checks that were skipped, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Everything the queue needs from the index.
#[derive(Debug, Default)]
pub struct DreamRows {
    hygiene: HygieneRows,
    /// (source page, missing target).
    broken_links: Vec<(String, String)>,
    /// (page, missing source id).
    broken_sources: Vec<(String, String)>,
    /// Page id → (has summary, summary is stale).
    summaries: HashMap<String, (bool, bool)>,
    /// Page id → reads (pages without a `page_access` row are absent).
    reads: HashMap<String, i64>,
    /// Whether the summary columns are filled (see
    /// `pages_index::summaries_indexed`).
    summaries_indexed: bool,
}

pub fn dream_queue_path(vault: &Path) -> PathBuf {
    meta_dir(vault).join(DREAM_QUEUE_FILENAME)
}

pub fn dream_log_path(vault: &Path) -> PathBuf {
    meta_dir(vault).join(DREAM_LOG_FILENAME)
}

/// All DB reads of the dream queue.
pub fn load_dream_rows(conn: &rusqlite::Connection) -> DbResult<DreamRows> {
    let hygiene = load_rows(conn)?;
    let pairs = |sql: &str| -> DbResult<Vec<(String, String)>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    };
    let broken_links = pairs(
        "SELECT src_id, dst_id FROM wiki_links \
         WHERE dst_id NOT IN (SELECT id FROM pages) ORDER BY src_id, dst_id",
    )?;
    let broken_sources = pairs(
        "SELECT page_id, source_id FROM page_sources \
         WHERE source_id NOT IN (SELECT id FROM pages) ORDER BY page_id, source_id",
    )?;
    let summaries = {
        let mut stmt = conn.prepare(
            "SELECT id, summary IS NOT NULL, \
                    summary IS NOT NULL AND summary_body_hash IS NOT NULL \
                    AND body_hash IS NOT NULL AND summary_body_hash <> body_hash \
             FROM pages",
        )?;
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?)))
        })?
        .collect::<Result<HashMap<_, _>, _>>()?
    };
    let reads = {
        let mut stmt = conn.prepare("SELECT page_id, reads FROM page_access")?;
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<HashMap<_, _>, _>>()?
    };
    Ok(DreamRows {
        hygiene,
        broken_links,
        broken_sources,
        summaries,
        reads,
        summaries_indexed: crate::db::pages_index::summaries_indexed(conn),
    })
}

/// Build the queue from `rows` as of `now`. Pure.
pub fn build_queue(rows: &DreamRows, now: chrono::DateTime<chrono::Utc>) -> DreamQueue {
    let now_unix = now.timestamp();
    let mut candidates: Vec<DreamItem> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // Priority 1: duplicates first (the costliest kind of drift), then
    // broken links (one item per source page) and broken sources.
    for (a, b, score) in rows.hygiene.duplicate_pairs() {
        candidates.push(item(
            1,
            "duplicate-candidate",
            vec![a, b],
            format!("pages read almost the same (similarity {score:.2})"),
            "merge",
        ));
    }
    let mut by_src: Vec<(String, Vec<String>)> = Vec::new();
    for (src, dst) in &rows.broken_links {
        match by_src.last_mut() {
            Some((s, targets)) if s == src => targets.push(dst.clone()),
            _ => by_src.push((src.clone(), vec![dst.clone()])),
        }
    }
    for (src, targets) in by_src {
        candidates.push(item(
            1,
            "broken-link",
            vec![src],
            format!("links to missing page(s): {}", targets.join(", ")),
            "fix-link",
        ));
    }
    for (page, source) in &rows.broken_sources {
        candidates.push(item(
            1,
            "broken-source",
            vec![page.clone()],
            format!("sources entry '{source}' has no page"),
            "fix-link",
        ));
    }

    // Priority 2: summaries — only once the index holds them. Right after
    // the upgrade (before the first complete rebuild with metadata
    // version 2) a NULL summary may just mean "not indexed yet".
    let facts = rows.hygiene.page_facts();
    if !rows.summaries_indexed {
        notes.push(
            "summary checks skipped: the index has not recorded page summaries yet — they \
             appear after the next complete index rebuild"
                .into(),
        );
    }
    for page in facts.iter().filter(|_| rows.summaries_indexed) {
        if rows
            .summaries
            .get(&page.id)
            .is_some_and(|(_, stale)| *stale)
        {
            candidates.push(item(
                2,
                "summary-stale",
                vec![page.id.clone()],
                "the body changed since the summary was written".into(),
                "update-summary",
            ));
        }
    }
    for page in facts.iter().filter(|_| rows.summaries_indexed) {
        let has_summary = rows.summaries.get(&page.id).is_some_and(|(has, _)| *has);
        if !has_summary && page.inbound >= HUB_MIN_INBOUND {
            candidates.push(item(
                2,
                "missing-summary",
                vec![page.id.clone()],
                format!("{} pages link here, but it has no summary", page.inbound),
                "write-summary",
            ));
        }
    }

    // Priority 3: decay candidates, then the remaining orphans.
    let cutoff = now_unix - DECAY_MIN_AGE_DAYS * 24 * 60 * 60;
    for page in &facts {
        let never_read = rows.reads.get(&page.id).copied().unwrap_or(0) == 0;
        if never_read && page.inbound == 0 && page.mtime > 0 && page.mtime < cutoff {
            candidates.push(item(
                3,
                "decay-candidate",
                vec![page.id.clone()],
                format!(
                    "never read on this machine, no page links here, unchanged for more than \
                     {DECAY_MIN_AGE_DAYS} days"
                ),
                "archive-or-supersede",
            ));
        }
    }
    for id in rows.hygiene.orphan_ids(now_unix) {
        candidates.push(item(
            3,
            "orphan",
            vec![id],
            format!(
                "no other page links here and it is unchanged for more than {} days",
                super::hygiene::ORPHAN_MIN_AGE_DAYS
            ),
            "review-or-archive",
        ));
    }

    // Candidates are in rank order (priority, then kind order above, then
    // page order); keep the first item per page — except that merge and
    // fix-link items never hide each other, so a lingering broken link
    // cannot hold back a merge (or the other way round) forever.
    candidates.sort_by_key(|i| i.priority);
    let mut covered: HashMap<String, Vec<String>> = HashMap::new();
    let mut items: Vec<DreamItem> = Vec::new();
    for candidate in candidates {
        let blocked = candidate.pages.iter().any(|p| {
            covered.get(p).is_some_and(|actions| {
                actions
                    .iter()
                    .any(|a| !exempt_from_dedupe(a, &candidate.suggested_action))
            })
        });
        if blocked {
            continue;
        }
        for p in &candidate.pages {
            covered
                .entry(p.clone())
                .or_default()
                .push(candidate.suggested_action.clone());
        }
        items.push(candidate);
    }
    let omitted = items.len().saturating_sub(MAX_ITEMS);
    items.truncate(MAX_ITEMS);
    DreamQueue {
        generated_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        items,
        omitted,
        notes,
    }
}

/// Whether an item with action `a` may share a page with one of action
/// `b` (only merge ↔ fix-link).
fn exempt_from_dedupe(a: &str, b: &str) -> bool {
    matches!((a, b), ("merge", "fix-link") | ("fix-link", "merge"))
}

fn item(priority: u8, kind: &str, pages: Vec<String>, reason: String, action: &str) -> DreamItem {
    DreamItem {
        priority,
        kind: kind.into(),
        pages,
        reason,
        suggested_action: action.into(),
    }
}

/// Deep-sleep housekeeping of the index, run by the daily audit:
/// merges the FTS b-trees (`optimize`) and, when more than 20 % of the
/// database file is free pages, `VACUUM`s it. Returns whether it
/// vacuumed. Callers treat failures as best-effort (log, continue): a
/// `VACUUM` fails with `SQLITE_BUSY` while another process (an MCP
/// server) is reading, and is simply tried again the next day.
pub fn deep_sleep_housekeeping(conn: &rusqlite::Connection) -> DbResult<bool> {
    conn.execute("INSERT INTO pages_fts(pages_fts) VALUES('optimize')", [])?;
    let free: i64 = conn.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
    let total: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    if !needs_vacuum(free, total) {
        return Ok(false);
    }
    conn.execute_batch("VACUUM")?;
    Ok(true)
}

/// `VACUUM` pays off once free pages exceed 20 % of the file.
pub fn needs_vacuum(freelist_pages: i64, total_pages: i64) -> bool {
    total_pages > 0 && freelist_pages * 5 > total_pages
}

/// [`load_dream_rows`] + [`build_queue`] on a GUI-side handle.
pub fn build_dream_queue(
    db: &DbHandle,
    now: chrono::DateTime<chrono::Utc>,
) -> DbResult<DreamQueue> {
    let rows = db.with(load_dream_rows)?;
    Ok(build_queue(&rows, now))
}

/// Build the queue and write `00_meta/dream-queue.md`.
pub fn refresh_dream_queue(
    vault: &Path,
    db: &DbHandle,
    now: chrono::DateTime<chrono::Utc>,
) -> DbResult<DreamQueue> {
    let queue = build_dream_queue(db, now)?;
    write_dream_queue(vault, &queue)?;
    Ok(queue)
}

/// Write `queue` as Markdown (with a machine-readable copy in a trailing
/// HTML comment, which [`read_queue_file`] parses back).
pub fn write_dream_queue(vault: &Path, queue: &DreamQueue) -> std::io::Result<PathBuf> {
    let path = dream_queue_path(vault);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, render_queue(queue))?;
    Ok(path)
}

/// The Markdown form of the queue.
pub fn render_queue(queue: &DreamQueue) -> String {
    let mut out = String::from("# Dream queue\n\n");
    let _ = writeln!(
        out,
        "generated {}, items {}",
        queue.generated_at,
        queue.items.len()
    );
    out.push('\n');
    out.push_str(
        "Written by BRAIN. In a dream session, work top-down: at most 10 changes per session, \
         never delete a page other pages link to, supersede instead of overwriting, and note \
         what you did with `brain_dream_log`. Call `brain_dream_queue` for the live list.\n",
    );
    if queue.items.is_empty() {
        out.push_str("\nNothing to do — the wiki is in order.\n");
    } else {
        out.push_str("\n| # | Priority | Kind | Pages | Reason | Suggested action |\n");
        out.push_str("|---|---|---|---|---|---|\n");
        for (i, it) in queue.items.iter().enumerate() {
            let pages: Vec<String> = it.pages.iter().map(|p| format!("`{p}`")).collect();
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} |",
                i + 1,
                it.priority,
                it.kind,
                pages.join(", "),
                it.reason.replace('|', "\\|"),
                it.suggested_action
            );
        }
    }
    if queue.omitted > 0 {
        let _ = writeln!(
            out,
            "\n{} more items left out (limit {MAX_ITEMS}).",
            queue.omitted
        );
    }
    for note in &queue.notes {
        let _ = writeln!(out, "\nNote: {note}");
    }
    let json = serde_json::to_string(queue)
        .unwrap_or_default()
        .replace("--", "\\u002d\\u002d");
    let _ = write!(out, "\n{JSON_MARKER}{json} -->\n");
    out
}

/// Parse a queue file written by [`write_dream_queue`]; `None` when the
/// file is missing or has no readable machine copy.
pub fn read_queue_file(path: &Path) -> Option<DreamQueue> {
    let text = std::fs::read_to_string(path).ok()?;
    let start = text.rfind(JSON_MARKER)? + JSON_MARKER.len();
    let rest = &text[start..];
    let end = rest.rfind("-->")?;
    serde_json::from_str(rest[..end].trim()).ok()
}

/// The cached queue of `vault` if it was generated less than
/// [`MAX_QUEUE_AGE`] before `now` (judged by its `generated_at`, not the
/// file time). `None` → recompute.
pub fn cached_queue(vault: &Path, now: chrono::DateTime<chrono::Utc>) -> Option<DreamQueue> {
    let queue = read_queue_file(&dream_queue_path(vault))?;
    let generated = chrono::DateTime::parse_from_rfc3339(&queue.generated_at).ok()?;
    let age = now.signed_duration_since(generated.with_timezone(&chrono::Utc));
    (age >= chrono::Duration::zero() && age < MAX_QUEUE_AGE).then_some(queue)
}

/// Append one dated line to `00_meta/dream-log.md` (created with a
/// header on first use). The entry is collapsed to one line and capped
/// at 2000 characters; an empty entry is refused.
pub fn append_dream_log(
    vault: &Path,
    entry: &str,
    when: chrono::DateTime<chrono::Local>,
) -> std::io::Result<String> {
    use std::io::Write as _;
    let mut line: String = entry.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the log entry must not be empty",
        ));
    }
    if line.chars().count() > MAX_LOG_ENTRY_CHARS {
        line = line.chars().take(MAX_LOG_ENTRY_CHARS - 1).collect();
        line.push('…');
    }
    let path = dream_log_path(vault);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let is_new = !path.is_file();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if is_new {
        file.write_all(
            b"# Dream log\n\nOne line per dream-session note (brain_dream_log). Local file - not synced.\n\n",
        )?;
    }
    let written = format!("- {} {line}", when.format("%Y-%m-%d %H:%M"));
    file.write_all(format!("{written}\n").as_bytes())?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::vec_to_bytes;
    use tempfile::TempDir;

    const DAY: i64 = 24 * 60 * 60;
    /// 2026-10-06T00:00:00Z.
    const NOW: i64 = 1_791_244_800;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(NOW, 0).unwrap()
    }

    /// A DB whose summary columns count as indexed (metadata version 2).
    fn open_db() -> (TempDir, DbHandle) {
        let (tmp, db) = open_unindexed_db();
        exec(
            &db,
            "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('index_meta_version', '2')",
            &[],
        );
        (tmp, db)
    }

    /// A DB on which no rebuild has filled the summary columns yet.
    fn open_unindexed_db() -> (TempDir, DbHandle) {
        let tmp = TempDir::new().unwrap();
        crate::vault::layout::ensure_skeleton(tmp.path()).unwrap();
        let db = DbHandle::open(tmp.path()).unwrap();
        (tmp, db)
    }

    fn exec(db: &DbHandle, sql: &str, params: &[&dyn rusqlite::ToSql]) {
        db.with(|conn| {
            conn.execute(sql, params)?;
            Ok(())
        })
        .unwrap();
    }

    fn page(db: &DbHandle, id: &str, mtime: i64) {
        exec(
            db,
            "INSERT INTO pages(id, type, path, file_mtime) VALUES (?1, 'entity', ?2, ?3)",
            &[&id, &format!("02_wiki/{id}.md"), &mtime],
        );
    }

    fn link(db: &DbHandle, src: &str, dst: &str) {
        exec(
            db,
            "INSERT INTO wiki_links(src_id, dst_id) VALUES (?1, ?2)",
            &[&src, &dst],
        );
    }

    fn read(db: &DbHandle, id: &str) {
        db.with(|conn| crate::db::pages_index::record_reads(conn, &[id.to_string()], NOW))
            .unwrap();
    }

    fn queue(db: &DbHandle) -> DreamQueue {
        build_dream_queue(db, now()).unwrap()
    }

    fn kinds_of(q: &DreamQueue, id: &str) -> Vec<String> {
        q.items
            .iter()
            .filter(|i| i.pages.iter().any(|p| p == id))
            .map(|i| i.kind.clone())
            .collect()
    }

    #[test]
    fn a_link_to_a_missing_page_gives_a_fix_link_item_for_the_linking_page() {
        let (_tmp, db) = open_db();
        page(&db, "entities/a", NOW);
        link(&db, "entities/a", "entities/gone");
        let q = queue(&db);
        assert_eq!(
            (
                q.items[0].kind.as_str(),
                q.items[0].suggested_action.as_str(),
                q.items[0].pages.clone()
            ),
            ("broken-link", "fix-link", vec!["entities/a".to_string()])
        );
    }

    #[test]
    fn a_sources_entry_without_a_page_gives_a_broken_source_item() {
        let (_tmp, db) = open_db();
        page(&db, "entities/a", NOW);
        exec(
            &db,
            "INSERT INTO page_sources(page_id, source_id) VALUES ('entities/a', 'sources/gone')",
            &[],
        );
        assert_eq!(kinds_of(&queue(&db), "entities/a"), vec!["broken-source"]);
    }

    /// Pages a and b with identical vectors in a model-embedded index.
    fn duplicate_pair(db: &DbHandle) {
        exec(
            db,
            "INSERT INTO schema_meta(key, value) VALUES ('index_embedder', 'bge-m3')",
            &[],
        );
        for id in ["entities/a", "entities/b"] {
            page(db, id, NOW);
            exec(
                db,
                "INSERT INTO page_vectors(page_id, embedding) VALUES (?1, ?2)",
                &[&id, &vec_to_bytes(&[1.0, 0.0])],
            );
        }
    }

    #[test]
    fn a_duplicate_pair_comes_before_broken_links_among_the_first_priority_items() {
        let (_tmp, db) = open_db();
        page(&db, "entities/c", NOW);
        link(&db, "entities/c", "entities/gone");
        duplicate_pair(&db);
        let kinds: Vec<String> = queue(&db).items.iter().map(|i| i.kind.clone()).collect();
        assert_eq!(kinds, vec!["duplicate-candidate", "broken-link"]);
    }

    #[test]
    fn a_broken_link_on_a_page_of_a_duplicate_pair_does_not_hide_either_item() {
        let (_tmp, db) = open_db();
        duplicate_pair(&db);
        link(&db, "entities/a", "entities/gone");
        let actions: Vec<String> = queue(&db)
            .items
            .iter()
            .map(|i| i.suggested_action.clone())
            .collect();
        assert_eq!(actions, vec!["merge", "fix-link"]);
    }

    #[test]
    fn summary_rules_are_skipped_until_the_index_has_recorded_summaries() {
        let (_tmp, db) = open_unindexed_db();
        hub_with_inbound(&db, 5);
        assert!(kinds_of(&queue(&db), "entities/hub").is_empty());
    }

    #[test]
    fn skipped_summary_rules_are_explained_in_a_note() {
        let (_tmp, db) = open_unindexed_db();
        let q = queue(&db);
        assert!(
            q.notes.len() == 1 && q.notes[0].starts_with("summary checks skipped"),
            "{:?}",
            q.notes
        );
    }

    #[test]
    fn vacuum_is_needed_once_more_than_a_fifth_of_the_pages_are_free() {
        assert_eq!(
            (needs_vacuum(20, 100), needs_vacuum(21, 100)),
            (false, true)
        );
    }

    #[test]
    fn deep_sleep_housekeeping_optimises_a_fresh_index_without_vacuuming() {
        let (_tmp, db) = open_db();
        assert!(!db.with(deep_sleep_housekeeping).unwrap());
    }

    #[test]
    fn a_duplicate_pair_gives_one_merge_item_naming_both_pages() {
        let (_tmp, db) = open_db();
        duplicate_pair(&db);
        let merges: Vec<Vec<String>> = queue(&db)
            .items
            .iter()
            .filter(|i| i.suggested_action == "merge")
            .map(|i| i.pages.clone())
            .collect();
        assert_eq!(
            merges,
            vec![vec!["entities/a".to_string(), "entities/b".to_string()]]
        );
    }

    #[test]
    fn a_summary_whose_body_changed_since_gives_an_update_summary_item() {
        let (_tmp, db) = open_db();
        page(&db, "entities/a", NOW);
        exec(
            &db,
            "UPDATE pages SET summary = 's', body_hash = 'new', summary_body_hash = 'old'",
            &[],
        );
        assert_eq!(kinds_of(&queue(&db), "entities/a"), vec!["summary-stale"]);
    }

    #[test]
    fn a_summary_written_with_the_current_body_gives_no_item() {
        let (_tmp, db) = open_db();
        page(&db, "entities/a", NOW);
        exec(
            &db,
            "UPDATE pages SET summary = 's', body_hash = 'h', summary_body_hash = 'h'",
            &[],
        );
        assert!(queue(&db).items.is_empty());
    }

    fn hub_with_inbound(db: &DbHandle, n: usize) {
        page(db, "entities/hub", NOW);
        for i in 0..n {
            let src = format!("entities/s{i}");
            page(db, &src, NOW);
            link(db, &src, "entities/hub");
        }
    }

    #[test]
    fn a_hub_with_five_inbound_links_and_no_summary_gives_a_write_summary_item() {
        let (_tmp, db) = open_db();
        hub_with_inbound(&db, 5);
        assert_eq!(
            kinds_of(&queue(&db), "entities/hub"),
            vec!["missing-summary"]
        );
    }

    #[test]
    fn a_page_with_four_inbound_links_and_no_summary_is_not_a_hub() {
        let (_tmp, db) = open_db();
        hub_with_inbound(&db, 4);
        assert!(kinds_of(&queue(&db), "entities/hub").is_empty());
    }

    #[test]
    fn an_old_unlinked_never_read_page_is_a_decay_candidate() {
        let (_tmp, db) = open_db();
        page(&db, "entities/old", NOW - 120 * DAY);
        let q = queue(&db);
        assert_eq!(
            (
                kinds_of(&q, "entities/old"),
                q.items[0].suggested_action.as_str()
            ),
            (vec!["decay-candidate".to_string()], "archive-or-supersede")
        );
    }

    #[test]
    fn an_old_unlinked_page_that_was_read_is_an_orphan_to_review() {
        let (_tmp, db) = open_db();
        page(&db, "entities/old", NOW - 120 * DAY);
        read(&db, "entities/old");
        let q = queue(&db);
        assert_eq!(
            (
                kinds_of(&q, "entities/old"),
                q.items[0].suggested_action.as_str()
            ),
            (vec!["orphan".to_string()], "review-or-archive")
        );
    }

    #[test]
    fn a_recent_unlinked_never_read_page_is_not_a_decay_candidate() {
        let (_tmp, db) = open_db();
        page(&db, "entities/new", NOW - 10 * DAY);
        assert!(queue(&db).items.is_empty());
    }

    #[test]
    fn a_page_appears_in_only_its_highest_priority_item() {
        let (_tmp, db) = open_db();
        page(&db, "entities/old", NOW - 120 * DAY);
        link(&db, "entities/old", "entities/gone");
        assert_eq!(kinds_of(&queue(&db), "entities/old"), vec!["broken-link"]);
    }

    #[test]
    fn items_are_ordered_by_priority() {
        let (_tmp, db) = open_db();
        page(&db, "entities/old", NOW - 120 * DAY);
        page(&db, "entities/a", NOW);
        link(&db, "entities/a", "entities/gone");
        let priorities: Vec<u8> = queue(&db).items.iter().map(|i| i.priority).collect();
        assert_eq!(priorities, vec![1, 3]);
    }

    #[test]
    fn the_queue_is_capped_at_fifty_items_and_counts_the_rest() {
        let (_tmp, db) = open_db();
        for i in 0..60 {
            page(&db, &format!("entities/p{i:02}"), NOW - 120 * DAY);
        }
        let q = queue(&db);
        assert_eq!((q.items.len(), q.omitted), (50, 10));
    }

    // ---- file + cache --------------------------------------------------------

    fn sample_queue(at: chrono::DateTime<chrono::Utc>) -> DreamQueue {
        DreamQueue {
            generated_at: at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            items: vec![item(
                1,
                "broken-link",
                vec!["entities/a".into()],
                "links to missing page(s): entities/x--y | z".into(),
                "fix-link",
            )],
            omitted: 0,
            notes: vec!["a note".into()],
        }
    }

    #[test]
    fn the_queue_file_names_its_generation_time_and_item_count_under_the_title() {
        let tmp = TempDir::new().unwrap();
        write_dream_queue(tmp.path(), &sample_queue(now())).unwrap();
        let text = std::fs::read_to_string(dream_queue_path(tmp.path())).unwrap();
        assert_eq!(
            text.lines().nth(2),
            Some("generated 2026-10-06T00:00:00Z, items 1")
        );
    }

    #[test]
    fn a_written_queue_file_parses_back_to_the_same_queue() {
        let tmp = TempDir::new().unwrap();
        let q = sample_queue(now());
        write_dream_queue(tmp.path(), &q).unwrap();
        assert_eq!(read_queue_file(&dream_queue_path(tmp.path())), Some(q));
    }

    #[test]
    fn a_queue_younger_than_an_hour_is_served_from_the_file() {
        let tmp = TempDir::new().unwrap();
        write_dream_queue(tmp.path(), &sample_queue(now())).unwrap();
        let later = now() + chrono::Duration::minutes(30);
        assert!(cached_queue(tmp.path(), later).is_some());
    }

    #[test]
    fn a_queue_older_than_an_hour_is_recomputed() {
        let tmp = TempDir::new().unwrap();
        write_dream_queue(tmp.path(), &sample_queue(now())).unwrap();
        let later = now() + chrono::Duration::minutes(61);
        assert!(cached_queue(tmp.path(), later).is_none());
    }

    #[test]
    fn without_a_queue_file_there_is_no_cached_queue() {
        let tmp = TempDir::new().unwrap();
        assert!(cached_queue(tmp.path(), now()).is_none());
    }

    #[test]
    fn refresh_dream_queue_writes_the_file() {
        let (tmp, db) = open_db();
        page(&db, "entities/old", NOW - 120 * DAY);
        refresh_dream_queue(tmp.path(), &db, now()).unwrap();
        assert_eq!(
            read_queue_file(&dream_queue_path(tmp.path())).map(|q| q.items.len()),
            Some(1)
        );
    }

    // ---- dream log -------------------------------------------------------------

    fn when() -> chrono::DateTime<chrono::Local> {
        use chrono::TimeZone as _;
        chrono::Local
            .with_ymd_and_hms(2026, 10, 6, 22, 15, 0)
            .unwrap()
    }

    #[test]
    fn a_dream_log_entry_is_appended_as_one_dated_line() {
        let tmp = TempDir::new().unwrap();
        append_dream_log(tmp.path(), "merged a\ninto b", when()).unwrap();
        let text = std::fs::read_to_string(dream_log_path(tmp.path())).unwrap();
        assert_eq!(
            text.lines().last(),
            Some("- 2026-10-06 22:15 merged a into b")
        );
    }

    #[test]
    fn an_empty_dream_log_entry_is_refused() {
        let tmp = TempDir::new().unwrap();
        assert!(append_dream_log(tmp.path(), "  \n ", when()).is_err());
    }

    // ---- not watcher-relevant ------------------------------------------------

    #[test]
    fn writing_the_dream_queue_does_not_wake_the_auto_committer() {
        let tmp = TempDir::new().unwrap();
        let meta = meta_dir(tmp.path());
        assert!(!super::super::watcher::is_relevant_event_path(
            &meta,
            &dream_queue_path(tmp.path())
        ));
    }

    #[test]
    fn writing_the_dream_log_does_not_wake_the_auto_committer() {
        let tmp = TempDir::new().unwrap();
        let meta = meta_dir(tmp.path());
        assert!(!super::super::watcher::is_relevant_event_path(
            &meta,
            &dream_log_path(tmp.path())
        ));
    }
}
