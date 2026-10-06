//! Filesystem → SQLite synchronisation for the wiki pages.
//!
//! Walks `02_wiki/<type>/*.md`, parses frontmatter + body, and upserts into
//! `pages`, `pages_fts`, `wiki_links`, and `page_tags`. Pages whose source
//! file disappeared are deleted. The pipeline is intended to run after each
//! auto-commit; the cost is roughly O(N) where N is the changed file count.
//!
//! ## Batches
//!
//! A rebuild commits in batches of [`REBUILD_BATCH_SIZE`] pages and never
//! holds the connection lock while it embeds: per batch it reads the files
//! (no lock), looks up which pages changed (short lock), embeds those
//! (no lock — the slow bge-m3 part), then writes the batch in one
//! transaction and releases the lock. Searches queued behind a re-index
//! therefore wait for at most one batch write, not for minutes.
//!
//! ## Resume invariant
//!
//! **Every `pages` row whose `file_hash` equals the hash of its file was
//! written by the current indexer format.** The `file_hash` fast path
//! (skip unchanged pages) is only sound because of this invariant, and it
//! is what makes an interrupted rebuild resumable:
//!
//!  - When the stored `index_format_version` is outdated, the first step
//!    clears `file_hash` on every row and records
//!    `index_format_pending = INDEX_FORMAT_VERSION` — in ONE transaction.
//!    From then on no stale row can take the fast path.
//!  - Each batch then writes fresh rows with their real `file_hash`.
//!  - Only after the LAST batch (and the prune) succeeds is
//!    `index_format_version` written and `index_format_pending` removed,
//!    again in one transaction.
//!
//! An interrupted run (crash, unplug, embedder panic) therefore leaves the
//! version unchanged and the pending marker set. The next run sees the
//! marker, does NOT clear the hashes again, and the fast path skips exactly
//! the pages earlier batches already rebuilt — it resumes instead of
//! re-embedding the whole vault. A finished run never repeats. A forced
//! re-index ([`invalidate_all_pages`]) uses the same marker, so it resumes
//! the same way.
//!
//! ## Which embedder wrote the vectors
//!
//! `schema_meta.index_embedder` names the embedder whose vectors the index
//! holds (`"bge-m3"`, the hashed fallback's name, or `"mixed"`). It is
//! written in the final transaction: a run that re-embedded every page (a
//! pending marker was set, or every page was rewritten) records its own
//! embedder; a partial run with a DIFFERENT embedder records `"mixed"`.
//! The duplicate-candidate lint only trusts similarities of a `"bge-m3"`
//! index; after the model download the GUI forces a re-index so the
//! vectors actually become bge-m3.
//!
//! ## Concurrency
//!
//! [`DbHandle::rebuild_guard`] serialises rebuilds within ONE process. The
//! GUI and an MCP server process each have their own handle on the same
//! SQLite file, so two rebuilds can run at once across processes. Every
//! write transaction here is opened `IMMEDIATE` (it takes the write lock
//! up front and waits on `busy_timeout` instead of failing with
//! `SQLITE_BUSY_SNAPSHOT` on a read-then-write upgrade), each batch only
//! rewrites whole pages, and the pending marker is shared through the
//! file — so concurrent runs at worst embed some pages twice and converge
//! on the same final state.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::{params, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::embedding::{chunk as chunker, vec_to_bytes, Embedder};
use crate::vault::layout::{wiki_dir, WIKI_SUBDIRS};
use crate::wiki::page::{parse, ParsedPage};

use super::{DbHandle, DbResult};

/// Format version of the page-index. Bump this whenever the indexer
/// changes how it parses page bodies, what it stores per page, or what
/// it puts into `wiki_links` / `chunks`. The next mount detects the
/// mismatch and forces a full re-index even for pages whose
/// `file_hash` hasn't changed — otherwise the file_hash skip-fast-path
/// would silently keep stale rows around.
///
/// History:
///  - v1: initial release ([[wiki-link]] only)
///  - v2: also recognise standard markdown `[text](id)` as wiki-link
///  - v3: contextual chunking — chunks are cut per Markdown section and
///    embedded with a `<title> (<type>) › <h1> › <h2>` context header
///    (stored chunk text unchanged in kind: the original section words).
///    Also: `wiki_links.dst_id` holds the page part of `[[id#heading]]`.
///    Vectors written by v2 carry no page/section context, so every page
///    must be re-embedded exactly once after the update; the version
///    bump below is what forces that. See the module docs for how an
///    interrupted upgrade resumes and why a finished one never repeats.
const INDEX_FORMAT_VERSION: i64 = 3;

/// `schema_meta` key holding the format version of a COMPLETED rebuild.
const VERSION_KEY: &str = "index_format_version";

/// `schema_meta` key present while a format upgrade or forced re-index is
/// in progress: the version whose rows earlier batches already wrote (see
/// module docs).
const PENDING_KEY: &str = "index_format_pending";

/// `schema_meta` key naming the embedder whose vectors the index holds
/// (see module docs).
const EMBEDDER_KEY: &str = "index_embedder";

/// [`EMBEDDER_KEY`] value of an index whose vectors come from more than
/// one embedder.
const MIXED_EMBEDDERS: &str = "mixed";

/// The embedder whose vectors the index holds (`schema_meta.index_embedder`),
/// or `None` for an index written before this was recorded.
pub fn index_embedder(conn: &rusqlite::Connection) -> Option<String> {
    read_meta(conn, EMBEDDER_KEY)
}

/// Pages per committed batch. Small enough that a batch write (and the
/// lock it holds) is short, large enough that a no-op rebuild of a big
/// vault stays a handful of transactions.
pub const REBUILD_BATCH_SIZE: usize = 50;

/// Rebuild with the process-cached embedder for `vault`, so a re-index
/// after every watcher commit does not reload the bge-m3 weights.
pub fn rebuild(db: &DbHandle, vault: &Path) -> DbResult<()> {
    rebuild_with_progress(db, vault, None)
}

