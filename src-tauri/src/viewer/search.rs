//! Tier-2 search and backlinks.
//!
//! Search uses FTS5 BM25 ranking when the SQLite index is available. When
//! no index handle is present (e.g. shortly after mount, before the first
//! rebuild) we fall back to a brute-force walk over the Markdown bodies.
//! The result shape matches across both paths so callers don't have to
//! branch.

use std::path::Path;

use serde::Serialize;

use crate::db::{migrations, DbHandle};
use crate::embedding::{bytes_to_vec, cosine, vec_to_bytes};
use crate::vault::layout::{wiki_dir, WIKI_SUBDIRS};
use crate::wiki::page::{extract_wiki_links, parse};

use super::ViewerResult;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchHit {
    pub id: String,
    pub title: String,
    pub path: String,
    pub snippet: String,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BacklinkInfo {
    pub id: String,
    pub title: String,
    pub path: String,
}

pub fn search(vault: &Path, query: &str) -> ViewerResult<Vec<SearchHit>> {
    search_with_db(vault, query, None)
}

/// Search using FTS5 + cosine fusion if `db` is provided, falling back to
/// filesystem walk otherwise. Public so the Tauri command layer can pass
/// the handle from `AppState`.
pub fn search_with_db(
    vault: &Path,
    query: &str,
    db: Option<&DbHandle>,
) -> ViewerResult<Vec<SearchHit>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    if let Some(handle) = db {
        if let Ok(hits) = search_hybrid(handle, vault, query) {
            if !hits.is_empty() {
                return Ok(hits);
            }
        }
    }
    search_brute_force(vault, query)
}

/// Hybrid search: combines FTS5 BM25 with cosine similarity over chunk
/// embeddings. When the `chunk_vectors` (sqlite-vec) virtual table is
/// available we use a single KNN sub-query to fetch the top 50 nearest
/// chunks; otherwise we fall back to brute-force cosine over the BLOB
/// column on chunks of the FTS candidates.
///
/// Score fusion uses Reciprocal Rank Fusion (RRF): for each result that
/// appears in either ranked list, `score = 1/(k+rank_fts) + 1/(k+rank_vec)`
/// with `k=60` per the canonical RRF paper. RRF is robust against
/// score-scale mismatches between BM25 and cosine.
fn search_hybrid(db: &DbHandle, vault: &Path, query: &str) -> ViewerResult<Vec<SearchHit>> {
    let embedder = crate::embedding::for_vault(vault);
    let q_vec = embedder.embed(query);
    let q = sanitize_fts_query(query);

    let result = db.with(|conn| {
        // FTS5 candidates.
        let mut stmt = conn.prepare(
            "SELECT pf.id, p.title, p.path, snippet(pages_fts, 2, '«', '»', ' … ', 24) \
             FROM pages_fts pf \
             LEFT JOIN pages p ON p.id = pf.id \
             WHERE pages_fts MATCH ?1 \
             ORDER BY bm25(pages_fts) ASC \
             LIMIT 50",
        )?;
        type FtsRow = (String, Option<String>, Option<String>, String);
        let fts_rows: Vec<FtsRow> = stmt
            .query_map([&q], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3).unwrap_or_default(),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        // Per-page metadata cache so vector-only hits get a title and path.
        let mut meta: std::collections::HashMap<String, (Option<String>, Option<String>, String)> =
            fts_rows
                .iter()
                .map(|(id, title, path, snippet)| {
                    (id.clone(), (title.clone(), path.clone(), snippet.clone()))
                })
                .collect();

        // Vec candidates — KNN over chunk_vectors when present, else
        // brute-force over the FTS candidates' embedding BLOBs.
        let vec_rank: std::collections::HashMap<String, usize> =
            if migrations::chunk_vectors_available(conn) {
                knn_top_pages(conn, &q_vec, 50)?
            } else {
                bruteforce_top_pages(conn, &q_vec, &fts_rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>())?
            };

        // Pull metadata for any vector-only page that didn't appear in the
        // FTS list, so the result row has a real title/path.
        for id in vec_rank.keys() {
            if !meta.contains_key(id) {
                let row = conn
                    .query_row(
                        "SELECT title, path FROM pages WHERE id = ?1",
                        [id],
                        |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?)),
                    )
                    .ok();
                if let Some((title, path)) = row {
                    meta.insert(id.clone(), (title, path, String::new()));
                }
            }
        }

        // RRF score fusion.
        const K: f32 = 60.0;
        let mut score: std::collections::HashMap<String, f32> =
            std::collections::HashMap::new();
        for (rank, (id, _, _, _)) in fts_rows.iter().enumerate() {
            *score.entry(id.clone()).or_default() += 1.0 / (K + rank as f32);
        }
        for (id, rank) in &vec_rank {
            *score.entry(id.clone()).or_default() += 1.0 / (K + *rank as f32);
        }

        let mut hits: Vec<SearchHit> = score
            .into_iter()
            .map(|(id, s)| {
                let (title, path, snippet) = meta
                    .get(&id)
                    .cloned()
                    .unwrap_or((None, None, String::new()));
                SearchHit {
                    title: title.unwrap_or_else(|| id.clone()),
                    path: path.unwrap_or_default(),
                    id,
                    snippet,
                    score: s,
                }
            })
            .collect();
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(20);
        Ok(hits)
    });
    result.map_err(|err| super::ViewerError::Io(std::io::Error::other(err.to_string())))
}

