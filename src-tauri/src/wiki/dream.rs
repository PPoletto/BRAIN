//! H1 — the dream queue ("Tiefschlaf").
//!
//! BRAIN has no LLM (C-04/C-08), so consolidating the wiki is split:
//! BRAIN does the mechanical part — it collects what needs attention into
//! a prioritised work list, `00_meta/dream-queue.md` — and an agent, when
//! the user triggers a dream session ("träum mal"), works through it over
//! MCP (`brain_dream` action `queue`, then merge / rename / patch / write a
//! summary) and notes what it did in `00_meta/dream-log.md`
//! (`brain_dream` action `log`).
//!
//! Item kinds, highest priority first:
//!
//! | priority | kind | suggested action |
//! |---|---|---|
//! | 1 | `duplicate-candidate` (hygiene pair: same title, or content alike in a model index) | `merge` (`check-or-distinct` for a same title with similarity < 0.7) |
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

/// `brain_dream` (action `queue`) recomputes a queue older than this.
pub const MAX_QUEUE_AGE: chrono::Duration = chrono::Duration::hours(1);

/// Longest `brain_dream` log entry kept.
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
    /// How often earlier dream sessions logged this very item (same kind,
    /// same pages) as `skipped` or `deferred` (see [`skip_counts`]).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped_before: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// From this many earlier skips on, an item says so in its reason and a
/// priority-3 item is ranked behind the other priority-3 items — never
/// dropped: the repetition is the signal.
pub const REPEATED_SKIP_THRESHOLD: u32 = 3;

/// Only the newest this-many bytes of the dream log are read for
/// [`skip_counts`] (the log only grows).
const LOG_TAIL_BYTES: u64 = 500 * 1024;

/// Longest note kept per logged item.
const MAX_ITEM_NOTE_CHARS: usize = 300;

/// What a dream session did with one queue item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogOutcome {
    Done,
    Skipped,
    Deferred,
}

impl LogOutcome {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "done" => Some(Self::Done),
            "skipped" => Some(Self::Skipped),
            "deferred" => Some(Self::Deferred),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Skipped => "skipped",
            Self::Deferred => "deferred",
        }
    }
}

/// One queue item as a dream session logs it. `kind` is a single token
/// (the queue item's kind) and `pages` are valid page ids — the caller
/// validates both; the log format relies on neither containing spaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogItem {
    pub kind: String,
    pub pages: Vec<String>,
    pub outcome: LogOutcome,
    pub note: Option<String>,
}

/// Key of a queue item in the skip history: kind + sorted pages.
pub type SkipKey = (String, Vec<String>);