/// [`rebuild`] that reports `(pages_done, pages_total)` after every
/// committed batch. GUI callers use it to show "Rebuilding the index
/// (120/843)" in the tray; the MCP subprocess and the watcher call plain
/// [`rebuild`]. The callback runs with the connection lock released.
pub fn rebuild_with_progress(
    db: &DbHandle,
    vault: &Path,
    progress: Option<&dyn Fn(usize, usize)>,
) -> DbResult<()> {
    // Serialise with other rebuilds on this handle BEFORE fetching the
    // embedder: the first call may wait several seconds for the model
    // load, and the audit (which waits for running rebuilds) must
    // already see that one is under way.
    let _serial = db.rebuild_guard();
    let embedder = crate::embedding::cached_for_vault(vault);
    rebuild_batched(db, vault, embedder.as_ref(), REBUILD_BATCH_SIZE, progress)
}

pub fn rebuild_with<E: Embedder + ?Sized>(
    db: &DbHandle,
    vault: &Path,
    embedder: &E,
) -> DbResult<()> {
    rebuild_with_batches(db, vault, embedder, REBUILD_BATCH_SIZE, None)
}

/// [`rebuild_with`] with an explicit batch size and progress callback —
/// the seam the batch tests use.
pub fn rebuild_with_batches<E: Embedder + ?Sized>(
    db: &DbHandle,
    vault: &Path,
    embedder: &E,
    batch_size: usize,
    progress: Option<&dyn Fn(usize, usize)>,
) -> DbResult<()> {
    let _serial = db.rebuild_guard();
    rebuild_batched(db, vault, embedder, batch_size, progress)
}

/// Mark every indexed page stale so the next rebuild re-indexes and
/// re-embeds all of them (Settings → "Rebuild index", model download).
/// Exactly what a format upgrade does — clear the hashes and set the
/// pending marker — so a forced run that is interrupted resumes instead
/// of starting over, and its final transaction records the embedder.
pub fn invalidate_all_pages(conn: &rusqlite::Connection) -> DbResult<()> {
    write_meta(conn, PENDING_KEY, &INDEX_FORMAT_VERSION.to_string())?;
    conn.execute("UPDATE pages SET file_hash = NULL", [])?;
    Ok(())
}

/// A write transaction that takes SQLite's write lock up front (see the
/// module docs on concurrency).
fn immediate(conn: &rusqlite::Connection) -> rusqlite::Result<Transaction<'_>> {
    Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
}

/// One page file read and parsed, ready to be checked and written.
struct PreparedPage {
    path: PathBuf,
    parsed: ParsedPage,
    mtime: i64,
    file_hash: String,
}

/// What a batch writes for one page.
enum PageWrite {
    /// Unchanged since the last index: only refresh path + mtime.
    Touch,
    /// Changed (or format-stale): full rewrite with these chunks
    /// (`text`, embedding bytes).
    Full(Vec<(String, Vec<u8>)>),
}

fn rebuild_batched<E: Embedder + ?Sized>(
    db: &DbHandle,
    vault: &Path,
    embedder: &E,
    batch_size: usize,
    progress: Option<&dyn Fn(usize, usize)>,
) -> DbResult<()> {
    let files = collect_page_files(&wiki_dir(vault))?;
    let total = files.len();
    db.with(begin_format_upgrade)?;

    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut done = 0usize;
    let mut rewritten = 0usize;
    for batch in files.chunks(batch_size.max(1)) {
        // 1. Read + parse + hash, no lock held.
        let mut prepared: Vec<PreparedPage> = Vec::with_capacity(batch.len());
        for path in batch {
            if let Some(page) = prepare_page(path)? {
                seen_ids.insert(page.parsed.frontmatter.id.clone());
                prepared.push(page);
            }
        }

        // 2. Which pages can take the fast path? Short lock.
        let unchanged: Vec<bool> = db.with(|conn| {
            prepared.iter().map(|p| is_unchanged(conn, p)).collect()
        })?;

        // 3. Embed the changed pages, no lock held.
        let writes: Vec<PageWrite> = prepared
            .iter()
            .zip(&unchanged)
            .map(|(page, &same)| {
                if same {
                    PageWrite::Touch
                } else {
                    PageWrite::Full(embed_page(&page.parsed, embedder))
                }
            })
            .collect();
        let reindexed = writes
            .iter()
            .filter(|w| matches!(w, PageWrite::Full(_)))
            .count();
        rewritten += reindexed;

        // 4. Write the batch in one transaction, then release the lock.
        db.with(|conn| {
            let tx = immediate(conn)?;
            for (page, write) in prepared.iter().zip(&writes) {
                write_page(&tx, page, write)?;
            }
            tx.commit()?;
            Ok(())
        })?;

        done += batch.len();
        if reindexed > 0 {
            tracing::info!(done, total, reindexed, "pages index: batch committed");
        } else {
            tracing::debug!(done, total, "pages index: batch unchanged");
        }
        if let Some(report) = progress {
            report(done, total);
        }
    }

    // Prune + finalise the format version (and the embedder record)
    // together: only a run that got through every batch may declare the
    // index current.
    db.with(|conn| {
        let tx = immediate(conn)?;
        prune_missing(&tx, &seen_ids)?;
        let full_run = read_meta(&tx, PENDING_KEY).is_some() || rewritten == seen_ids.len();
        let name = embedder.name();
        if full_run {
            write_meta(&tx, EMBEDDER_KEY, name)?;
        } else if rewritten > 0 && read_meta(&tx, EMBEDDER_KEY).as_deref() != Some(name) {
            write_meta(&tx, EMBEDDER_KEY, MIXED_EMBEDDERS)?;
        }
        write_meta(&tx, VERSION_KEY, &INDEX_FORMAT_VERSION.to_string())?;
        tx.execute("DELETE FROM schema_meta WHERE key = ?1", params![PENDING_KEY])?;
        tx.commit()?;
        Ok(())
    })
}

