//! Filesystem → SQLite synchronisation for the wiki pages.
//!
//! Walks `02_wiki/<type>/*.md`, parses frontmatter + body, and upserts into
//! `pages`, `pages_fts`, `wiki_links`, and `page_tags`. Pages whose source
//! file disappeared are deleted. The pipeline is intended to run after each
//! auto-commit; the cost is roughly O(N) where N is the changed file count.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::params;
use sha2::{Digest, Sha256};

use crate::embedding::{chunk as chunker, vec_to_bytes, Embedder};
use crate::vault::layout::{wiki_dir, WIKI_SUBDIRS};
use crate::wiki::page::parse;

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
///    Vectors written by v2 carry no page/section context, so every page
///    must be re-embedded exactly once after the update; the version
///    bump below is what forces that (written back in the same
///    transaction, so an interrupted run simply retries next mount and a
///    finished one never repeats).
const INDEX_FORMAT_VERSION: i64 = 3;

/// Rebuild with the process-cached embedder for `vault`, so a re-index
/// after every watcher commit does not reload the bge-m3 weights.
pub fn rebuild(db: &DbHandle, vault: &Path) -> DbResult<()> {
    let embedder = crate::embedding::cached_for_vault(vault);
    rebuild_with(db, vault, embedder.as_ref())
}

pub fn rebuild_with<E: Embedder + ?Sized>(
    db: &DbHandle,
    vault: &Path,
    embedder: &E,
) -> DbResult<()> {
    let wiki = wiki_dir(vault);
    db.with(|conn| {
        let tx = conn.unchecked_transaction()?;

        // Bypass the file_hash skip-fast-path when the indexer's format
        // version was bumped since the last rebuild — pages haven't
        // changed, but the way we extract data from them has, and
        // skipping would leave the DB inconsistent with the new code.
        let stored_version = read_index_version(&tx).unwrap_or(0);
        let force_full = stored_version != INDEX_FORMAT_VERSION;
        if force_full {
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
        }

        let mut seen_ids: HashSet<String> = HashSet::new();
        for sub in WIKI_SUBDIRS {
            let dir = wiki.join(sub);
            if !dir.exists() {
                continue;
            }
            visit(&dir, &tx, &mut seen_ids, embedder, force_full)?;
        }
        prune_missing(&tx, &seen_ids)?;
        write_index_version(&tx, INDEX_FORMAT_VERSION)?;
        tx.commit()?;
        Ok(())
    })
}