/// KNN top-K via sqlite-vec. Aggregates chunks → pages by best chunk rank.
fn knn_top_pages(
    conn: &rusqlite::Connection,
    q_vec: &[f32],
    k: usize,
) -> Result<std::collections::HashMap<String, usize>, rusqlite::Error> {
    let blob = vec_to_bytes(q_vec);
    // sqlite-vec requires the KNN limit to sit on the vec0 sub-query
    // itself, not on an outer JOIN — otherwise the planner can't push it
    // down and emits "A LIMIT or 'k = ?' constraint is required on vec0
    // knn queries.". `k` is bounded by callers (50 today) and not user
    // input, so inlining the literal isn't an injection vector.
    //
    // We capture the rowid + distance from vec0, then JOIN chunks
    // separately to map back to page_id while preserving the KNN order.
    let sql = format!(
        "SELECT c.page_id FROM chunks c WHERE c.id IN ( \
            SELECT rowid FROM chunk_vectors \
            WHERE embedding MATCH ?1 \
            ORDER BY distance \
            LIMIT {k} \
         )"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params![&blob], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (rank, page_id) in rows.into_iter().enumerate() {
        // Keep the BEST (lowest) rank per page.
        out.entry(page_id).or_insert(rank);
    }
    Ok(out)
}

/// Brute-force fallback when sqlite-vec isn't loaded. Scans `chunks` for
/// the supplied page ids only — bounded by the FTS candidate count.
fn bruteforce_top_pages(
    conn: &rusqlite::Connection,
    q_vec: &[f32],
    candidate_ids: &[&str],
) -> Result<std::collections::HashMap<String, usize>, rusqlite::Error> {
    if candidate_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let placeholders = vec!["?"; candidate_ids.len()].join(",");
    let sql = format!(
        "SELECT page_id, embedding FROM chunks \
         WHERE page_id IN ({placeholders}) AND embedding IS NOT NULL"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(candidate_ids.iter()),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut best_per_page: std::collections::HashMap<String, f32> =
        std::collections::HashMap::new();
    for (page_id, blob) in rows {
        let v = bytes_to_vec(&blob);
        if v.len() != q_vec.len() {
            continue;
        }
        let c = cosine(q_vec, &v);
        let entry = best_per_page.entry(page_id).or_insert(f32::MIN);
        if c > *entry {
            *entry = c;
        }
    }
    let mut sorted: Vec<(String, f32)> = best_per_page.into_iter().collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(sorted
        .into_iter()
        .enumerate()
        .map(|(rank, (id, _))| (id, rank))
        .collect())
}

/// FTS5's MATCH grammar treats `:` and other punctuation specially. For the
/// MVP we strip everything but alphanumerics + spaces, then OR-join the
/// remaining terms so the user gets a forgiving full-text behaviour.
fn sanitize_fts_query(raw: &str) -> String {
    let mut terms: Vec<String> = Vec::new();
    for token in raw.split_whitespace() {
        let cleaned: String = token
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !cleaned.is_empty() {
            terms.push(format!("\"{cleaned}\""));
        }
    }
    if terms.is_empty() {
        "\"\"".to_string()
    } else {
        terms.join(" OR ")
    }
}

fn search_brute_force(vault: &Path, query: &str) -> ViewerResult<Vec<SearchHit>> {
    let needle = query.to_lowercase();
    let mut hits: Vec<SearchHit> = Vec::new();

    walk_pages(vault, |id, path, raw, parsed| {
        let body = parsed.body.to_lowercase();
        let title = parsed.frontmatter.title.clone().unwrap_or_else(|| id.to_string());
        let title_lower = title.to_lowercase();
        let title_hits = title_lower.matches(&needle).count() as f32;
        let body_hits = body.matches(&needle).count() as f32;
        let total = title_hits * 2.0 + body_hits;
        if total > 0.0 {
            let snippet = build_snippet(&parsed.body, &needle).unwrap_or_else(|| {
                raw.lines().take(1).collect::<Vec<_>>().join(" ")
            });
            hits.push(SearchHit {
                id: id.to_string(),
                title,
                path: path.to_string_lossy().to_string(),
                snippet,
                score: total,
            });
        }
    })?;
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    Ok(hits)
}

pub fn backlinks(vault: &Path, target_id: &str) -> ViewerResult<Vec<BacklinkInfo>> {
    let mut out: Vec<BacklinkInfo> = Vec::new();
    walk_pages(vault, |id, path, _raw, parsed| {
        let links = extract_wiki_links(&parsed.body);
        if links.iter().any(|l| l == target_id) {
            out.push(BacklinkInfo {
                id: id.to_string(),
                title: parsed
                    .frontmatter
                    .title
                    .clone()
                    .unwrap_or_else(|| id.to_string()),
                path: path.to_string_lossy().to_string(),
            });
        }
    })?;
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

fn build_snippet(body: &str, needle: &str) -> Option<String> {
    let lower = body.to_lowercase();
    let idx = lower.find(needle)?;
    let start = idx.saturating_sub(40);
    let end = (idx + needle.len() + 60).min(body.len());
    Some(format!("…{}…", body[start..end].replace('\n', " ")))
}

fn walk_pages<F>(vault: &Path, mut callback: F) -> ViewerResult<()>
where
    F: FnMut(&str, &Path, &str, &crate::wiki::page::ParsedPage),
{
    for sub in WIKI_SUBDIRS {
        let dir = wiki_dir(vault).join(sub);
        if !dir.exists() {
            continue;
        }
        visit(&dir, &mut callback)?;
    }
    Ok(())
}

fn visit<F>(dir: &Path, callback: &mut F) -> ViewerResult<()>
where
    F: FnMut(&str, &Path, &str, &crate::wiki::page::ParsedPage),
{
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            visit(&p, callback)?;
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let raw = std::fs::read_to_string(&p)?;
        let Ok(parsed) = parse(&raw) else {
            continue;
        };
        let id = parsed.frontmatter.id.clone();
        callback(&id, &p, &raw, &parsed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::ensure_skeleton;
    use tempfile::TempDir;

    fn write_page(vault: &Path, sub: &str, slug: &str, title: &str, body: &str) {
        let dir = wiki_dir(vault).join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{slug}.md")),
            format!("---\nid: {sub}/{slug}\ntype: entity\ntitle: {title}\ncreated: 2026-04-29\nupdated: 2026-04-29\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn search_returns_hits_sorted_by_score_desc() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "Alice", "Alice loves NLSpec methodology.");
        write_page(tmp.path(), "concepts", "nlspec", "NLSpec", "NLSpec is the methodology for specs.");
        let hits = search(tmp.path(), "nlspec").unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].score >= hits.last().unwrap().score);
    }

    #[test]
    fn search_returns_empty_for_blank_query() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "Alice", "hi");
        assert!(search(tmp.path(), "  ").unwrap().is_empty());
    }

    #[test]
    fn fts5_search_finds_pages_after_index_rebuild() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(
            tmp.path(),
            "concepts",
            "nlspec",
            "NLSpec",
            "NLSpec is a methodology for specs.",
        );
        let db = crate::db::DbHandle::open(tmp.path()).unwrap();
        crate::db::pages_index::rebuild(&db, tmp.path()).unwrap();
        let hits = search_with_db(tmp.path(), "methodology", Some(&db)).unwrap();
        assert!(hits.iter().any(|h| h.id == "concepts/nlspec"));
    }

    #[test]
    fn fts5_search_falls_back_to_brute_force_when_no_db_handle() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "Alice", "alice talks");
        let hits = search_with_db(tmp.path(), "alice", None).unwrap();
        assert!(!hits.is_empty());
    }

    #[test]
    fn sanitize_fts_query_strips_punctuation_and_or_joins_terms() {
        assert_eq!(sanitize_fts_query("nis2 directive!"), "\"nis2\" OR \"directive\"");
    }

    #[test]
    fn backlinks_returns_pages_referencing_the_target_id() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            "Alice",
            "see [[concepts/nlspec]]",
        );
        write_page(tmp.path(), "concepts", "nlspec", "NLSpec", "the method");
        let bl = backlinks(tmp.path(), "concepts/nlspec").unwrap();
        assert_eq!(bl.len(), 1);
        assert_eq!(bl[0].id, "entities/alice");
    }

    #[test]
    fn backlinks_returns_empty_when_no_page_references_target() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "concepts", "lonely", "Lonely", "no inbound links");
        let bl = backlinks(tmp.path(), "concepts/lonely").unwrap();
        assert!(bl.is_empty());
    }
}