fn skip_key(kind: &str, pages: &[String]) -> SkipKey {
    let mut pages = pages.to_vec();
    pages.sort();
    (kind.to_string(), pages)
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

/// Why a duplicate pair is in the queue: "same title" and/or "pages read
/// almost the same", with the similarity when there is one.
fn duplicate_reason(pair: &super::hygiene::DuplicatePair) -> String {
    if pair.same_title_low_similarity() {
        return format!(
            "same title, low similarity ({:.2})",
            pair.similarity.unwrap_or_default()
        );
    }
    match (pair.same_title, pair.reads_alike(), pair.similarity) {
        (true, true, Some(s)) => {
            format!("same title, pages read almost the same (similarity {s:.2})")
        }
        (true, _, Some(s)) => format!("same title (similarity {s:.2})"),
        (true, _, None) => "same title".to_string(),
        (false, _, Some(s)) => format!("pages read almost the same (similarity {s:.2})"),
        (false, _, None) => "possible duplicates".to_string(),
    }
}

/// Build the queue from `rows` as of `now`, without skip history. Pure.
pub fn build_queue(rows: &DreamRows, now: chrono::DateTime<chrono::Utc>) -> DreamQueue {
    build_queue_with_history(rows, now, &HashMap::new())
}

/// [`build_queue`] with the skip history of earlier sessions (see
/// [`skip_counts`]): each item carries `skipped_before`; from
/// [`REPEATED_SKIP_THRESHOLD`] on, its reason says so and a priority-3
/// item moves behind the other priority-3 items. Pure.
pub fn build_queue_with_history(
    rows: &DreamRows,
    now: chrono::DateTime<chrono::Utc>,
    skips: &HashMap<SkipKey, u32>,
) -> DreamQueue {
    let now_unix = now.timestamp();
    let mut candidates: Vec<DreamItem> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // Priority 1: duplicates first (the costliest kind of drift), then
    // broken links (one item per source page) and broken sources.
    for pair in rows.hygiene.duplicate_pairs() {
        let reason = duplicate_reason(&pair);
        // Same name, different content: probably two things — check and
        // mark them distinct rather than merging.
        let action = if pair.same_title_low_similarity() {
            "check-or-distinct"
        } else {
            "merge"
        };
        candidates.push(item(
            1,
            "duplicate-candidate",
            vec![pair.a, pair.b],
            reason,
            action,
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
        if !page.keep && never_read && page.inbound == 0 && page.mtime > 0 && page.mtime < cutoff {
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

    // Skip history: how often each item was skipped or deferred before.
    for candidate in &mut candidates {
        let n = skips
            .get(&skip_key(&candidate.kind, &candidate.pages))
            .copied()
            .unwrap_or(0);
        candidate.skipped_before = n;
        if n >= REPEATED_SKIP_THRESHOLD {
            // `keep: true` only answers "nobody links / reads this page".
            let advice = if matches!(candidate.kind.as_str(), "orphan" | "decay-candidate") {
                "decide or mark keep"
            } else {
                "decide it"
            };
            candidate.reason = format!("{} (skipped {n}× before — {advice})", candidate.reason);
        }
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
    // A repeatedly skipped priority-3 item goes behind the other
    // priority-3 items. Only after the per-page dedupe: moved earlier, a
    // page's other priority-3 kind (orphan behind decay-candidate) would
    // win the dedupe and the skipped item would resurface under another
    // name.
    items.sort_by_key(|i| {
        (
            i.priority,
            i.priority == 3 && i.skipped_before >= REPEATED_SKIP_THRESHOLD,
        )
    });
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
    let pair_action = |s: &str| s == "merge" || s == "check-or-distinct";
    (pair_action(a) && b == "fix-link") || (a == "fix-link" && pair_action(b))
}

fn item(priority: u8, kind: &str, pages: Vec<String>, reason: String, action: &str) -> DreamItem {
    DreamItem {
        priority,
        kind: kind.into(),
        pages,
        reason,
        suggested_action: action.into(),
        skipped_before: 0,
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

/// Build the queue and write `00_meta/dream-queue.md`.
pub fn refresh_dream_queue(
    vault: &Path,
    db: &DbHandle,
    now: chrono::DateTime<chrono::Utc>,
) -> DbResult<DreamQueue> {
    let rows = db.with(load_dream_rows)?;
    let queue = build_queue_with_history(&rows, now, &skip_counts(vault));
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
         what you did with `brain_dream` (action `log`). Call `brain_dream` (action `queue`) for the live list.\n",
    );
    if queue.items.is_empty() {
        out.push_str("\nNothing to do — the wiki is in order.\n");
    } else {
        out.push_str("\n| # | Priority | Kind | Pages | Reason | Suggested action |\n");
        out.push_str("|---|---|---|---|---|---|\n");
        for (i, it) in queue.items.iter().enumerate() {
            let pages: Vec<String> = it.pages.iter().map(|p| format!("`{p}`")).collect();
            // From the threshold on, the reason itself says it.
            let reason = if (1..REPEATED_SKIP_THRESHOLD).contains(&it.skipped_before) {
                format!("{} (skipped {}× before)", it.reason, it.skipped_before)
            } else {
                it.reason.clone()
            };
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} |",
                i + 1,
                it.priority,
                it.kind,
                pages.join(", "),
                reason.replace('|', "\\|"),
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

/// Append one dated block to `00_meta/dream-log.md` (created with a
/// header on first use): the session line, then one indented bullet per
/// logged item, ``- <outcome> <kind> `<page>`, `<page>` — <note>`` (ids in
/// backticks, so ids with spaces or commas stay unambiguous; the caller
/// refuses ids containing a backtick). The entry is
/// collapsed to one line and capped at 2000 characters (an empty entry
/// is refused); notes are collapsed and capped at 300. Returns the
/// session line.
pub fn append_dream_log(
    vault: &Path,
    entry: &str,
    items: &[LogItem],
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
            b"# Dream log\n\nOne block per dream session (brain_dream action log): the session line, \
              then one bullet per queue item looked at. Local file - not synced.\n\n",
        )?;
    }
    let written = format!("- {} {line}", when.format("%Y-%m-%d %H:%M"));
    let mut block = format!("{written}\n");
    for item in items {
        let pages: Vec<String> = item.pages.iter().map(|p| format!("`{p}`")).collect();
        let _ = write!(
            block,
            "  - {} {} {}",
            item.outcome.as_str(),
            item.kind,
            pages.join(", ")
        );
        let note = item
            .note
            .as_deref()
            .map(|n| n.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|n| !n.is_empty());
        if let Some(mut note) = note {
            if note.chars().count() > MAX_ITEM_NOTE_CHARS {
                note = note.chars().take(MAX_ITEM_NOTE_CHARS - 1).collect();
                note.push('…');
            }
            let _ = write!(block, " — {note}");
        }
        block.push('\n');
    }
    file.write_all(block.as_bytes())?;
    Ok(written)
}

/// How often each queue item (kind + sorted pages) was logged `skipped`
/// or `deferred` in `00_meta/dream-log.md`. Reads at most the newest
/// [`LOG_TAIL_BYTES`]; a missing or unreadable log counts nothing, and
/// lines that are not item bullets of the expected shape are ignored.
pub fn skip_counts(vault: &Path) -> HashMap<SkipKey, u32> {
    read_log_tail(vault).map_or_else(HashMap::new, |text| count_skips(&text))
}

/// The newest [`LOG_TAIL_BYTES`] of the dream log, starting at a whole
/// line; `None` when the log is missing or unreadable.
fn read_log_tail(vault: &Path) -> Option<String> {
    use std::io::{Read as _, Seek as _};
    let mut file = std::fs::File::open(dream_log_path(vault)).ok()?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(LOG_TAIL_BYTES);
    file.seek(std::io::SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // A cut into the middle of the file starts with a partial line.
    if start > 0 {
        Some(
            text.split_once('\n')
                .map_or("", |(_, rest)| rest)
                .to_string(),
        )
    } else {
        Some(text)
    }
}

/// Most entries in [`DreamStats::most_skipped`].
pub const MOST_SKIPPED_MAX: usize = 10;

/// What the dream log says about past dream sessions (`brain_dream`
/// action `stats`, Integrity page).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DreamStats {
    /// Session lines (`- YYYY-MM-DD HH:MM …`).
    pub sessions: usize,
    /// Item bullets of any outcome.
    pub items_total: usize,
    /// Outcomes per item kind, most items first.
    pub per_kind: Vec<KindStats>,
    /// Items skipped or deferred since they were last logged done, most
    /// often first (at most [`MOST_SKIPPED_MAX`]).
    pub most_skipped: Vec<SkippedItem>,
}

/// Outcome counts of one item kind in the dream log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct KindStats {
    pub kind: String,
    pub done: usize,
    pub skipped: usize,
    pub deferred: usize,
}

/// One queue item (kind + pages) with its skip count since the last
/// `done` (see [`count_skips`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedItem {
    pub kind: String,
    pub pages: Vec<String>,
    pub count: u32,
}

/// [`stats_from_log`] of the vault's dream log (its newest
/// [`LOG_TAIL_BYTES`]); all zero without a log.
pub fn dream_stats(vault: &Path) -> DreamStats {
    read_log_tail(vault).map_or_else(DreamStats::default, |text| stats_from_log(&text))
}

/// Count sessions, item outcomes per kind and the most skipped items of
/// a dream-log text. Pure.
pub fn stats_from_log(text: &str) -> DreamStats {
    let sessions = text.lines().filter(|l| is_session_line(l)).count();
    let mut per_kind: Vec<KindStats> = Vec::new();
    let mut items_total = 0;
    for (outcome, (kind, _pages)) in text.lines().filter_map(parse_item_line) {
        items_total += 1;
        let pos = match per_kind.iter().position(|k| k.kind == kind) {
            Some(pos) => pos,
            None => {
                per_kind.push(KindStats {
                    kind,
                    ..KindStats::default()
                });
                per_kind.len() - 1
            }
        };
        let entry = &mut per_kind[pos];
        match outcome {
            LogOutcome::Done => entry.done += 1,
            LogOutcome::Skipped => entry.skipped += 1,
            LogOutcome::Deferred => entry.deferred += 1,
        }
    }
    per_kind.sort_by(|a, b| {
        let total = |k: &KindStats| k.done + k.skipped + k.deferred;
        total(b).cmp(&total(a)).then_with(|| a.kind.cmp(&b.kind))
    });
    let mut most_skipped: Vec<SkippedItem> = count_skips(text)
        .into_iter()
        .map(|((kind, pages), count)| SkippedItem { kind, pages, count })
        .collect();
    most_skipped.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.pages.cmp(&b.pages))
    });
    most_skipped.truncate(MOST_SKIPPED_MAX);
    DreamStats {
        sessions,
        items_total,
        per_kind,
        most_skipped,
    }
}

/// A session line of the log: `- YYYY-MM-DD HH:MM <entry>` at the start
/// of the line.
fn is_session_line(line: &str) -> bool {
    line.strip_prefix("- ")
        .and_then(|rest| rest.get(..16))
        .is_some_and(|stamp| chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M").is_ok())
}

/// The counting behind [`skip_counts`], on the log text, oldest line
/// first: each `skipped` / `deferred` bullet adds one for its key, a
/// `done` bullet resets it — an item that was fixed and comes back
/// starts fresh. Pure.
pub fn count_skips(text: &str) -> HashMap<SkipKey, u32> {
    let mut counts: HashMap<SkipKey, u32> = HashMap::new();
    for line in text.lines() {
        let Some((outcome, key)) = parse_item_line(line) else {
            continue;
        };
        match outcome {
            LogOutcome::Done => {
                counts.remove(&key);
            }
            LogOutcome::Skipped | LogOutcome::Deferred => *counts.entry(key).or_default() += 1,
        }
    }
    counts
}

/// An item bullet of the log: ``  - <outcome> <kind> `<id>`, `<id>`[ — note]``.
/// `None` for session lines, the header and anything malformed —
/// including bullets whose ids are not in backticks.
fn parse_item_line(line: &str) -> Option<(LogOutcome, SkipKey)> {
    if !line.starts_with(' ') {
        return None; // session lines and the header
    }
    let rest = line.trim_start().strip_prefix("- ")?;
    let (outcome, rest) = rest.split_once(' ')?;
    let outcome = LogOutcome::parse(outcome)?;
    let (kind, mut rest) = rest.split_once(' ')?;
    if kind.is_empty() {
        return None;
    }
    let mut pages: Vec<String> = Vec::new();
    loop {
        let inner = rest.strip_prefix('`')?;
        let end = inner.find('`')?;
        let id = &inner[..end];
        if id.trim().is_empty() || !id.contains('/') {
            return None;
        }
        pages.push(id.to_string());
        rest = &inner[end + 1..];
        match rest.strip_prefix(", ") {
            Some(next) => rest = next,
            None => break,
        }
    }
    // After the ids: nothing, or the note.
    if !(rest.is_empty() || rest.starts_with(" — ")) {
        return None;
    }
    Some((outcome, skip_key(kind, &pages)))
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
        build_queue(&db.with(load_dream_rows).unwrap(), now())
    }

    fn kinds_of(q: &DreamQueue, id: &str) -> Vec<String> {
        q.items
            .iter()
            .filter(|i| i.pages.iter().any(|p| p == id))
            .map(|i| i.kind.clone())
            .collect()
    }

    // ---- stats -------------------------------------------------------------

    /// Three sessions; orphan `entities/x` skipped twice, then done, then
    /// skipped again; decay `entities/y` deferred twice; one merge done.
    const STATS_LOG: &str = "# Dream log

One block per dream session (brain_dream action log): the session line, then one bullet per queue item looked at. Local file - not synced.

- 2026-10-05 21:10 first session
  - skipped orphan `entities/x` — unsure
  - deferred decay-candidate `entities/y`
  - done duplicate-candidate `entities/a`, `entities/b` — merged b into a
- 2026-10-06 21:15 second session
  - skipped orphan `entities/x` — still unsure
  - deferred decay-candidate `entities/y`
- 2026-10-07 09:00 third session
  - done orphan `entities/x` — linked from the hub
  - skipped orphan `entities/x` — came back
  - not an item bullet
";

    #[test]
    fn stats_count_every_session_line() {
        assert_eq!(stats_from_log(STATS_LOG).sessions, 3);
    }

    #[test]
    fn stats_count_every_item_bullet() {
        assert_eq!(stats_from_log(STATS_LOG).items_total, 7);
    }

    #[test]
    fn stats_count_the_outcomes_per_kind_most_items_first() {
        let per_kind: Vec<(String, usize, usize, usize)> = stats_from_log(STATS_LOG)
            .per_kind
            .into_iter()
            .map(|k| (k.kind, k.done, k.skipped, k.deferred))
            .collect();
        assert_eq!(
            per_kind,
            vec![
                ("orphan".to_string(), 1, 3, 0),
                ("decay-candidate".to_string(), 0, 0, 2),
                ("duplicate-candidate".to_string(), 1, 0, 0),
            ]
        );
    }

    #[test]
    fn the_most_skipped_items_count_skips_since_the_last_done() {
        let most: Vec<(String, Vec<String>, u32)> = stats_from_log(STATS_LOG)
            .most_skipped
            .into_iter()
            .map(|s| (s.kind, s.pages, s.count))
            .collect();
        assert_eq!(
            most,
            vec![
                (
                    "decay-candidate".to_string(),
                    vec!["entities/y".to_string()],
                    2
                ),
                ("orphan".to_string(), vec!["entities/x".to_string()], 1),
            ]
        );
    }

    #[test]
    fn the_most_skipped_list_keeps_at_most_ten_items() {
        let mut log = String::from("- 2026-10-07 09:00 s\n");
        for i in 0..12 {
            log.push_str(&format!("  - skipped orphan `entities/p{i:02}` — later\n"));
        }
        assert_eq!(stats_from_log(&log).most_skipped.len(), MOST_SKIPPED_MAX);
    }

    #[test]
    fn a_vault_without_a_dream_log_has_empty_stats() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(dream_stats(tmp.path()), DreamStats::default());
    }

    #[test]
    fn dream_stats_read_the_vaults_dream_log() {
        let tmp = TempDir::new().unwrap();
        let path = dream_log_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, STATS_LOG).unwrap();
        assert_eq!(dream_stats(tmp.path()).sessions, 3);
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
    fn a_duplicate_pair_declared_distinct_gives_no_merge_item() {
        let (_tmp, db) = open_db();
        duplicate_pair(&db);
        exec(
            &db,
            "UPDATE pages SET frontmatter = ?1 WHERE id = 'entities/b'",
            &[&r#"{"id":"entities/b","type":"entity","distinct_from":["entities/a"]}"#],
        );
        let merges = queue(&db)
            .items
            .iter()
            .filter(|i| i.suggested_action == "merge")
            .count();
        assert_eq!(merges, 0);
    }

    #[test]
    fn two_pages_with_the_same_title_give_a_merge_item_without_a_model_index() {
        let (_tmp, db) = open_db();
        for id in ["entities/cockpit", "entities/cio-cockpit"] {
            page(&db, id, NOW);
            exec(
                &db,
                "UPDATE pages SET title = 'CIO COCKPIT' WHERE id = ?1",
                &[&id],
            );
        }
        let reasons: Vec<String> = queue(&db)
            .items
            .iter()
            .filter(|i| i.suggested_action == "merge")
            .map(|i| i.reason.clone())
            .collect();
        assert_eq!(reasons, vec!["same title".to_string()]);
    }

    /// Two pages titled "Michael Meier" whose vectors have cosine 0.6.
    fn same_title_low_similarity_pair(db: &DbHandle) {
        exec(
            db,
            "INSERT INTO schema_meta(key, value) VALUES ('index_embedder', 'bge-m3')",
            &[],
        );
        for (id, v) in [("entities/a", [1.0f32, 0.0]), ("entities/b", [0.6, 0.8])] {
            page(db, id, NOW);
            exec(
                db,
                "UPDATE pages SET title = 'Michael Meier' WHERE id = ?1",
                &[&id],
            );
            exec(
                db,
                "INSERT INTO page_vectors(page_id, embedding) VALUES (?1, ?2)",
                &[&id, &vec_to_bytes(&v)],
            );
        }
    }

    #[test]
    fn a_same_title_pair_with_low_similarity_asks_to_check_or_mark_distinct() {
        let (_tmp, db) = open_db();
        same_title_low_similarity_pair(&db);
        let actions: Vec<String> = queue(&db)
            .items
            .iter()
            .filter(|i| i.kind == "duplicate-candidate")
            .map(|i| i.suggested_action.clone())
            .collect();
        assert_eq!(actions, vec!["check-or-distinct".to_string()]);
    }

    #[test]
    fn a_same_title_pair_with_low_similarity_names_the_similarity_in_its_reason() {
        let (_tmp, db) = open_db();
        same_title_low_similarity_pair(&db);
        let reasons: Vec<String> = queue(&db)
            .items
            .iter()
            .filter(|i| i.kind == "duplicate-candidate")
            .map(|i| i.reason.clone())
            .collect();
        assert_eq!(
            reasons,
            vec!["same title, low similarity (0.60)".to_string()]
        );
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
        append_dream_log(tmp.path(), "merged a\ninto b", &[], when()).unwrap();
        let text = std::fs::read_to_string(dream_log_path(tmp.path())).unwrap();
        assert_eq!(
            text.lines().last(),
            Some("- 2026-10-06 22:15 merged a into b")
        );
    }

    #[test]
    fn an_empty_dream_log_entry_is_refused() {
        let tmp = TempDir::new().unwrap();
        assert!(append_dream_log(tmp.path(), "  \n ", &[], when()).is_err());
    }

    fn log_item(outcome: LogOutcome, kind: &str, pages: &[&str], note: Option<&str>) -> LogItem {
        LogItem {
            kind: kind.into(),
            pages: pages.iter().map(|p| p.to_string()).collect(),
            outcome,
            note: note.map(str::to_string),
        }
    }

    #[test]
    fn a_dream_log_with_items_writes_one_bullet_per_item_under_the_session_line() {
        let tmp = TempDir::new().unwrap();
        let items = [
            log_item(
                LogOutcome::Done,
                "duplicate-candidate",
                &["entities/a", "entities/b"],
                Some("merged b into a"),
            ),
            log_item(
                LogOutcome::Skipped,
                "orphan",
                &["entities/x"],
                Some("still  a\nuseful reference"),
            ),
            log_item(LogOutcome::Deferred, "summary-stale", &["entities/y"], None),
        ];
        append_dream_log(tmp.path(), "tidied up", &items, when()).unwrap();
        let text = std::fs::read_to_string(dream_log_path(tmp.path())).unwrap();
        let block: Vec<&str> = text
            .lines()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert_eq!(
            block,
            vec![
                "- 2026-10-06 22:15 tidied up",
                "  - done duplicate-candidate `entities/a`, `entities/b` — merged b into a",
                "  - skipped orphan `entities/x` — still a useful reference",
                "  - deferred summary-stale `entities/y`",
            ]
        );
    }

    #[test]
    fn skips_and_deferrals_after_the_latest_done_are_counted_per_kind_and_page_set() {
        let tmp = TempDir::new().unwrap();
        let skip = [log_item(
            LogOutcome::Skipped,
            "duplicate-candidate",
            &["entities/b", "entities/a"],
            Some("no"),
        )];
        let defer = [log_item(
            LogOutcome::Deferred,
            "duplicate-candidate",
            &["entities/a", "entities/b"],
            None,
        )];
        let done = [log_item(
            LogOutcome::Done,
            "duplicate-candidate",
            &["entities/a", "entities/b"],
            None,
        )];
        // skipped, done (resets), skipped, deferred → 2.
        append_dream_log(tmp.path(), "one", &skip, when()).unwrap();
        append_dream_log(tmp.path(), "two", &done, when()).unwrap();
        append_dream_log(tmp.path(), "three", &skip, when()).unwrap();
        append_dream_log(tmp.path(), "four", &defer, when()).unwrap();
        let key = (
            "duplicate-candidate".to_string(),
            vec!["entities/a".to_string(), "entities/b".to_string()],
        );
        assert_eq!(skip_counts(tmp.path()).get(&key), Some(&2));
    }

    #[test]
    fn malformed_log_lines_are_ignored_when_counting_skips() {
        let text = "# Dream log\n\n- 2026-10-01 10:00 session\n  - skipped orphan `entities/x` — fine\n  \
                    - skipped\n  - skipped orphan\n  - maybe orphan `entities/x`\n  - skipped orphan `not an id`\n  \
                    - skipped orphan entities/x\n  - skipped orphan `entities/x\n  - skipped orphan `entities/x`junk\n\
                    garbage line ✓\n  - skipped orphan `entities/x`\n";
        let counts = count_skips(text);
        assert_eq!(
            counts.into_iter().collect::<Vec<_>>(),
            vec![(("orphan".to_string(), vec!["entities/x".to_string()]), 2)]
        );
    }

    #[test]
    fn only_the_tail_of_a_huge_log_is_read() {
        let tmp = TempDir::new().unwrap();
        let path = dream_log_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // An old skip, then more than LOG_TAIL_BYTES of other lines.
        let mut text = String::from("  - skipped orphan `entities/old`\n");
        while (text.len() as u64) < LOG_TAIL_BYTES + 1024 {
            text.push_str(
                "- 2026-10-01 10:00 a session line that is long enough to fill the log\n",
            );
        }
        text.push_str("  - skipped orphan `entities/new`\n");
        std::fs::write(&path, text).unwrap();
        let counts = skip_counts(tmp.path());
        assert_eq!(
            (
                counts.contains_key(&("orphan".to_string(), vec!["entities/old".to_string()])),
                counts.len()
            ),
            (false, 1)
        );
    }

    #[test]
    fn an_id_with_a_space_round_trips_through_the_log_into_the_skip_count() {
        let tmp = TempDir::new().unwrap();
        let item = [log_item(
            LogOutcome::Skipped,
            "orphan",
            &["entities/Acme Inc"],
            Some("ask"),
        )];
        append_dream_log(tmp.path(), "one", &item, when()).unwrap();
        let key = ("orphan".to_string(), vec!["entities/Acme Inc".to_string()]);
        assert_eq!(skip_counts(tmp.path()).get(&key), Some(&1));
    }

    #[test]
    fn a_single_id_containing_a_comma_is_not_read_as_two_pages() {
        let tmp = TempDir::new().unwrap();
        let item = [log_item(
            LogOutcome::Skipped,
            "orphan",
            &["entities/a, entities/b"],
            None,
        )];
        append_dream_log(tmp.path(), "one", &item, when()).unwrap();
        let keys: Vec<SkipKey> = skip_counts(tmp.path()).into_keys().collect();
        assert_eq!(
            keys,
            vec![(
                "orphan".to_string(),
                vec!["entities/a, entities/b".to_string()]
            )]
        );
    }

    #[test]
    fn a_non_orphan_item_skipped_three_times_is_told_to_be_decided_not_kept() {
        let (_tmp, db) = open_db();
        page(&db, "entities/a", NOW);
        link(&db, "entities/a", "entities/missing");
        let log = "  - skipped broken-link `entities/a`\n".repeat(3);
        let item = queue_with(&db, &log).items.into_iter().next().unwrap();
        assert!(
            item.reason.ends_with("(skipped 3× before — decide it)"),
            "{}",
            item.reason
        );
    }

    /// Two old, unread, unlinked pages `a` and `b` (decay candidates).
    fn two_decaying_pages(db: &DbHandle) {
        page(db, "entities/a", NOW - 200 * DAY);
        page(db, "entities/b", NOW - 200 * DAY);
    }

    fn queue_with(db: &DbHandle, log: &str) -> DreamQueue {
        let rows = db.with(load_dream_rows).unwrap();
        build_queue_with_history(&rows, now(), &count_skips(log))
    }

    #[test]
    fn an_item_skipped_twice_before_carries_skipped_before_two() {
        let (_tmp, db) = open_db();
        two_decaying_pages(&db);
        let log = "  - skipped decay-candidate `entities/a` — later\n  - deferred decay-candidate `entities/a`\n";
        let a = queue_with(&db, log)
            .items
            .into_iter()
            .find(|i| i.pages == ["entities/a"])
            .unwrap();
        assert_eq!(a.skipped_before, 2);
    }

    #[test]
    fn an_item_skipped_three_times_goes_behind_the_other_priority_three_items() {
        let (_tmp, db) = open_db();
        two_decaying_pages(&db);
        let log = "  - skipped decay-candidate `entities/a`\n".repeat(3);
        let order: Vec<(String, Vec<String>)> = queue_with(&db, &log)
            .items
            .into_iter()
            .map(|i| (i.kind, i.pages))
            .collect();
        assert_eq!(
            order,
            vec![
                (
                    "decay-candidate".to_string(),
                    vec!["entities/b".to_string()]
                ),
                (
                    "decay-candidate".to_string(),
                    vec!["entities/a".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn an_item_skipped_three_times_says_so_in_its_reason() {
        let (_tmp, db) = open_db();
        two_decaying_pages(&db);
        let log = "  - skipped decay-candidate `entities/a`\n".repeat(3);
        let a = queue_with(&db, &log)
            .items
            .into_iter()
            .find(|i| i.pages == ["entities/a"])
            .unwrap();
        assert!(
            a.reason
                .ends_with("(skipped 3× before — decide or mark keep)"),
            "{}",
            a.reason
        );
    }

    #[test]
    fn skipped_before_is_left_out_of_the_json_when_zero() {
        let (_tmp, db) = open_db();
        two_decaying_pages(&db);
        let json = serde_json::to_string(&queue_with(&db, "")).unwrap();
        assert!(!json.contains("skipped_before"), "{json}");
    }

    #[test]
    fn the_markdown_queue_names_an_earlier_skip() {
        let (_tmp, db) = open_db();
        two_decaying_pages(&db);
        let md = render_queue(&queue_with(
            &db,
            "  - skipped decay-candidate `entities/a`\n",
        ));
        assert!(md.contains("(skipped 1× before)"), "{md}");
    }

    fn mark_keep(db: &DbHandle, id: &str) {
        exec(
            db,
            "UPDATE pages SET frontmatter = ?1 WHERE id = ?2",
            &[
                &format!(r#"{{"id":"{id}","type":"entity","keep":true}}"#),
                &id,
            ],
        );
    }

    #[test]
    fn a_page_marked_keep_gets_no_decay_or_orphan_item() {
        let (_tmp, db) = open_db();
        page(&db, "entities/kept", NOW - 200 * DAY);
        mark_keep(&db, "entities/kept");
        assert!(queue(&db).items.is_empty(), "{:?}", queue(&db).items);
    }

    #[test]
    fn a_page_marked_keep_still_gets_its_broken_link_item() {
        let (_tmp, db) = open_db();
        page(&db, "entities/kept", NOW - 200 * DAY);
        mark_keep(&db, "entities/kept");
        link(&db, "entities/kept", "entities/missing");
        assert_eq!(kinds_of(&queue(&db), "entities/kept"), vec!["broken-link"]);
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
