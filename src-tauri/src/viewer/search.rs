//! Tier-2 search and backlinks.
//!
//! Search uses FTS5 BM25 ranking when the SQLite index is available. When
//! no index handle is present (e.g. shortly after mount, before the first
//! rebuild) we fall back to a brute-force walk over the Markdown bodies.
//! The result shape matches across both paths so callers don't have to
//! branch.

use std::path::Path;

use serde::Serialize;

use crate::db::{DbHandle, migrations};
use crate::embedding::{Embedder, bytes_to_vec, cosine, vec_to_bytes};
use crate::vault::layout::{WIKI_SUBDIRS, wiki_dir};
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
/// available we use a single KNN sub-query to fetch the 200 nearest
/// chunks (folded into dense page ranks); otherwise we fall back to brute-force cosine over the BLOB
/// column on chunks of the FTS candidates.
///
/// The two candidate lists are combined by [`fuse`] with the variant
/// [`FUSION`] (dense-first since 0.3.6; see there for why not plain RRF).
///
/// The embedder comes from the process-wide cache, so the bge-m3 weights
/// are loaded once (first search or mount warm-up), not per query.
fn search_hybrid(db: &DbHandle, vault: &Path, query: &str) -> ViewerResult<Vec<SearchHit>> {
    let embedder = crate::embedding::cached_for_vault(vault);
    db.with(|conn| {
        search_hybrid_on_conn(conn, embedder.as_ref(), query).map_err(crate::db::DbError::from)
    })
    .map_err(|err| super::ViewerError::Io(std::io::Error::other(err.to_string())))
}

/// The retrieval paths the eval (B1, `viewer::eval`) compares. `Hybrid` is
/// what `brain_search` and the GUI search run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetrievalMode {
    FtsOnly,
    DenseOnly,
    Hybrid,
}

impl RetrievalMode {
    pub const ALL: [RetrievalMode; 3] = [Self::FtsOnly, Self::DenseOnly, Self::Hybrid];

    pub fn label(self) -> &'static str {
        match self {
            Self::FtsOnly => "fts-only",
            Self::DenseOnly => "dense-only",
            Self::Hybrid => "hybrid",
        }
    }
}

/// FTS candidate PAGES before fusion.
const CANDIDATES: usize = 50;

/// Nearest CHUNKS fetched by the vector KNN before they are folded into
/// page ranks. Larger than [`CANDIDATES`] so one long page with many
/// similar chunks cannot fill the whole window on its own.
const KNN_CHUNKS: usize = 200;

/// BM25 ranking of `pages_fts` with per-column weights, in column order
/// `id` (UNINDEXED, weight irrelevant), `title`, `body`, `summary`: a
/// title hit counts 3×, a summary hit (B2) 5× a body hit. The summary
/// outweighs the title because it is a deliberate one-to-two-sentence
/// description of the page, while titles are often short names. These
/// are starting values, to be tuned with `brain eval` on real vaults.
const FTS_RANK: &str = "bm25(pages_fts, 0.0, 3.0, 1.0, 5.0)";

/// One FTS candidate: id, title, path, snippet, relevance (`-bm25`,
/// higher is better).
type FtsRow = (String, Option<String>, Option<String>, String, f32);

/// The FTS5 candidates for an already sanitised MATCH query, best first
/// (ties by id, so the order is deterministic). The snippet comes from
/// whichever column matched best (`-1`), so a hit only in the title or
/// summary is highlighted too.
fn fts_candidates(conn: &rusqlite::Connection, match_query: &str) -> rusqlite::Result<Vec<FtsRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT pf.id, p.title, p.path, snippet(pages_fts, -1, '«', '»', ' … ', 24), \
                -{FTS_RANK} \
         FROM pages_fts pf \
         LEFT JOIN pages p ON p.id = pf.id \
         WHERE pages_fts MATCH ?1 \
         ORDER BY {FTS_RANK} ASC, pf.id ASC \
         LIMIT {CANDIDATES}"
    ))?;
    let rows = stmt
        .query_map([match_query], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3).unwrap_or_default(),
                row.get::<_, f64>(4).unwrap_or(0.0) as f32,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Page ids ranked by ONE retrieval mode, best first, at most `limit`.