/// Step one of the resume invariant (module docs): when the stored format
/// is outdated and no upgrade to the current format is already under
/// way, clear every `file_hash` and record the pending upgrade — in one
/// transaction. A no-op when the index is current or an interrupted
/// upgrade is being resumed.
fn begin_format_upgrade(conn: &rusqlite::Connection) -> DbResult<()> {
    // Read and (maybe) write in one IMMEDIATE transaction, so two
    // processes cannot both decide to start the upgrade.
    let tx = immediate(conn)?;
    let stored_version = read_meta_i64(&tx, VERSION_KEY).unwrap_or(0);
    if stored_version == INDEX_FORMAT_VERSION {
        return Ok(());
    }
    if read_meta_i64(&tx, PENDING_KEY) == Some(INDEX_FORMAT_VERSION) {
        tracing::info!(
            stored_version,
            new_version = INDEX_FORMAT_VERSION,
            "resuming an interrupted re-index — pages finished earlier are kept"
        );
        return Ok(());
    }
    let indexed_pages: i64 = tx
        .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
        .unwrap_or(0);
    if indexed_pages > 0 {
        tracing::info!(
            stored_version,
            new_version = INDEX_FORMAT_VERSION,
            pages = indexed_pages,
            "index format changed — re-indexing and re-embedding every page once"
        );
    }
    invalidate_all_pages(&tx)?;
    tx.commit()?;
    Ok(())
}

fn read_meta(conn: &rusqlite::Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM schema_meta WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

fn read_meta_i64(conn: &rusqlite::Connection, key: &str) -> Option<i64> {
    read_meta(conn, key).and_then(|s| s.parse::<i64>().ok())
}

fn write_meta(conn: &rusqlite::Connection, key: &str, value: &str) -> DbResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO schema_meta(key, value) VALUES (?1, ?2)",
        params![key, value],
    )?;
    Ok(())
}

/// Every `.md` file under the wiki type directories, in a stable order
/// (type directory order, then path) so batches are deterministic.
fn collect_page_files(wiki: &Path) -> DbResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    for sub in WIKI_SUBDIRS {
        let dir = wiki.join(sub);
        if !dir.exists() {
            continue;
        }
        let mut files = Vec::new();
        walk_md(&dir, &mut files)?;
        files.sort();
        out.extend(files);
    }
    Ok(out)
}

fn walk_md(dir: &Path, out: &mut Vec<PathBuf>) -> DbResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        if p.is_dir() {
            walk_md(&p, out)?;
        } else if p.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(p);
        }
    }
    Ok(())
}

/// Read, parse and hash one page file. `Ok(None)` for a file that does
/// not parse as a page, or that was deleted/renamed since the file list
/// was collected (batches are minutes apart during a re-embed): it is
/// skipped and pruned from the index like any deleted page.
fn prepare_page(path: &Path) -> DbResult<Option<PreparedPage>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let Ok(parsed) = parse(&raw) else {
        return Ok(None);
    };
    let mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    Ok(Some(PreparedPage {
        path: path.to_path_buf(),
        parsed,
        mtime,
        file_hash: hex::encode(hasher.finalize()),
    }))
}