fn read_index_version(tx: &rusqlite::Transaction) -> Option<i64> {
    tx.query_row(
        "SELECT value FROM schema_meta WHERE key='index_format_version'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| s.parse::<i64>().ok())
}

fn write_index_version(tx: &rusqlite::Transaction, version: i64) -> DbResult<()> {
    tx.execute(
        "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('index_format_version', ?1)",
        rusqlite::params![version.to_string()],
    )?;
    Ok(())
}

fn visit<E: Embedder + ?Sized>(
    dir: &Path,
    tx: &rusqlite::Transaction,
    seen: &mut HashSet<String>,
    embedder: &E,
    force_full: bool,
) -> DbResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            visit(&p, tx, seen, embedder, force_full)?;
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let raw = std::fs::read_to_string(&p)?;
        let Ok(parsed) = parse(&raw) else { continue };
        let id = parsed.frontmatter.id.clone();
        seen.insert(id.clone());

        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        let file_hash = hex::encode(hasher.finalize());
        let title = parsed.frontmatter.title.clone();
        let updated = parsed.frontmatter.updated.clone();
        let frontmatter_json = serde_json::to_string(&parsed.frontmatter).unwrap_or_default();

        // Fast-path: if the file content is unchanged from the last
        // indexed state AND the chunk rows are still intact, skip the
        // whole re-index for this page. The previous implementation
        // re-embedded every page on every bootstrap, which on a vault
        // with bge-m3 active meant a 30-60 s freeze per startup even
        // when nothing had been edited. We pin the answer to two facts
        // — file_hash matches and chunk_count > 0 — to avoid resurrecting
        // stale chunks if a previous run was interrupted mid-rebuild.
        let prev: Option<(String, i64)> = tx
            .query_row(
                "SELECT p.file_hash, COUNT(c.id) \
                 FROM pages p LEFT JOIN chunks c ON c.page_id = p.id \
                 WHERE p.id = ?1 \
                 GROUP BY p.id",
                params![&id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                    ))
                },
            )
            .ok();
        if let Some((prev_hash, chunk_count)) = prev {
            if !force_full && prev_hash == file_hash && chunk_count > 0 {
                // Touch mtime/path in case the file was moved without
                // changing its bytes (rename + same content). Cheap
                // single-row UPDATE; no embeddings, no FTS rewrite.
                tx.execute(
                    "UPDATE pages SET path=?1, file_mtime=?2 WHERE id=?3",
                    params![&p.to_string_lossy().to_string(), mtime, &id],
                )?;
                continue;
            }
        }

        tx.execute(
            "INSERT INTO pages(id, type, path, title, frontmatter, body, updated_at, file_mtime, file_hash) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(id) DO UPDATE SET \
                type=excluded.type, path=excluded.path, title=excluded.title, \
                frontmatter=excluded.frontmatter, body=excluded.body, updated_at=excluded.updated_at, \
                file_mtime=excluded.file_mtime, file_hash=excluded.file_hash",
            params![
                &id,
                &parsed.frontmatter.page_type,
                &p.to_string_lossy().to_string(),
                title.as_deref(),
                &frontmatter_json,
                &parsed.body,
                updated.as_deref(),
                mtime,
                &file_hash,
            ],
        )?;

        // Refresh FTS row (delete + insert keeps things simple).
        tx.execute("DELETE FROM pages_fts WHERE id = ?1", params![&id])?;
        tx.execute(
            "INSERT INTO pages_fts(id, title, body) VALUES (?1, ?2, ?3)",
            params![&id, title.as_deref().unwrap_or(""), &parsed.body],
        )?;

        // Wiki-links: replace.
        tx.execute("DELETE FROM wiki_links WHERE src_id = ?1", params![&id])?;
        for link in &parsed.wiki_links {
            tx.execute(
                "INSERT OR IGNORE INTO wiki_links(src_id, dst_id, broken) VALUES (?1, ?2, 0)",
                params![&id, link],
            )?;
        }

        // Tags: replace.
        tx.execute("DELETE FROM page_tags WHERE page_id = ?1", params![&id])?;
        for tag in &parsed.frontmatter.tags {
            tx.execute(
                "INSERT OR IGNORE INTO page_tags(page_id, tag) VALUES (?1, ?2)",
                params![&id, tag],
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
            stmt.query_map(params![&id], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        if vec_table_present {
            for cid in &old_chunk_ids {
                let _ = tx.execute(
                    "DELETE FROM chunk_vectors WHERE rowid = ?1",
                    params![cid],
                );
            }
        }
        tx.execute("DELETE FROM chunks WHERE page_id = ?1", params![&id])?;
        // Contextual chunking: the embedder sees "<title> (<type>) › <h1> ›
        // <h2>" in front of each chunk so the vector encodes where the text
        // belongs. Only the bare chunk text is stored — snippets, FTS and
        // the UI never see the header. Queries are embedded bare.
        let context_title = title.as_deref().unwrap_or(&id);
        for (idx, chunk) in chunker::section_chunks(&parsed.body).into_iter().enumerate() {
            let embed_input = chunker::contextual_text(
                context_title,
                &parsed.frontmatter.page_type,
                &chunk.heading_path,
                &chunk.text,
            );
            let v = embedder.embed(&embed_input);
            let blob = vec_to_bytes(&v);
            tx.execute(
                "INSERT INTO chunks(page_id, chunk_idx, text, embedding) VALUES (?1, ?2, ?3, ?4)",
                params![&id, idx as i64, &chunk.text, &blob],
            )?;
            if vec_table_present {
                let chunk_id = tx.last_insert_rowid();
                let _ = tx.execute(
                    "INSERT INTO chunk_vectors(rowid, embedding) VALUES (?1, ?2)",
                    params![chunk_id, &blob],
                );
            }
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
}