/// The eval's entry point: every mode runs the same code `brain_search`
/// runs (FTS candidates, vector candidates, or their RRF fusion), so the
/// numbers describe the real search.
pub fn ranked_ids_on_conn(
    conn: &rusqlite::Connection,
    embedder: &dyn Embedder,
    query: &str,
    mode: RetrievalMode,
    limit: usize,
) -> rusqlite::Result<Vec<String>> {
    let mut ids: Vec<String> = match mode {
        RetrievalMode::FtsOnly => fts_candidates(conn, &sanitize_fts_query(query))?
            .into_iter()
            .map(|row| row.0)
            .collect(),
        RetrievalMode::DenseOnly => {
            let q_vec = embedder.embed(query);
            let ranked = if migrations::chunk_vectors_available(conn) {
                knn_top_pages(conn, &q_vec, KNN_CHUNKS)?
            } else {
                bruteforce_top_pages(conn, &q_vec, None)?
            };
            ranked.into_iter().map(|(id, _)| id).collect()
        }
        RetrievalMode::Hybrid => hybrid_ids_on_conn(conn, embedder, query, FUSION, limit)?,
    };
    ids.truncate(limit);
    Ok(ids)
}

/// Hybrid page ids for `query` fused with `fusion`, best first, at most
/// `limit` — the eval's way to compare fusion variants on the same
/// candidates (`brain eval` prints all of them).
pub fn hybrid_ids_on_conn(
    conn: &rusqlite::Connection,
    embedder: &dyn Embedder,
    query: &str,
    fusion: Fusion,
    limit: usize,
) -> rusqlite::Result<Vec<String>> {
    let q_vec = embedder.embed(query);
    let fts_rows = fts_candidates(conn, &sanitize_fts_query(query))?;
    let dense = dense_candidates(conn, &q_vec, &fts_rows)?;
    let fts: Vec<(String, f32)> = fts_rows.into_iter().map(|r| (r.0, r.4)).collect();
    let mut ids: Vec<String> = fuse(&fts, &dense, fusion)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    ids.truncate(limit);
    Ok(ids)
}

/// How the FTS and the dense candidate lists are combined.
///
/// Measured on a real vault (50 curated queries, bge-m3) plain RRF made
/// hybrid search clearly WORSE than dense search alone (Recall@10 0.80
/// vs 0.96): unrelated FTS hits that also sit deep in the dense list
/// collect two reciprocal ranks and push the dense top hit out of the top
/// 10. The variants below are the candidates of the 0.3.6 fusion tuning;
/// `brain eval` prints the metrics of every variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fusion {
    /// `1/(k+rank_fts) + 1/(k+rank_dense)`, k = 60 (until 0.3.5).
    Rrf,
    /// RRF with the FTS contribution weighted down
    /// ([`WEIGHTED_RRF_FTS_WEIGHT`]).
    WeightedRrf,
    /// RRF over the FTS hits whose relevance is at least
    /// [`WEAK_FTS_FRACTION`] of the query's best FTS hit only.
    WeakFtsIgnored,
    /// The dense ranking is the base; FTS only boosts pages that are in
    /// BOTH lists. A page found only by FTS scores below every page in the
    /// first ~110 dense ranks, so it never enters the top 10 unless the
    /// dense list has fewer than 10 pages.
    DenseFirst,
    /// `0.7 × dense + 0.3 × fts` over min–max normalised scores.
    Convex,
}

impl Fusion {
    pub const ALL: [Fusion; 5] = [
        Self::Rrf,
        Self::WeightedRrf,
        Self::WeakFtsIgnored,
        Self::DenseFirst,
        Self::Convex,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Rrf => "rrf",
            Self::WeightedRrf => "weighted-rrf",
            Self::WeakFtsIgnored => "weak-fts-ignored",
            Self::DenseFirst => "dense-first",
            Self::Convex => "convex",
        }
    }
}