/// Fast-path check: the file content is unchanged from the last indexed
/// state AND the chunk rows are still intact. The previous implementation
/// re-embedded every page on every bootstrap, which on a vault with
/// bge-m3 active meant a 30-60 s freeze per startup even when nothing
/// had been edited. A cleared (NULL) hash — format upgrade or forced
/// rebuild — never matches.
fn is_unchanged(conn: &rusqlite::Connection, page: &PreparedPage) -> DbResult<bool> {
    let prev: Option<(Option<String>, i64)> = conn
        .query_row(
            "SELECT p.file_hash, COUNT(c.id) \
             FROM pages p LEFT JOIN chunks c ON c.page_id = p.id \
             WHERE p.id = ?1 \
             GROUP BY p.id",
            params![&page.parsed.frontmatter.id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
        )
        .ok();
    Ok(matches!(
        prev,
        Some((Some(prev_hash), chunk_count)) if prev_hash == page.file_hash && chunk_count > 0
    ))
}

/// Contextual chunking: the embedder sees "<title> (<type>) › <h1> ›
/// <h2>" in front of each chunk so the vector encodes where the text
/// belongs. Only the bare chunk text is stored — snippets, FTS and the
/// UI never see the header. Queries are embedded bare.
fn embed_page<E: Embedder + ?Sized>(parsed: &ParsedPage, embedder: &E) -> Vec<(String, Vec<u8>)> {
    let id = &parsed.frontmatter.id;
    let context_title = parsed.frontmatter.title.as_deref().unwrap_or(id);
    chunker::section_chunks(&parsed.body)
        .into_iter()
        .map(|chunk| {
            let embed_input = chunker::contextual_text(
                context_title,
                &parsed.frontmatter.page_type,
                &chunk.heading_path,
                &chunk.text,
            );
            let blob = vec_to_bytes(&embedder.embed(&embed_input));
            (chunk.text, blob)
        })
        .collect()
}

fn write_page(tx: &rusqlite::Transaction, page: &PreparedPage, write: &PageWrite) -> DbResult<()> {
    let id = &page.parsed.frontmatter.id;
    let path = page.path.to_string_lossy().to_string();
    let chunks = match write {
        PageWrite::Touch => {
            // Touch mtime/path in case the file was moved without
            // changing its bytes (rename + same content). Cheap
            // single-row UPDATE; no embeddings, no FTS rewrite.
            tx.execute(
                "UPDATE pages SET path=?1, file_mtime=?2 WHERE id=?3",
                params![&path, page.mtime, id],
            )?;
            return Ok(());
        }
        PageWrite::Full(chunks) => chunks,
    };
    let parsed = &page.parsed;
    let title = parsed.frontmatter.title.as_deref();
    let frontmatter_json = serde_json::to_string(&parsed.frontmatter).unwrap_or_default();

    tx.execute(
        "INSERT INTO pages(id, type, path, title, frontmatter, body, updated_at, file_mtime, file_hash) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT(id) DO UPDATE SET \
            type=excluded.type, path=excluded.path, title=excluded.title, \
            frontmatter=excluded.frontmatter, body=excluded.body, updated_at=excluded.updated_at, \
            file_mtime=excluded.file_mtime, file_hash=excluded.file_hash",
        params![
            id,
            &parsed.frontmatter.page_type,
            &path,
            title,
            &frontmatter_json,
            &parsed.body,
            parsed.frontmatter.updated.as_deref(),
            page.mtime,
            &page.file_hash,
        ],
    )?;

    // Refresh FTS row (delete + insert keeps things simple).
    tx.execute("DELETE FROM pages_fts WHERE id = ?1", params![id])?;
    tx.execute(
        "INSERT INTO pages_fts(id, title, body) VALUES (?1, ?2, ?3)",
        params![id, title.unwrap_or(""), &parsed.body],
    )?;

    // Wiki-links: replace. `[[id#heading]]` is stored as a link to `id`
    // (the page text keeps the anchor), so graph and backlinks agree with
    // the lint; a bare in-page `[[#heading]]` is no page link at all.
    tx.execute("DELETE FROM wiki_links WHERE src_id = ?1", params![id])?;
    for link in &parsed.wiki_links {
        let target = link.split('#').next().unwrap_or(link).trim();
        if target.is_empty() {
            continue;
        }
        tx.execute(
            "INSERT OR IGNORE INTO wiki_links(src_id, dst_id, broken) VALUES (?1, ?2, 0)",
            params![id, target],
        )?;
    }

    // Tags: replace.
    tx.execute("DELETE FROM page_tags WHERE page_id = ?1", params![id])?;
    for tag in &parsed.frontmatter.tags {
        tx.execute(
            "INSERT OR IGNORE INTO page_tags(page_id, tag) VALUES (?1, ?2)",
            params![id, tag],
        )?;
    }

    // Chunks + embeddings: replace. We mirror each embedding into the
    // sqlite-vec `chunk_vectors` virtual table so KNN sub-queries can
    // join on `chunks.id = chunk_vectors.rowid`. Mirror is best-effort
    // — if `chunk_vectors` isn't available (sqlite-vec not loaded for
    // some reason) the BLOB column in `chunks` remains the source of
    // truth and the search code falls back to brute-force cosine.
    let vec_table_present = super::migrations::chunk_vectors_available(tx);
    // Wipe all old chunks (and their vec rows by rowid) for this page.
    let old_chunk_ids: Vec<i64> = {
        let mut stmt = tx.prepare("SELECT id FROM chunks WHERE page_id = ?1")?;
        stmt.query_map(params![id], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    if vec_table_present {
        for cid in &old_chunk_ids {
            let _ = tx.execute("DELETE FROM chunk_vectors WHERE rowid = ?1", params![cid]);
        }
    }
    tx.execute("DELETE FROM chunks WHERE page_id = ?1", params![id])?;
    for (idx, (text, blob)) in chunks.iter().enumerate() {
        tx.execute(
            "INSERT INTO chunks(page_id, chunk_idx, text, embedding) VALUES (?1, ?2, ?3, ?4)",
            params![id, idx as i64, text, blob],
        )?;
        if vec_table_present {
            let chunk_id = tx.last_insert_rowid();
            let _ = tx.execute(
                "INSERT INTO chunk_vectors(rowid, embedding) VALUES (?1, ?2)",
                params![chunk_id, blob],
            );
        }
    }
    Ok(())
}

fn prune_missing(tx: &rusqlite::Transaction, seen: &HashSet<String>) -> DbResult<()> {
    let mut stmt = tx.prepare("SELECT id FROM pages")?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    for id in existing {
        if !seen.contains(&id) {
            delete_page_rows(tx, &id)?;
        }
    }
    mark_broken_links(tx)
}

/// Drop every index row of the given page ids — `pages`, `pages_fts`,
/// outbound `wiki_links`, `page_tags`, `chunks` and, when sqlite-vec is
/// loaded, the matching `chunk_vectors` rows — then re-flag broken links.
/// The targeted counterpart of the prune step in [`rebuild`]: used after
/// an MCP refactor (rename, delete, merge) removed a page file, so the MCP
/// process stops returning an id that no longer exists without paying for
/// a full rebuild (which may have to embed). Rows of pages that were
/// rewritten or newly created are refreshed by the next rebuild, as for
/// every other MCP write.
pub fn forget_pages(conn: &rusqlite::Connection, ids: &[String]) -> DbResult<()> {
    let tx = conn.unchecked_transaction()?;
    for id in ids {
        delete_page_rows(&tx, id)?;
    }
    mark_broken_links(&tx)?;
    tx.commit()?;
    Ok(())
}

/// Delete every index row of one page id. Vector rows go first, by the
/// rowids of the page's chunks (the same pattern the re-index uses);
/// best-effort, because `chunk_vectors` only exists with sqlite-vec.
fn delete_page_rows(conn: &rusqlite::Connection, id: &str) -> DbResult<()> {
    if super::migrations::chunk_vectors_available(conn) {
        let chunk_ids: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT id FROM chunks WHERE page_id = ?1")?;
            stmt.query_map(params![id], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        for cid in &chunk_ids {
            let _ = conn.execute("DELETE FROM chunk_vectors WHERE rowid = ?1", params![cid]);
        }
    }
    conn.execute("DELETE FROM pages WHERE id = ?1", params![id])?;
    conn.execute("DELETE FROM pages_fts WHERE id = ?1", params![id])?;
    conn.execute("DELETE FROM wiki_links WHERE src_id = ?1", params![id])?;
    conn.execute("DELETE FROM page_tags WHERE page_id = ?1", params![id])?;
    conn.execute("DELETE FROM chunks WHERE page_id = ?1", params![id])?;
    Ok(())
}

/// Mark broken outbound links so the search/graph can flag them.
fn mark_broken_links(conn: &rusqlite::Connection) -> DbResult<()> {
    conn.execute(
        "UPDATE wiki_links SET broken = CASE \
            WHEN dst_id IN (SELECT id FROM pages) THEN 0 \
            ELSE 1 \
         END",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::ensure_skeleton;
    use tempfile::TempDir;

    fn write_page(vault: &Path, sub: &str, slug: &str, body: &str, tags: &[&str]) {
        let dir = wiki_dir(vault).join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        let tags_yaml = format!(
            "[{}]",
            tags.iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        std::fs::write(
            dir.join(format!("{slug}.md")),
            format!(
                "---\nid: {sub}/{slug}\ntype: entity\ntitle: T\ntags: {tags_yaml}\ncreated: 2026-04-29\nupdated: 2026-04-29\n---\n\n{body}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn rebuild_indexes_pages_into_sqlite_with_fts5_searchable_body() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "Alice talks about NLSpec.", &["spec"]);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild(&db, tmp.path()).unwrap();
        db.with(|conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM pages_fts WHERE pages_fts MATCH 'nlspec'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn rebuild_prunes_pages_whose_source_file_was_deleted() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "x", &[]);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild(&db, tmp.path()).unwrap();
        std::fs::remove_file(wiki_dir(tmp.path()).join("entities").join("alice.md")).unwrap();
        rebuild(&db, tmp.path()).unwrap();
        db.with(|conn| {
            let count: i64 = conn
                .query_row("SELECT count(*) FROM pages", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 0);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn forget_pages_removes_only_the_named_ids_from_the_index() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "x", &[]);
        write_page(tmp.path(), "entities", "bob", "y", &[]);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild(&db, tmp.path()).unwrap();
        db.with(|conn| {
            forget_pages(conn, &["entities/alice".to_string()])?;
            let ids: Vec<String> = conn
                .prepare("SELECT id FROM pages ORDER BY id")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            assert_eq!(ids, vec!["entities/bob".to_string()]);
            Ok(())
        })
        .unwrap();
    }

    /// Rows left behind for `id` in every per-page index table (and in
    /// `chunk_vectors`, counted via the vector rowids that no longer have
    /// a chunk, when sqlite-vec is loaded).
    fn leftover_rows(conn: &rusqlite::Connection, id: &str) -> i64 {
        let count = |sql: &str| -> i64 {
            conn.query_row(sql, params![id], |row| row.get(0)).unwrap()
        };
        let mut total = count("SELECT count(*) FROM pages_fts WHERE id = ?1")
            + count("SELECT count(*) FROM chunks WHERE page_id = ?1")
            + count("SELECT count(*) FROM page_tags WHERE page_id = ?1")
            + count("SELECT count(*) FROM wiki_links WHERE src_id = ?1");
        if crate::db::migrations::chunk_vectors_available(conn) {
            total += conn
                .query_row(
                    "SELECT count(*) FROM chunk_vectors \
                     WHERE rowid NOT IN (SELECT id FROM chunks)",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap();
        }
        total
    }

    #[test]
    fn forget_pages_leaves_no_fts_chunk_tag_link_or_vector_rows_for_the_id() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "Alice links [[entities/bob]].", &["t"]);
        write_page(tmp.path(), "entities", "bob", "y", &[]);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild(&db, tmp.path()).unwrap();
        let left = db
            .with(|conn| {
                forget_pages(conn, &["entities/alice".to_string()])?;
                Ok(leftover_rows(conn, "entities/alice"))
            })
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn rebuild_marks_outbound_links_to_missing_pages_as_broken() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "see [[entities/missing]]", &[]);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild(&db, tmp.path()).unwrap();
        db.with(|conn| {
            let broken: i64 = conn
                .query_row(
                    "SELECT count(*) FROM wiki_links WHERE broken = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(broken, 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn rebuild_indexes_tags_for_each_page() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "x", &["nis2", "customer"]);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild(&db, tmp.path()).unwrap();
        db.with(|conn| {
            let tags: Vec<String> = conn
                .prepare("SELECT tag FROM page_tags WHERE page_id = 'entities/alice' ORDER BY tag")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert_eq!(tags, vec!["customer".to_string(), "nis2".to_string()]);
            Ok(())
        })
        .unwrap();
    }

    /// Regression for the slow-bootstrap bug: a second `rebuild` over an
    /// unchanged vault must NOT call `embedder.embed()` for any page —
    /// previously it re-generated every chunk on every mount, freezing
    /// startup for 30-60 s when bge-m3 was active.
    #[test]
    fn rebuild_skips_embedding_when_file_hash_is_unchanged() {
        use crate::embedding::Embedder;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingEmbedder {
            calls: AtomicUsize,
        }
        impl Embedder for CountingEmbedder {
            fn dim(&self) -> usize { crate::embedding::EMBED_DIM }
            fn name(&self) -> &'static str { "counting" }
            fn embed(&self, _text: &str) -> Vec<f32> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                vec![0.0; crate::embedding::EMBED_DIM]
            }
        }

        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "Alice and Bob.", &[]);
        write_page(tmp.path(), "concepts", "nlspec", "NLSpec body.", &[]);
        let db = DbHandle::open(tmp.path()).unwrap();

        let embedder = CountingEmbedder { calls: AtomicUsize::new(0) };
        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        let first_run = embedder.calls.load(Ordering::SeqCst);
        assert!(
            first_run > 0,
            "first rebuild must populate chunks (got {first_run} embed calls)"
        );

        // Second rebuild over an unchanged vault: must not embed anything.
        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        let second_run = embedder.calls.load(Ordering::SeqCst);
        assert_eq!(
            second_run, first_run,
            "rebuild over unchanged vault re-embedded ({} new calls)",
            second_run - first_run
        );
    }

    /// Bumping `INDEX_FORMAT_VERSION` (indexer logic changed) must
    /// invalidate the file_hash skip-fast-path for one run. Simulates
    /// the upgrade scenario where a code change starts capturing more
    /// data per page and the existing DB rows would otherwise be left
    /// stale.
    #[test]
    fn rebuild_forces_full_reindex_when_index_format_version_changes() {
        use crate::embedding::Embedder;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingEmbedder { calls: AtomicUsize }
        impl Embedder for CountingEmbedder {
            fn dim(&self) -> usize { crate::embedding::EMBED_DIM }
            fn name(&self) -> &'static str { "counting" }
            fn embed(&self, _t: &str) -> Vec<f32> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                vec![0.0; crate::embedding::EMBED_DIM]
            }
        }

        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "body", &[]);
        let db = DbHandle::open(tmp.path()).unwrap();
        let embedder = CountingEmbedder { calls: AtomicUsize::new(0) };

        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        let baseline = embedder.calls.load(Ordering::SeqCst);
        assert!(baseline > 0);

        // Simulate "an old DB" by manually rolling the stored version
        // back. The next rebuild must re-embed everything despite
        // unchanged file hashes.
        db.with(|conn| {
            conn.execute(
                "UPDATE schema_meta SET value='0' WHERE key='index_format_version'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        let after = embedder.calls.load(Ordering::SeqCst);
        assert!(
            after > baseline,
            "version-bump should force re-embedding ({baseline} -> {after})"
        );
    }

    /// Editing a page must trigger re-embedding of *that* page (and only
    /// that page) on the next rebuild.
    #[test]
    fn rebuild_re_embeds_only_pages_whose_file_hash_changed() {
        use crate::embedding::Embedder;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingEmbedder { calls: AtomicUsize }
        impl Embedder for CountingEmbedder {
            fn dim(&self) -> usize { crate::embedding::EMBED_DIM }
            fn name(&self) -> &'static str { "counting" }
            fn embed(&self, _text: &str) -> Vec<f32> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                vec![0.0; crate::embedding::EMBED_DIM]
            }
        }

        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "Original body.", &[]);
        write_page(tmp.path(), "concepts", "nlspec", "Untouched.", &[]);
        let db = DbHandle::open(tmp.path()).unwrap();

        let embedder = CountingEmbedder { calls: AtomicUsize::new(0) };
        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        let initial = embedder.calls.load(Ordering::SeqCst);

        // Mutate alice; leave nlspec alone.
        write_page(tmp.path(), "entities", "alice", "REWRITTEN body.", &[]);
        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        let after_edit = embedder.calls.load(Ordering::SeqCst);

        // Some embeds happened (alice was re-indexed) but fewer than the
        // initial run, because nlspec stayed in the skip-fast-path.
        assert!(
            after_edit > initial,
            "edited page should have triggered re-embedding"
        );
        assert!(
            after_edit - initial < initial,
            "untouched page should have stayed cached (initial={initial}, delta={})",
            after_edit - initial
        );
    }

    /// Fake embedder that records every text it was asked to embed.
    struct RecordingEmbedder {
        inputs: std::sync::Mutex<Vec<String>>,
    }
    impl RecordingEmbedder {
        fn new() -> Self {
            Self { inputs: std::sync::Mutex::new(Vec::new()) }
        }
        fn count(&self) -> usize {
            self.inputs.lock().unwrap().len()
        }
    }
    impl crate::embedding::Embedder for RecordingEmbedder {
        fn dim(&self) -> usize { crate::embedding::EMBED_DIM }
        fn name(&self) -> &'static str { "recording" }
        fn embed(&self, text: &str) -> Vec<f32> {
            self.inputs.lock().unwrap().push(text.to_string());
            vec![0.0; crate::embedding::EMBED_DIM]
        }
    }

    const CONTRACT_BODY: &str = "# Vertrag

## Laufzeit

the contract renews for 12 months";

    #[test]
    fn rebuild_embeds_the_chunk_with_a_context_header_starting_with_the_page_title() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "kunde-a", CONTRACT_BODY, &[]);
        let db = DbHandle::open(tmp.path()).unwrap();
        let embedder = RecordingEmbedder::new();
        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        let inputs = embedder.inputs.lock().unwrap().clone();
        assert_eq!(
            inputs,
            vec!["T (entity) › Vertrag › Laufzeit

the contract renews for 12 months".to_string()]
        );
    }

    #[test]
    fn rebuild_stores_the_original_chunk_text_without_the_context_header() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "kunde-a", CONTRACT_BODY, &[]);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild_with(&db, tmp.path(), &RecordingEmbedder::new()).unwrap();
        let stored: Vec<String> = db
            .with(|conn| {
                let mut stmt = conn.prepare("SELECT text FROM chunks ORDER BY chunk_idx")?;
                let rows = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(stored, vec!["the contract renews for 12 months".to_string()]);
    }

    /// Upgrade path: an index written by the pre-contextual indexer (format
    /// v2) holds context-free vectors. The first rebuild after the update
    /// must re-embed a page even though its file hash is unchanged, and
    /// the rebuild after that must not embed anything again.
    #[test]
    fn upgrading_from_format_v2_re_embeds_an_unchanged_page_exactly_once() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "kunde-a", CONTRACT_BODY, &[]);
        let db = DbHandle::open(tmp.path()).unwrap();
        let embedder = RecordingEmbedder::new();
        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        db.with(|conn| {
            conn.execute(
                "UPDATE schema_meta SET value='2' WHERE key='index_format_version'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

        let before = embedder.count();
        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        let upgrade_run = embedder.count() - before;
        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        let next_run = embedder.count() - before - upgrade_run;

        assert_eq!((upgrade_run, next_run), (1, 0));
    }

    // ---- G4: batched rebuild ------------------------------------------

    /// Writes `n` pages `entities/page-000` … whose bodies name their slug,
    /// so an embedder can recognise a page by its embed input.
    fn write_numbered_pages(vault: &Path, n: usize) {
        for i in 0..n {
            let slug = format!("page-{i:03}");
            write_page(vault, "entities", &slug, &format!("Body of {slug}."), &[]);
        }
    }

    /// Embedder that panics on the page whose embed input contains
    /// `fail_on` — the stand-in for a model failure mid-rebuild.
    struct FailingEmbedder {
        fail_on: &'static str,
    }
    impl crate::embedding::Embedder for FailingEmbedder {
        fn dim(&self) -> usize { crate::embedding::EMBED_DIM }
        fn name(&self) -> &'static str { "failing" }
        fn embed(&self, text: &str) -> Vec<f32> {
            assert!(!text.contains(self.fail_on), "simulated embedder failure");
            vec![0.0; crate::embedding::EMBED_DIM]
        }
    }

    fn stored_meta(db: &DbHandle, key: &str) -> Option<String> {
        db.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT value FROM schema_meta WHERE key = ?1",
                    params![key],
                    |row| row.get::<_, String>(0),
                )
                .ok())
        })
        .unwrap()
    }

    /// Runs a rebuild that dies on page 70 (second batch of 50). The
    /// panic happens while embedding, i.e. with the connection unlocked.
    fn interrupted_rebuild(db: &DbHandle, vault: &Path) {
        let failing = FailingEmbedder { fail_on: "page-070" };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rebuild_with(db, vault, &failing)
        }));
        assert!(outcome.is_err(), "the failing embedder must abort the rebuild");
    }

    #[test]
    fn rebuild_of_120_pages_commits_three_batches_each_visible_to_readers_before_the_next() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 120);
        let db = DbHandle::open(tmp.path()).unwrap();
        // The callback runs after each commit with the connection lock
        // released: a reader (like a queued search) gets in and already
        // sees the committed batch.
        let visible = std::sync::Mutex::new(Vec::new());
        let progress = |_done: usize, _total: usize| {
            let pages: i64 = db
                .with(|conn| Ok(conn.query_row("SELECT count(*) FROM pages", [], |r| r.get(0))?))
                .unwrap();
            visible.lock().unwrap().push(pages);
        };
        rebuild_with_batches(&db, tmp.path(), &RecordingEmbedder::new(), 50, Some(&progress))
            .unwrap();
        assert_eq!(*visible.lock().unwrap(), vec![50, 100, 120]);
    }

    #[test]
    fn rebuild_reports_progress_as_pages_done_out_of_pages_total() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 120);
        let db = DbHandle::open(tmp.path()).unwrap();
        let reports = std::sync::Mutex::new(Vec::new());
        let progress = |done: usize, total: usize| reports.lock().unwrap().push((done, total));
        rebuild_with_batches(&db, tmp.path(), &RecordingEmbedder::new(), 50, Some(&progress))
            .unwrap();
        assert_eq!(*reports.lock().unwrap(), vec![(50, 120), (100, 120), (120, 120)]);
    }

    #[test]
    fn an_interrupted_rebuild_leaves_the_index_format_version_unwritten() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 120);
        let db = DbHandle::open(tmp.path()).unwrap();
        interrupted_rebuild(&db, tmp.path());
        assert_eq!(stored_meta(&db, VERSION_KEY), None);
    }

    #[test]
    fn an_interrupted_format_upgrade_keeps_the_old_index_format_version() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 120);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild_with(&db, tmp.path(), &RecordingEmbedder::new()).unwrap();
        db.with(|conn| write_meta(conn, VERSION_KEY, "2")).unwrap();
        interrupted_rebuild(&db, tmp.path());
        assert_eq!(stored_meta(&db, VERSION_KEY), Some("2".to_string()));
    }

    #[test]
    fn a_rebuild_after_an_interruption_embeds_only_the_pages_the_interrupted_run_did_not_commit() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 120);
        let db = DbHandle::open(tmp.path()).unwrap();
        interrupted_rebuild(&db, tmp.path());
        let embedder = RecordingEmbedder::new();
        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        // Batch 1 (pages 0-49) was committed before the failure; pages
        // 50-119 (one chunk each) are what is left.
        assert_eq!(embedder.count(), 70);
    }

    #[test]
    fn a_resumed_format_upgrade_does_not_re_embed_the_batches_committed_before_the_interruption() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 120);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild_with(&db, tmp.path(), &RecordingEmbedder::new()).unwrap();
        db.with(|conn| write_meta(conn, VERSION_KEY, "2")).unwrap();
        interrupted_rebuild(&db, tmp.path());
        let embedder = RecordingEmbedder::new();
        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        assert_eq!(embedder.count(), 70);
    }

    #[test]
    fn a_rebuild_after_an_interruption_indexes_every_page() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 120);
        let db = DbHandle::open(tmp.path()).unwrap();
        interrupted_rebuild(&db, tmp.path());
        rebuild_with(&db, tmp.path(), &RecordingEmbedder::new()).unwrap();
        let pages_with_chunks: i64 = db
            .with(|conn| {
                Ok(conn.query_row(
                    "SELECT count(DISTINCT page_id) FROM chunks",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(pages_with_chunks, 120);
    }

    #[test]
    fn a_rebuild_after_an_interruption_writes_the_current_index_format_version() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 120);
        let db = DbHandle::open(tmp.path()).unwrap();
        interrupted_rebuild(&db, tmp.path());
        rebuild_with(&db, tmp.path(), &RecordingEmbedder::new()).unwrap();
        assert_eq!(
            (stored_meta(&db, VERSION_KEY), stored_meta(&db, PENDING_KEY)),
            (Some(INDEX_FORMAT_VERSION.to_string()), None)
        );
    }

    #[test]
    fn a_third_rebuild_after_a_resumed_one_embeds_nothing() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 120);
        let db = DbHandle::open(tmp.path()).unwrap();
        interrupted_rebuild(&db, tmp.path());
        rebuild_with(&db, tmp.path(), &RecordingEmbedder::new()).unwrap();
        let third = RecordingEmbedder::new();
        rebuild_with(&db, tmp.path(), &third).unwrap();
        assert_eq!(third.count(), 0);
    }

    #[test]
    fn invalidate_all_pages_makes_the_next_rebuild_re_embed_every_page() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 3);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild_with(&db, tmp.path(), &RecordingEmbedder::new()).unwrap();
        db.with(invalidate_all_pages).unwrap();
        let embedder = RecordingEmbedder::new();
        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        assert_eq!(embedder.count(), 3);
    }

    #[test]
    fn a_page_deleted_between_batches_is_pruned_and_the_rebuild_completes() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 120);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild_with(&db, tmp.path(), &RecordingEmbedder::new()).unwrap();
        let victim = wiki_dir(tmp.path()).join("entities").join("page-110.md");
        // Deleted after batch 1 committed, before batch 3 reads it.
        let progress = |done: usize, _total: usize| {
            if done == 50 {
                std::fs::remove_file(&victim).unwrap();
            }
        };
        db.with(invalidate_all_pages).unwrap();
        rebuild_with_batches(&db, tmp.path(), &RecordingEmbedder::new(), 50, Some(&progress))
            .unwrap();
        let pages: i64 = db
            .with(|conn| Ok(conn.query_row("SELECT count(*) FROM pages", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(
            (pages, stored_meta(&db, VERSION_KEY)),
            (119, Some(INDEX_FORMAT_VERSION.to_string()))
        );
    }

    #[test]
    fn an_interrupted_forced_rebuild_resumes_with_only_the_pages_it_did_not_commit() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 120);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild_with(&db, tmp.path(), &RecordingEmbedder::new()).unwrap();
        db.with(invalidate_all_pages).unwrap();
        interrupted_rebuild(&db, tmp.path());
        let embedder = RecordingEmbedder::new();
        rebuild_with(&db, tmp.path(), &embedder).unwrap();
        assert_eq!(embedder.count(), 70);
    }

    // ---- which embedder wrote the index -----------------------------------

    /// Fake embedder with a chosen name (e.g. "bge-m3").
    struct NamedEmbedder(&'static str);
    impl crate::embedding::Embedder for NamedEmbedder {
        fn dim(&self) -> usize { crate::embedding::EMBED_DIM }
        fn name(&self) -> &'static str { self.0 }
        fn embed(&self, _text: &str) -> Vec<f32> {
            vec![0.0; crate::embedding::EMBED_DIM]
        }
    }

    #[test]
    fn a_full_rebuild_records_the_embedder_that_wrote_the_vectors() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 3);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild_with(&db, tmp.path(), &NamedEmbedder("bge-m3")).unwrap();
        assert_eq!(stored_meta(&db, EMBEDDER_KEY), Some("bge-m3".to_string()));
    }

    #[test]
    fn re_embedding_some_pages_with_another_embedder_records_a_mixed_index() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 3);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild_with(&db, tmp.path(), &NamedEmbedder("hashed-fh-1024")).unwrap();
        write_page(tmp.path(), "entities", "page-001", "Edited body.", &[]);
        rebuild_with(&db, tmp.path(), &NamedEmbedder("bge-m3")).unwrap();
        assert_eq!(stored_meta(&db, EMBEDDER_KEY), Some(MIXED_EMBEDDERS.to_string()));
    }

    #[test]
    fn re_embedding_some_pages_with_the_same_embedder_keeps_the_record() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 3);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild_with(&db, tmp.path(), &NamedEmbedder("bge-m3")).unwrap();
        write_page(tmp.path(), "entities", "page-001", "Edited body.", &[]);
        rebuild_with(&db, tmp.path(), &NamedEmbedder("bge-m3")).unwrap();
        assert_eq!(stored_meta(&db, EMBEDDER_KEY), Some("bge-m3".to_string()));
    }

    #[test]
    fn a_forced_re_index_turns_a_fallback_index_into_a_model_index() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_numbered_pages(tmp.path(), 3);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild_with(&db, tmp.path(), &NamedEmbedder("hashed-fh-1024")).unwrap();
        db.with(invalidate_all_pages).unwrap();
        rebuild_with(&db, tmp.path(), &NamedEmbedder("bge-m3")).unwrap();
        assert_eq!(stored_meta(&db, EMBEDDER_KEY), Some("bge-m3".to_string()));
    }

    // ---- links to headings ------------------------------------------------

    fn stored_link_targets(db: &DbHandle) -> Vec<String> {
        db.with(|conn| {
            let mut stmt = conn.prepare("SELECT dst_id FROM wiki_links ORDER BY dst_id")?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .unwrap()
    }

    #[test]
    fn a_link_to_a_heading_is_indexed_as_a_link_to_the_page() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "see [[entities/bob#Contract]]", &[]);
        write_page(tmp.path(), "entities", "bob", "hi", &[]);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild_with(&db, tmp.path(), &RecordingEmbedder::new()).unwrap();
        assert_eq!(stored_link_targets(&db), vec!["entities/bob".to_string()]);
    }

    #[test]
    fn a_link_to_a_heading_of_an_existing_page_is_not_marked_broken() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "see [[entities/bob#Contract]]", &[]);
        write_page(tmp.path(), "entities", "bob", "hi", &[]);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild_with(&db, tmp.path(), &RecordingEmbedder::new()).unwrap();
        let broken: i64 = db
            .with(|conn| {
                Ok(conn.query_row("SELECT count(*) FROM wiki_links WHERE broken = 1", [], |r| {
                    r.get(0)
                })?)
            })
            .unwrap();
        assert_eq!(broken, 0);
    }

    #[test]
    fn indexing_a_heading_link_leaves_the_page_text_untouched() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "see [[entities/bob#Contract]]", &[]);
        let db = DbHandle::open(tmp.path()).unwrap();
        rebuild_with(&db, tmp.path(), &RecordingEmbedder::new()).unwrap();
        let body: String = db
            .with(|conn| {
                Ok(conn.query_row("SELECT body FROM pages WHERE id = 'entities/alice'", [], |r| {
                    r.get(0)
                })?)
            })
            .unwrap();
        assert!(body.contains("[[entities/bob#Contract]]"), "{body}");
    }
}