/// The fusion `brain_search`, the GUI search and the eval's hybrid mode
/// use. An internal choice, not a user setting.
///
/// Dense-first keeps the dense top 10 as the result set whenever the
/// dense list has at least 10 pages, so hybrid Recall@10 can no longer
/// fall below dense-only Recall@10 — the failure measured with RRF. The
/// FTS boost only reorders pages both lists found. Its strength
/// ([`DENSE_FIRST_K`], [`DENSE_FIRST_FTS_WEIGHT`]) was chosen, not yet
/// measured; compare with `brain eval` on a real vault.
pub const FUSION: Fusion = Fusion::DenseFirst;

/// RRF constant (the canonical 60).
const RRF_K: f32 = 60.0;
/// FTS weight of [`Fusion::WeightedRrf`] (dense weighs 1.0).
const WEIGHTED_RRF_FTS_WEIGHT: f32 = 0.4;
/// [`Fusion::WeakFtsIgnored`]: FTS hits below this fraction of the best
/// FTS relevance of the query are dropped.
const WEAK_FTS_FRACTION: f32 = 0.5;
/// [`Fusion::DenseFirst`]: dense score `1/(K + rank)`. Smaller than RRF's
/// 60 so neighbouring dense ranks stay distinguishable and the boost can
/// only move a page up by a rank or two near the top.
const DENSE_FIRST_K: f32 = 10.0;
/// [`Fusion::DenseFirst`]: boost `W/(60 + fts_rank)` for a page in both
/// lists (at most 0.0083; the gap between dense ranks 0 and 1 is 0.0091).
const DENSE_FIRST_FTS_WEIGHT: f32 = 0.5;
/// [`Fusion::Convex`]: weight of the normalised dense score.
const CONVEX_DENSE_WEIGHT: f32 = 0.7;

/// Combine the FTS candidates `fts` (id, relevance — higher is better)
/// and the dense candidates `dense` (id, similarity — higher is better),
/// both best first, into one ranking: every id of either list with its
/// fused score, highest first, ties by id. Pure.
pub fn fuse<'a>(
    fts: &'a [(String, f32)],
    dense: &'a [(String, f32)],
    fusion: Fusion,
) -> Vec<(String, f32)> {
    use std::collections::HashMap;
    let mut score: HashMap<&str, f32> = HashMap::new();
    let mut add = |id: &'a str, s: f32| *score.entry(id).or_default() += s;
    match fusion {
        Fusion::Rrf | Fusion::WeightedRrf | Fusion::WeakFtsIgnored => {
            let fts_weight = if fusion == Fusion::WeightedRrf {
                WEIGHTED_RRF_FTS_WEIGHT
            } else {
                1.0
            };
            let best = fts.first().map_or(0.0, |r| r.1);
            let mut rank = 0usize;
            for (id, relevance) in fts {
                if fusion == Fusion::WeakFtsIgnored && *relevance < WEAK_FTS_FRACTION * best {
                    continue;
                }
                add(id, fts_weight / (RRF_K + rank as f32));
                rank += 1;
            }
            for (rank, (id, _)) in dense.iter().enumerate() {
                add(id, 1.0 / (RRF_K + rank as f32));
            }
        }
        Fusion::DenseFirst => {
            for (rank, (id, _)) in dense.iter().enumerate() {
                add(id, 1.0 / (DENSE_FIRST_K + rank as f32));
            }
            // FTS-only pages get the bare boost: below the first ~110
            // dense ranks, so they only fill up a short dense list.
            for (rank, (id, _)) in fts.iter().enumerate() {
                add(id, DENSE_FIRST_FTS_WEIGHT / (RRF_K + rank as f32));
            }
        }
        Fusion::Convex => {
            for (id, s) in normalised(dense) {
                add(id, CONVEX_DENSE_WEIGHT * s);
            }
            for (id, s) in normalised(fts) {
                add(id, (1.0 - CONVEX_DENSE_WEIGHT) * s);
            }
        }
    }
    let mut out: Vec<(String, f32)> = score
        .into_iter()
        .map(|(id, s)| (id.to_string(), s))
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Min–max normalised scores (1.0 for every entry when all are equal).
fn normalised(list: &[(String, f32)]) -> Vec<(&str, f32)> {
    let max = list.iter().map(|r| r.1).fold(f32::NEG_INFINITY, f32::max);
    let min = list.iter().map(|r| r.1).fold(f32::INFINITY, f32::min);
    list.iter()
        .map(|(id, s)| {
            let n = if max > min {
                (s - min) / (max - min)
            } else {
                1.0
            };
            (id.as_str(), n)
        })
        .collect()
}

/// Dense candidate pages, best first, with a similarity (higher is
/// closer): KNN over `chunk_vectors` when present, else brute force over
/// the FTS candidates' chunk vectors.
fn dense_candidates(
    conn: &rusqlite::Connection,
    q_vec: &[f32],
    fts_rows: &[FtsRow],
) -> rusqlite::Result<Vec<(String, f32)>> {
    if migrations::chunk_vectors_available(conn) {
        knn_top_pages(conn, q_vec, KNN_CHUNKS)
    } else {
        let candidates: Vec<&str> = fts_rows.iter().map(|r| r.0.as_str()).collect();
        bruteforce_top_pages(conn, q_vec, Some(&candidates))
    }
}

/// The connection-only core of hybrid search. Split out from
/// [`search_hybrid`] so the MCP server can run it through its
/// timeout/reopen wrapper (`db_op`) — that wrapper owns the
/// `&mut Option<DbHandle>` and therefore must call into a function that
/// takes a bare `&Connection`.
///
/// The embedder is a parameter on purpose: callers obtain it via
/// `embedding::cached_for_vault` BEFORE entering any timeout boundary, so
/// a one-time model load (seconds for the 2.2 GB bge-m3 weights) is never
/// misread as a hung disk. Only the query embedding (fast, CPU) and the
/// DB work run here.
pub fn search_hybrid_on_conn(
    conn: &rusqlite::Connection,
    embedder: &dyn Embedder,
    query: &str,
) -> rusqlite::Result<Vec<SearchHit>> {
    let q_vec = embedder.embed(query);
    let fts_rows = fts_candidates(conn, &sanitize_fts_query(query))?;
    let dense = dense_candidates(conn, &q_vec, &fts_rows)?;

    // Per-page metadata so dense-only hits get a title and path too.
    let mut meta: std::collections::HashMap<String, (Option<String>, Option<String>, String)> =
        fts_rows
            .iter()
            .map(|(id, title, path, snippet, _)| {
                (id.clone(), (title.clone(), path.clone(), snippet.clone()))
            })
            .collect();
    let fts: Vec<(String, f32)> = fts_rows.iter().map(|r| (r.0.clone(), r.4)).collect();
    let mut fused = fuse(&fts, &dense, FUSION);
    fused.truncate(20);
    for (id, _) in &fused {
        if !meta.contains_key(id) {
            let row = conn
                .query_row("SELECT title, path FROM pages WHERE id = ?1", [id], |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                })
                .ok();
            if let Some((title, path)) = row {
                meta.insert(id.clone(), (title, path, String::new()));
            }
        }
    }
    Ok(fused
        .into_iter()
        .map(|(id, score)| {
            let (title, path, snippet) =
                meta.get(&id)
                    .cloned()
                    .unwrap_or((None, None, String::new()));
            // Post-process FTS5's snippet markers so a query token that
            // recurs many times in a single page gets highlighted only on
            // its first occurrence per snippet, and at most
            // MAX_DISTINCT_MARKER_TOKENS distinct tokens at all.
            let snippet = limit_snippet_markers(&snippet, MAX_DISTINCT_MARKER_TOKENS);
            SearchHit {
                title: title.unwrap_or_else(|| id.clone()),
                path: path.unwrap_or_default(),
                id,
                snippet,
                score,
            }
        })
        .collect())
}

/// KNN top-K via sqlite-vec. Aggregates chunks → pages by best chunk,
/// best first, with similarity `-distance`.
fn knn_top_pages(
    conn: &rusqlite::Connection,
    q_vec: &[f32],
    k: usize,
) -> Result<Vec<(String, f32)>, rusqlite::Error> {
    let blob = vec_to_bytes(q_vec);
    // sqlite-vec requires the KNN limit to sit on the vec0 sub-query
    // itself, not on an outer JOIN — otherwise the planner can't push it
    // down and emits "A LIMIT or 'k = ?' constraint is required on vec0
    // knn queries.". `k` is bounded by callers (KNN_CHUNKS = 200) and not user
    // input, so inlining the literal isn't an injection vector.
    //
    // We capture the rowid + distance from vec0, then JOIN chunks
    // separately to map back to page_id while preserving the KNN order:
    // the outer ORDER BY distance is what keeps it. (An earlier
    // `WHERE c.id IN (…)` form returned the chunks in rowid order, so the
    // "rank" was insertion order, not similarity.) A sub-query with LIMIT
    // on the left of a join is never flattened by SQLite, so the KNN
    // limit stays on the vec0 scan.
    let sql = format!(
        "SELECT c.page_id, v.distance FROM ( \
            SELECT rowid, distance FROM chunk_vectors \
            WHERE embedding MATCH ?1 \
            ORDER BY distance \
            LIMIT {k} \
         ) v JOIN chunks c ON c.id = v.rowid \
         ORDER BY v.distance, c.id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params![&blob], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<(String, f32)> = Vec::new();
    for (page_id, distance) in rows {
        // PAGES by their best chunk (the first one in distance order): the
        // FTS list ranks pages, so fusion must see page ranks here too — a
        // page's other chunks must not push the next page down.
        if seen.insert(page_id.clone()) {
            out.push((page_id, -(distance as f32)));
        }
    }
    Ok(out)
}

/// Brute-force fallback when sqlite-vec isn't loaded (pages best first,
/// with their best chunk's cosine). Scans `chunks` for
/// the supplied page ids only — bounded by the FTS candidate count — or,
/// with `None` (the eval's dense-only mode), every chunk.
fn bruteforce_top_pages(
    conn: &rusqlite::Connection,
    q_vec: &[f32],
    candidate_ids: Option<&[&str]>,
) -> Result<Vec<(String, f32)>, rusqlite::Error> {
    let (sql, ids): (String, &[&str]) = match candidate_ids {
        Some([]) => return Ok(Vec::new()),
        Some(ids) => (
            format!(
                "SELECT page_id, embedding FROM chunks \
                 WHERE page_id IN ({}) AND embedding IS NOT NULL",
                vec!["?"; ids.len()].join(",")
            ),
            ids,
        ),
        None => (
            "SELECT page_id, embedding FROM chunks WHERE embedding IS NOT NULL".to_string(),
            &[],
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
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
    sorted.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    Ok(sorted)
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

pub fn search_brute_force(vault: &Path, query: &str) -> ViewerResult<Vec<SearchHit>> {
    let needle = query.to_lowercase();
    let mut hits: Vec<SearchHit> = Vec::new();

    walk_pages(vault, |id, path, raw, parsed| {
        let body = parsed.body.to_lowercase();
        let title = parsed
            .frontmatter
            .title
            .clone()
            .unwrap_or_else(|| id.to_string());
        let title_lower = title.to_lowercase();
        let title_hits = title_lower.matches(&needle).count() as f32;
        let body_hits = body.matches(&needle).count() as f32;
        let total = title_hits * 2.0 + body_hits;
        if total > 0.0 {
            let snippet = build_snippet(&parsed.body, &needle)
                .unwrap_or_else(|| raw.lines().take(1).collect::<Vec<_>>().join(" "));
            hits.push(SearchHit {
                id: id.to_string(),
                title,
                path: path.to_string_lossy().to_string(),
                snippet,
                score: total,
            });
        }
    })?;
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
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

/// Maximum distinct query-token highlights kept inside a single FTS5
/// snippet. FTS5 wraps every occurrence of every matching token in
/// `«…»`, which on a vault where one of the query tokens is the user's
/// own name produces snippets like `«Pascal» dropped «Pascal» note,
/// «Pascal» wrote …` — visually noisy and not informative. Three
/// distinct tokens, first occurrence each, is enough to show why the
/// page ranked.
const MAX_DISTINCT_MARKER_TOKENS: usize = 3;

/// Walks an FTS5-produced snippet and keeps highlight markers only for
/// the first `max_distinct` distinct (case-insensitive) tokens it sees;
/// drops the markers around every later occurrence of an already-seen
/// token, and drops them entirely for tokens beyond the cap. The text
/// content of the snippet is preserved verbatim — only the `«` / `»`
/// characters are removed. Marker pairs are assumed well-formed
/// (FTS5's `snippet()` produces matched pairs in row order); an
/// unterminated `«` at the end of the snippet is silently dropped to
/// keep the output renderable.
fn limit_snippet_markers(snippet: &str, max_distinct: usize) -> String {
    let mut out = String::with_capacity(snippet.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut chars = snippet.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '«' {
            out.push(c);
            continue;
        }
        // Collect everything up to the closing »; if the source is
        // malformed (no closing marker), we drop the opener.
        let mut token = String::new();
        let mut closed = false;
        for inner in chars.by_ref() {
            if inner == '»' {
                closed = true;
                break;
            }
            token.push(inner);
        }
        if !closed {
            // Unterminated marker — drop both the opener and the
            // trailing partial content so the snippet stays clean.
            out.push_str(&token);
            continue;
        }
        let key = token.to_lowercase();
        let already_seen = !seen.insert(key);
        // Keep the markers only for the first occurrence of each
        // distinct token AND only while we are under the cap. Once we
        // are over the cap (or this token was marked before), keep the
        // text and drop the markers.
        if !already_seen && seen.len() <= max_distinct {
            out.push('«');
            out.push_str(&token);
            out.push('»');
        } else {
            out.push_str(&token);
        }
    }
    out
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
    fn limit_snippet_markers_keeps_first_occurrence_of_each_distinct_token() {
        // Repeating a single token within one snippet — FTS5 marks
        // every occurrence; the post-processor keeps only the first.
        let input = "the «pascal» went to «helsinki» and «pascal» came back";
        let out = limit_snippet_markers(input, 3);
        assert_eq!(out, "the «pascal» went to «helsinki» and pascal came back");
    }

    #[test]
    fn limit_snippet_markers_caps_at_max_distinct_tokens() {
        // Four distinct match tokens but cap is 2 → only the first two
        // get markers, the rest keep their text but lose their wrap.
        let input = "«alpha» «beta» «gamma» «delta» «alpha»";
        let out = limit_snippet_markers(input, 2);
        assert_eq!(out, "«alpha» «beta» gamma delta alpha");
    }

    #[test]
    fn limit_snippet_markers_dedupe_is_case_insensitive() {
        // The same word in different cases counts as one distinct
        // token — otherwise "Pascal", "pascal", "PASCAL" would each
        // burn a slot in the cap.
        let input = "«Pascal» visited «pascal» and «PASCAL» too";
        let out = limit_snippet_markers(input, 3);
        assert_eq!(out, "«Pascal» visited pascal and PASCAL too");
    }

    #[test]
    fn limit_snippet_markers_preserves_non_marker_characters() {
        // Surrounding whitespace, punctuation, and ellipsis from
        // FTS5's `snippet()` must pass through untouched.
        let input = " … context before «match» context after … ";
        let out = limit_snippet_markers(input, 3);
        assert_eq!(out, " … context before «match» context after … ");
    }

    #[test]
    fn limit_snippet_markers_drops_unterminated_opener_gracefully() {
        // Defensive: an unterminated `«` (shouldn't happen in
        // practice with FTS5's snippet) must not crash and must not
        // produce a half-marker in the output.
        let input = "well-formed «match» and then dangling «end-of-snippet";
        let out = limit_snippet_markers(input, 3);
        assert_eq!(out, "well-formed «match» and then dangling end-of-snippet");
    }

    #[test]
    fn search_returns_hits_sorted_by_score_desc() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            "Alice",
            "Alice loves NLSpec methodology.",
        );
        write_page(
            tmp.path(),
            "concepts",
            "nlspec",
            "NLSpec",
            "NLSpec is the methodology for specs.",
        );
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
        assert_eq!(
            sanitize_fts_query("nis2 directive!"),
            "\"nis2\" OR \"directive\""
        );
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
        write_page(
            tmp.path(),
            "concepts",
            "lonely",
            "Lonely",
            "no inbound links",
        );
        let bl = backlinks(tmp.path(), "concepts/lonely").unwrap();
        assert!(bl.is_empty());
    }

    /// Embeds every text containing "near" along axis 0, everything else
    /// along axis 1.
    struct AxisEmbedder;
    impl Embedder for AxisEmbedder {
        fn dim(&self) -> usize {
            crate::embedding::EMBED_DIM
        }
        fn name(&self) -> &'static str {
            "axis"
        }
        fn embed(&self, text: &str) -> Vec<f32> {
            let mut v = vec![0.0; crate::embedding::EMBED_DIM];
            v[usize::from(!text.contains("near"))] = 1.0;
            v
        }
    }

    fn ranked(db: &crate::db::DbHandle, query: &str, mode: RetrievalMode) -> Vec<String> {
        db.with(|conn| Ok(ranked_ids_on_conn(conn, &AxisEmbedder, query, mode, 10)?))
            .unwrap()
    }

    #[test]
    fn full_text_ranking_puts_a_summary_hit_above_a_body_only_hit() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let dir = wiki_dir(tmp.path()).join("entities");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.md"),
            "---\nid: entities/a\ntype: entity\ntitle: A\n---\n\nThe body talks about pricing and delivery terms.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("b.md"),
            "---\nid: entities/b\ntype: entity\ntitle: B\nsummary: Contract renewal for Kunde B.\n---\n\nThe body talks about pricing and delivery terms.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("c.md"),
            "---\nid: entities/c\ntype: entity\ntitle: C\n---\n\nThe renewal is mentioned once in this body about pricing and delivery terms.\n",
        )
        .unwrap();
        let db = crate::db::DbHandle::open(tmp.path()).unwrap();
        crate::db::pages_index::rebuild_with(&db, tmp.path(), &AxisEmbedder).unwrap();
        assert_eq!(
            ranked(&db, "renewal", RetrievalMode::FtsOnly),
            vec!["entities/b", "entities/c"]
        );
    }

    #[test]
    fn dense_ranking_orders_pages_by_vector_distance_not_by_chunk_insertion_order() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        // entities/a is indexed (and its chunk inserted) first.
        write_page(tmp.path(), "entities", "a", "A", "far away words");
        write_page(tmp.path(), "entities", "b", "B", "near words");
        let db = crate::db::DbHandle::open(tmp.path()).unwrap();
        crate::db::pages_index::rebuild_with(&db, tmp.path(), &AxisEmbedder).unwrap();
        assert_eq!(
            ranked(&db, "near", RetrievalMode::DenseOnly),
            vec!["entities/b", "entities/a"]
        );
    }

    #[test]
    fn vector_page_ranks_are_dense_even_when_one_page_holds_the_closest_chunks() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        // Three "near" chunks on page a, one "far" chunk on page b.
        write_page(
            tmp.path(),
            "entities",
            "a",
            "A",
            "# X\nnear one\n# Y\nnear two\n# Z\nnear three",
        );
        write_page(tmp.path(), "entities", "b", "B", "far words");
        let db = crate::db::DbHandle::open(tmp.path()).unwrap();
        crate::db::pages_index::rebuild_with(&db, tmp.path(), &AxisEmbedder).unwrap();
        let ranked = db
            .with(|conn| {
                let q = AxisEmbedder.embed("near");
                Ok(if migrations::chunk_vectors_available(conn) {
                    knn_top_pages(conn, &q, KNN_CHUNKS)?
                } else {
                    bruteforce_top_pages(conn, &q, None)?
                })
            })
            .unwrap();
        let ids: Vec<String> = ranked.into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            ids,
            vec!["entities/a".to_string(), "entities/b".to_string()]
        );
    }

    // ---- fusion ---------------------------------------------------------

    fn list(ids: &[&str]) -> Vec<(String, f32)> {
        // Scores descending with the rank, as the real candidate lists.
        ids.iter()
            .enumerate()
            .map(|(i, id)| (id.to_string(), 10.0 - i as f32 * 0.1))
            .collect()
    }

    fn top(fused: &[(String, f32)], n: usize) -> Vec<&str> {
        fused.iter().take(n).map(|(id, _)| id.as_str()).collect()
    }

    /// The measured failure: the right page is dense #1; ten FTS hits that
    /// are unrelated also sit deep in the dense list (ranks 20–29).
    type Candidates = Vec<(String, f32)>;

    fn noisy_lists() -> (Candidates, Candidates) {
        let dense_ids: Vec<String> = (0..40).map(|i| format!("entities/d{i:02}")).collect();
        let dense: Vec<&str> = dense_ids.iter().map(String::as_str).collect();
        let fts: Vec<&str> = dense[20..30].to_vec();
        (list(&fts), list(&dense))
    }

    #[test]
    fn plain_rrf_pushes_the_dense_top_hit_out_of_the_top_ten_under_fts_noise() {
        let (fts, dense) = noisy_lists();
        let fused = fuse(&fts, &dense, Fusion::Rrf);
        assert!(!top(&fused, 10).contains(&"entities/d00"));
    }

    #[test]
    fn dense_first_keeps_the_dense_top_hit_first_under_fts_noise() {
        let (fts, dense) = noisy_lists();
        let fused = fuse(&fts, &dense, Fusion::DenseFirst);
        assert_eq!(fused[0].0, "entities/d00");
    }

    #[test]
    fn dense_first_keeps_the_dense_top_ten_as_the_top_ten_under_fts_noise() {
        let (fts, dense) = noisy_lists();
        let fused = fuse(&fts, &dense, Fusion::DenseFirst);
        let mut got = top(&fused, 10);
        got.sort_unstable();
        let want: Vec<String> = (0..10).map(|i| format!("entities/d{i:02}")).collect();
        assert_eq!(got, want.iter().map(String::as_str).collect::<Vec<_>>());
    }

    #[test]
    fn dense_first_boosts_a_page_found_by_both_lists() {
        // c is dense #3 and FTS #1: it overtakes dense #2 (b), not dense #1.
        let dense = list(&["entities/a", "entities/b", "entities/c", "entities/d"]);
        let fts = list(&["entities/c"]);
        let fused = fuse(&fts, &dense, Fusion::DenseFirst);
        assert_eq!(
            top(&fused, 4),
            vec!["entities/a", "entities/c", "entities/b", "entities/d"]
        );
    }

    #[test]
    fn dense_first_lets_fts_only_pages_fill_a_short_dense_list() {
        let dense = list(&["entities/a", "entities/b"]);
        let fts = list(&["entities/x", "entities/a"]);
        let fused = fuse(&fts, &dense, Fusion::DenseFirst);
        assert_eq!(
            top(&fused, 3),
            vec!["entities/a", "entities/b", "entities/x"]
        );
    }

    #[test]
    fn weak_fts_hits_are_ignored_by_the_weak_fts_variant() {
        let dense = list(&["entities/a"]);
        let fts = vec![
            ("entities/strong".to_string(), 10.0),
            ("entities/weak".to_string(), 2.0),
        ];
        let fused = fuse(&fts, &dense, Fusion::WeakFtsIgnored);
        assert!(fused.iter().all(|(id, _)| id != "entities/weak"));
    }

    #[test]
    fn the_convex_variant_weights_the_dense_score_above_the_fts_score() {
        let dense = list(&["entities/a", "entities/b"]);
        let fts = list(&["entities/b", "entities/a"]);
        let fused = fuse(&fts, &dense, Fusion::Convex);
        assert_eq!(fused[0].0, "entities/a");
    }

    #[test]
    fn fusion_breaks_score_ties_by_id() {
        let fts = list(&["entities/b"]);
        let dense = list(&["entities/a"]);
        let fused = fuse(&fts, &dense, Fusion::Rrf);
        assert_eq!(top(&fused, 2), vec!["entities/a", "entities/b"]);
    }
}
