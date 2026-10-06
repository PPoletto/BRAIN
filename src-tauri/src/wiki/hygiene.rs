//! Index-backed hygiene rules (A3): pages nobody links to that have been
//! left alone for a long time (`orphan`), and pairs of pages that look
//! like the same thing written twice (`duplicate-candidate`).
//!
//! Both read the SQLite index (`pages`, `wiki_links`, `chunks`) instead of
//! the files: the link graph and the chunk vectors are already there. The
//! work is split in two so callers can bound the DB part:
//!
//!  - [`load_rows`] does every DB read (under the connection lock). Chunk
//!    vectors are STREAMED into per-page running sums — no blob is ever
//!    collected — so the lock is held only for one sequential scan. The
//!    MCP server runs it through its timeout/reopen wrapper (`db_op`).
//!  - [`evaluate`] is pure: it applies the rules to the loaded rows with
//!    no lock held.
//!
//! Findings are warnings only — they never block an auto-commit; the
//! watcher's pre-commit gate does not run these rules.
//!
//! Dead links (`[[id]]` without a target page) are NOT a hygiene rule:
//! the filesystem lint already reports them as the `broken-link` error.

use std::collections::{HashMap, HashSet};

use rusqlite::types::ValueRef;

use crate::db::{DbHandle, DbResult};

use super::lint::LintWarning;

/// Cosine similarity of two page vectors (mean of their chunk vectors) at
/// or above which the pair is reported as a `duplicate-candidate`.
pub const DUPLICATE_SIMILARITY_THRESHOLD: f32 = 0.92;

/// Duplicate detection compares every pair of pages of one type
/// (O(n²)). Types with more pages than this are skipped with a note
/// instead of stalling `brain_lint_report` for many seconds.
pub const DUPLICATE_MAX_PAGES_PER_TYPE: usize = 2000;

/// A page without inbound links is reported as `orphan` only once its
/// file has not changed for this many days — new pages get time to be
/// linked from somewhere.
pub const ORPHAN_MIN_AGE_DAYS: i64 = 90;

/// Value of `schema_meta.index_embedder` when every vector in the index
/// was written by the real model. Only then are similarities meaningful;
/// the hashed fallback's vectors can be spuriously similar.
const SEMANTIC_EMBEDDER: &str = "bge-m3";

const SKIPPED_KIND: &str = "duplicate-detection-skipped";

/// What [`evaluate`] found: page findings (warnings) and info notes.
#[derive(Debug, Default)]
pub struct HygieneFindings {
    pub warnings: Vec<LintWarning>,
    pub notes: Vec<LintWarning>,
}

/// Everything the hygiene rules need from the index, loaded by
/// [`load_rows`].
#[derive(Debug, Default)]
pub struct HygieneRows {
    pages: Vec<PageRow>,
    links: Vec<(String, String)>,
    /// Page id → L2-normalised mean of its chunk vectors. Only loaded
    /// when the index was embedded by the real model.
    page_vectors: HashMap<String, Vec<f32>>,
    /// `schema_meta.index_embedder` (see `pages_index`).
    index_embedder: Option<String>,
}

/// One `pages` row, as far as the hygiene rules need it.
#[derive(Debug, Clone)]
struct PageRow {
    id: String,
    page_type: String,
    path: String,
    /// Unix seconds of the file's last modification; 0 = unknown.
    mtime: i64,
}

/// [`load_rows`] + [`evaluate`] on a GUI-side handle (no timeout).
/// `model_available` (see [`crate::embedding::model_available`]) only
/// chooses the wording of the "duplicate detection skipped" note; whether
/// duplicates are checked depends on which embedder wrote the index.
/// `now_unix` is the reference time for the orphan age.
pub fn check(db: &DbHandle, model_available: bool, now_unix: i64) -> DbResult<HygieneFindings> {
    let rows = db.with(load_rows)?;
    Ok(evaluate(&rows, model_available, now_unix))
}

/// All DB reads of the hygiene rules. Chunk vectors are only read when
/// the index was embedded by the real model, and are summed per page
/// while the rows stream by.
pub fn load_rows(conn: &rusqlite::Connection) -> DbResult<HygieneRows> {
    let index_embedder = crate::db::pages_index::index_embedder(conn);
    let pages = {
        let mut stmt = conn
            .prepare("SELECT id, type, path, COALESCE(file_mtime, 0) FROM pages ORDER BY id")?;
        stmt.query_map([], |row| {
            Ok(PageRow {
                id: row.get(0)?,
                page_type: row.get(1)?,
                path: row.get(2)?,
                mtime: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    let links = {
        let mut stmt = conn.prepare("SELECT src_id, dst_id FROM wiki_links")?;
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?
    };
    let page_vectors = if index_embedder.as_deref() == Some(SEMANTIC_EMBEDDER) {
        stream_page_vectors(conn)?
    } else {
        HashMap::new()
    };
    Ok(HygieneRows {
        pages,
        links,
        page_vectors,
        index_embedder,
    })
}

/// Page id → L2-normalised mean of its chunk vectors, accumulated row by
/// row. Blobs that are empty, not a whole number of f32s, or of another
/// dimension than the page's first chunk are skipped; pages whose sum is
/// the zero vector are left out.
fn stream_page_vectors(conn: &rusqlite::Connection) -> DbResult<HashMap<String, Vec<f32>>> {
    let mut sums: HashMap<String, Vec<f32>> = HashMap::new();
    let mut stmt =
        conn.prepare("SELECT page_id, embedding FROM chunks WHERE embedding IS NOT NULL")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let ValueRef::Blob(blob) = row.get_ref(1)? else {
            continue;
        };
        if blob.is_empty() || blob.len() % 4 != 0 {
            continue;
        }
        let dim = blob.len() / 4;
        let page_id = row.get_ref(0)?.as_str().unwrap_or_default();
        if !sums.contains_key(page_id) {
            sums.insert(page_id.to_string(), vec![0.0; dim]);
        }
        let Some(sum) = sums.get_mut(page_id) else {
            continue;
        };
        if sum.len() != dim {
            continue;
        }
        for (s, bytes) in sum.iter_mut().zip(blob.chunks_exact(4)) {
            *s += f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
    }
    Ok(sums
        .into_iter()
        .filter_map(|(id, mut v)| {
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm <= f32::EPSILON {
                return None;
            }
            for x in &mut v {
                *x /= norm;
            }
            Some((id, v))
        })
        .collect())
}

/// Apply every hygiene rule to `rows`. Pure — no DB access.
pub fn evaluate(rows: &HygieneRows, model_available: bool, now_unix: i64) -> HygieneFindings {
    let mut findings = HygieneFindings {
        warnings: orphans(&rows.pages, &rows.links, now_unix),
        notes: Vec::new(),
    };
    if rows.index_embedder.as_deref() == Some(SEMANTIC_EMBEDDER) {
        duplicate_candidates(rows, &mut findings);
    } else {
        let message = if model_available {
            "duplicate detection needs the embedding model's vectors in the index — it still \
             holds vectors from the fallback embedder. Settings → \"Rebuild index\" re-embeds \
             every page with the model"
        } else {
            "duplicate detection needs the embedding model (bge-m3) — download it in Settings \
             to get duplicate-candidate findings"
        };
        findings.notes.push(LintWarning {
            path: String::new(),
            kind: SKIPPED_KIND.into(),
            message: message.into(),
        });
    }
    findings
}

/// The page part of a link target: `id#heading` → `id`.
fn strip_fragment(dst: &str) -> &str {
    dst.split('#').next().unwrap_or(dst).trim()
}

/// Pages that no OTHER page links to (a self-link does not count; a link
/// to `id#heading` counts for `id`, a link whose raw text equals an id
/// counts for that id) and whose file has not changed for
/// [`ORPHAN_MIN_AGE_DAYS`]. Pages with an unknown mtime are not reported.
fn orphans(pages: &[PageRow], links: &[(String, String)], now_unix: i64) -> Vec<LintWarning> {
    let mut linked: HashSet<&str> = HashSet::new();
    for (src, dst) in links {
        for target in [dst.as_str(), strip_fragment(dst)] {
            if target != src {
                linked.insert(target);
            }
        }
    }
    let cutoff = now_unix - ORPHAN_MIN_AGE_DAYS * 24 * 60 * 60;
    pages
        .iter()
        .filter(|p| p.mtime > 0 && p.mtime < cutoff && !linked.contains(p.id.as_str()))
        .map(|p| LintWarning {
            path: p.path.clone(),
            kind: "orphan".into(),
            message: format!(
                "no other page links to '{}', and it was last changed on {} (more than {} \
                 days ago) — link it from a related page, merge it into another page \
                 (brain_merge_pages) or delete it (brain_delete_page)",
                p.id,
                format_local_date(p.mtime),
                ORPHAN_MIN_AGE_DAYS
            ),
        })
        .collect()
}

fn format_local_date(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "an unknown date".into())
}

/// Pairs of pages of the same `type` whose page vectors have a cosine
/// similarity of at least [`DUPLICATE_SIMILARITY_THRESHOLD`], highest
/// first. Each pair is reported under BOTH pages' paths (same message,
/// naming both ids), so a page-scoped view of either page shows it. Types
/// with more than [`DUPLICATE_MAX_PAGES_PER_TYPE`] pages are skipped with
/// a note; pages without vectors are ignored.
fn duplicate_candidates(rows: &HygieneRows, findings: &mut HygieneFindings) {
    // Pages are loaded ORDER BY id, so within a group every pair is (a < b).
    let mut by_type: HashMap<&str, Vec<(&PageRow, &Vec<f32>)>> = HashMap::new();
    for page in &rows.pages {
        if let Some(v) = rows.page_vectors.get(&page.id) {
            by_type.entry(page.page_type.as_str()).or_default().push((page, v));
        }
    }
    let mut types: Vec<&str> = by_type.keys().copied().collect();
    types.sort_unstable();

    let mut pairs: Vec<(f32, &PageRow, &PageRow)> = Vec::new();
    for page_type in types {
        let group = &by_type[page_type];
        if group.len() > DUPLICATE_MAX_PAGES_PER_TYPE {
            findings.notes.push(LintWarning {
                path: String::new(),
                kind: SKIPPED_KIND.into(),
                message: format!(
                    "duplicate detection skipped for type '{page_type}': {} pages exceed the \
                     limit of {DUPLICATE_MAX_PAGES_PER_TYPE} per type",
                    group.len()
                ),
            });
            continue;
        }
        for (i, (a, va)) in group.iter().enumerate() {
            for (b, vb) in &group[i + 1..] {
                let score = crate::embedding::cosine(va, vb);
                if score >= DUPLICATE_SIMILARITY_THRESHOLD {
                    pairs.push((score, a, b));
                }
            }
        }
    }
    pairs.sort_by(|x, y| {
        y.0.total_cmp(&x.0)
            .then_with(|| x.1.id.cmp(&y.1.id))
            .then_with(|| x.2.id.cmp(&y.2.id))
    });
    for (score, a, b) in pairs {
        let message = format!(
            "'{}' and '{}' may be duplicates (similarity {score:.2}) — if they describe the \
             same thing, fold one into the other with brain_merge_pages",
            a.id, b.id
        );
        for page in [a, b] {
            findings.warnings.push(LintWarning {
                path: page.path.clone(),
                kind: "duplicate-candidate".into(),
                message: message.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::vec_to_bytes;
    use tempfile::TempDir;

    const DAY: i64 = 24 * 60 * 60;
    /// 2026-10-06T00:00:00Z — the fixed "now" of these tests.
    const NOW: i64 = 1_791_244_800;

    fn open_db() -> (TempDir, DbHandle) {
        let tmp = TempDir::new().unwrap();
        crate::vault::layout::ensure_skeleton(tmp.path()).unwrap();
        let db = DbHandle::open(tmp.path()).unwrap();
        (tmp, db)
    }

    /// An index whose vectors were written by the real model.
    fn open_semantic_db() -> (TempDir, DbHandle) {
        let (tmp, db) = open_db();
        set_index_embedder(&db, SEMANTIC_EMBEDDER);
        (tmp, db)
    }

    fn set_index_embedder(db: &DbHandle, name: &str) {
        db.with(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('index_embedder', ?1)",
                rusqlite::params![name],
            )?;
            Ok(())
        })
        .unwrap();
    }

    fn insert_page(db: &DbHandle, id: &str, page_type: &str, mtime: i64) {
        db.with(|conn| {
            conn.execute(
                "INSERT INTO pages(id, type, path, file_mtime) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, page_type, format!("02_wiki/{id}.md"), mtime],
            )?;
            Ok(())
        })
        .unwrap();
    }

    fn insert_link(db: &DbHandle, src: &str, dst: &str) {
        db.with(|conn| {
            conn.execute(
                "INSERT INTO wiki_links(src_id, dst_id) VALUES (?1, ?2)",
                rusqlite::params![src, dst],
            )?;
            Ok(())
        })
        .unwrap();
    }

    /// Unit vector of dimension 4 pointing mostly along `axis`, tilted by
    /// `tilt` into the next axis — two vectors with different tilts have
    /// a controllable cosine.
    fn unit(axis: usize, tilt: f32) -> Vec<f32> {
        let mut v = [0.0f32; 4];
        v[axis] = 1.0;
        v[(axis + 1) % 4] = tilt;
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / norm).collect()
    }

    fn insert_chunk(db: &DbHandle, page_id: &str, v: &[f32]) {
        db.with(|conn| {
            conn.execute(
                "INSERT INTO chunks(page_id, chunk_idx, text, embedding) VALUES (?1, 0, 'x', ?2)",
                rusqlite::params![page_id, vec_to_bytes(v)],
            )?;
            Ok(())
        })
        .unwrap();
    }

    fn kinds(findings: &HygieneFindings, kind: &str) -> Vec<String> {
        findings
            .warnings
            .iter()
            .filter(|w| w.kind == kind)
            .map(|w| w.message.clone())
            .collect()
    }

    // ---- orphan ---------------------------------------------------------

    #[test]
    fn a_page_without_inbound_links_unchanged_for_more_than_90_days_is_an_orphan() {
        let (_tmp, db) = open_db();
        insert_page(&db, "entities/old", "entity", NOW - 91 * DAY);
        let findings = check(&db, false, NOW).unwrap();
        assert_eq!(kinds(&findings, "orphan").len(), 1);
    }

    #[test]
    fn the_orphan_message_names_the_page_and_its_last_change_date() {
        let (_tmp, db) = open_db();
        // Noon UTC, so the local date is the same in every time zone.
        insert_page(&db, "entities/old", "entity", NOW - 100 * DAY + DAY / 2);
        let findings = check(&db, false, NOW).unwrap();
        let message = &kinds(&findings, "orphan")[0];
        assert!(
            message.contains("'entities/old'") && message.contains("2026-06-28"),
            "{message}"
        );
    }

    #[test]
    fn a_page_without_inbound_links_changed_recently_is_not_an_orphan() {
        let (_tmp, db) = open_db();
        insert_page(&db, "entities/fresh", "entity", NOW - 10 * DAY);
        let findings = check(&db, false, NOW).unwrap();
        assert!(kinds(&findings, "orphan").is_empty());
    }

    #[test]
    fn an_old_page_that_another_page_links_to_is_not_an_orphan() {
        let (_tmp, db) = open_db();
        insert_page(&db, "entities/old", "entity", NOW - 200 * DAY);
        insert_page(&db, "entities/hub", "entity", NOW);
        insert_link(&db, "entities/hub", "entities/old");
        let findings = check(&db, false, NOW).unwrap();
        assert!(kinds(&findings, "orphan").is_empty());
    }

    #[test]
    fn a_link_to_a_heading_of_a_page_counts_as_an_inbound_link() {
        let (_tmp, db) = open_db();
        insert_page(&db, "entities/old", "entity", NOW - 200 * DAY);
        insert_page(&db, "entities/hub", "entity", NOW);
        insert_link(&db, "entities/hub", "entities/old#Contract");
        let findings = check(&db, false, NOW).unwrap();
        assert!(kinds(&findings, "orphan").is_empty());
    }

    #[test]
    fn a_link_to_a_page_whose_id_contains_a_hash_counts_as_an_inbound_link() {
        let (_tmp, db) = open_db();
        insert_page(&db, "entities/c#", "entity", NOW - 200 * DAY);
        insert_page(&db, "entities/hub", "entity", NOW);
        insert_link(&db, "entities/hub", "entities/c#");
        let findings = check(&db, false, NOW).unwrap();
        assert!(kinds(&findings, "orphan").is_empty());
    }

    #[test]
    fn a_link_from_a_page_to_itself_does_not_count_as_an_inbound_link() {
        let (_tmp, db) = open_db();
        insert_page(&db, "entities/old", "entity", NOW - 200 * DAY);
        insert_link(&db, "entities/old", "entities/old");
        let findings = check(&db, false, NOW).unwrap();
        assert_eq!(kinds(&findings, "orphan").len(), 1);
    }

    #[test]
    fn a_page_with_unknown_modification_time_is_not_an_orphan() {
        let (_tmp, db) = open_db();
        insert_page(&db, "entities/unknown", "entity", 0);
        let findings = check(&db, false, NOW).unwrap();
        assert!(kinds(&findings, "orphan").is_empty());
    }

    // ---- duplicate-candidate ---------------------------------------------

    #[test]
    fn each_page_of_a_near_identical_same_type_pair_gets_the_duplicate_finding() {
        let (_tmp, db) = open_semantic_db();
        insert_page(&db, "entities/mueller-gmbh", "entity", NOW);
        insert_page(&db, "entities/muller-gmbh", "entity", NOW);
        insert_chunk(&db, "entities/mueller-gmbh", &unit(0, 0.0));
        insert_chunk(&db, "entities/muller-gmbh", &unit(0, 0.1));
        let findings = check(&db, true, NOW).unwrap();
        let paths: Vec<&str> = findings
            .warnings
            .iter()
            .filter(|w| w.kind == "duplicate-candidate")
            .map(|w| w.path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec!["02_wiki/entities/mueller-gmbh.md", "02_wiki/entities/muller-gmbh.md"]
        );
    }

    #[test]
    fn the_duplicate_message_names_both_ids_and_the_score_with_two_decimals() {
        let (_tmp, db) = open_semantic_db();
        insert_page(&db, "entities/a", "entity", NOW);
        insert_page(&db, "entities/b", "entity", NOW);
        insert_chunk(&db, "entities/a", &unit(0, 0.0));
        // cos = 1 / sqrt(1 + 0.3²) = 0.9578…
        insert_chunk(&db, "entities/b", &unit(0, 0.3));
        let findings = check(&db, true, NOW).unwrap();
        let message = &kinds(&findings, "duplicate-candidate")[0];
        assert!(
            message.contains("'entities/a' and 'entities/b'") && message.contains("0.96"),
            "{message}"
        );
    }

    #[test]
    fn pages_below_the_similarity_threshold_are_not_duplicate_candidates() {
        let (_tmp, db) = open_semantic_db();
        insert_page(&db, "entities/a", "entity", NOW);
        insert_page(&db, "entities/b", "entity", NOW);
        insert_chunk(&db, "entities/a", &unit(0, 0.0));
        // cos = 1 / sqrt(1 + 0.6²) = 0.857…
        insert_chunk(&db, "entities/b", &unit(0, 0.6));
        let findings = check(&db, true, NOW).unwrap();
        assert!(kinds(&findings, "duplicate-candidate").is_empty());
    }

    #[test]
    fn near_identical_pages_of_different_types_are_not_duplicate_candidates() {
        let (_tmp, db) = open_semantic_db();
        insert_page(&db, "entities/nis2", "entity", NOW);
        insert_page(&db, "concepts/nis2", "concept", NOW);
        insert_chunk(&db, "entities/nis2", &unit(0, 0.0));
        insert_chunk(&db, "concepts/nis2", &unit(0, 0.0));
        let findings = check(&db, true, NOW).unwrap();
        assert!(kinds(&findings, "duplicate-candidate").is_empty());
    }

    #[test]
    fn the_page_vector_is_the_mean_of_its_chunk_vectors() {
        let (_tmp, db) = open_semantic_db();
        insert_page(&db, "entities/a", "entity", NOW);
        insert_page(&db, "entities/b", "entity", NOW);
        // a = mean(axis 0, axis 1) points diagonally; b's single chunk
        // points the same way, so only the mean makes them identical.
        insert_chunk(&db, "entities/a", &unit(0, 0.0));
        insert_chunk(&db, "entities/a", &unit(1, 0.0));
        insert_chunk(&db, "entities/b", &unit(0, 1.0));
        let findings = check(&db, true, NOW).unwrap();
        assert_eq!(kinds(&findings, "duplicate-candidate").len(), 2);
    }

    #[test]
    fn a_page_without_chunk_vectors_is_skipped_by_duplicate_detection() {
        let (_tmp, db) = open_semantic_db();
        insert_page(&db, "entities/a", "entity", NOW);
        insert_page(&db, "entities/empty", "entity", NOW);
        insert_chunk(&db, "entities/a", &unit(0, 0.0));
        let findings = check(&db, true, NOW).unwrap();
        assert!(kinds(&findings, "duplicate-candidate").is_empty());
    }

    #[test]
    fn three_mutually_similar_pages_give_three_pairs_reported_under_both_pages() {
        let (_tmp, db) = open_semantic_db();
        for id in ["entities/a", "entities/b", "entities/c"] {
            insert_page(&db, id, "entity", NOW);
            insert_chunk(&db, id, &unit(0, 0.0));
        }
        let findings = check(&db, true, NOW).unwrap();
        assert_eq!(kinds(&findings, "duplicate-candidate").len(), 6);
    }

    #[test]
    fn a_type_with_more_pages_than_the_cap_is_skipped_with_a_note_naming_type_and_count() {
        let mut rows = HygieneRows {
            index_embedder: Some(SEMANTIC_EMBEDDER.into()),
            ..HygieneRows::default()
        };
        for i in 0..=DUPLICATE_MAX_PAGES_PER_TYPE {
            let id = format!("entities/p{i:04}");
            rows.page_vectors.insert(id.clone(), vec![1.0, 0.0]);
            rows.pages.push(PageRow {
                id,
                page_type: "entity".into(),
                path: String::new(),
                mtime: NOW,
            });
        }
        let findings = evaluate(&rows, true, NOW);
        let notes: Vec<&str> = findings.notes.iter().map(|n| n.message.as_str()).collect();
        assert_eq!(
            notes,
            vec!["duplicate detection skipped for type 'entity': 2001 pages exceed the limit of 2000 per type"]
        );
    }

    #[test]
    fn an_index_embedded_by_the_fallback_embedder_reports_no_duplicates() {
        let (_tmp, db) = open_db();
        set_index_embedder(&db, "hashed-fh-1024");
        insert_page(&db, "entities/a", "entity", NOW);
        insert_page(&db, "entities/b", "entity", NOW);
        insert_chunk(&db, "entities/a", &unit(0, 0.0));
        insert_chunk(&db, "entities/b", &unit(0, 0.0));
        let findings = check(&db, true, NOW).unwrap();
        assert!(kinds(&findings, "duplicate-candidate").is_empty());
    }

    #[test]
    fn with_the_model_present_but_a_fallback_index_the_note_points_at_rebuild_index() {
        let (_tmp, db) = open_db();
        set_index_embedder(&db, "hashed-fh-1024");
        let findings = check(&db, true, NOW).unwrap();
        assert!(
            findings.notes.len() == 1 && findings.notes[0].message.contains("Rebuild index"),
            "{:?}",
            findings.notes
        );
    }

    #[test]
    fn without_the_embedding_model_one_note_says_duplicate_detection_needs_it() {
        let (_tmp, db) = open_db();
        let findings = check(&db, false, NOW).unwrap();
        let notes: Vec<&str> = findings.notes.iter().map(|n| n.message.as_str()).collect();
        assert!(
            notes.len() == 1 && notes[0].starts_with("duplicate detection needs the embedding model"),
            "{notes:?}"
        );
    }

    #[test]
    fn with_a_model_embedded_index_no_note_is_emitted() {
        let (_tmp, db) = open_semantic_db();
        let findings = check(&db, true, NOW).unwrap();
        assert!(findings.notes.is_empty());
    }

    // ---- lint_with_index ---------------------------------------------------

    #[test]
    fn lint_with_index_adds_orphan_warnings_to_the_filesystem_lint() {
        let (tmp, db) = open_db();
        insert_page(&db, "entities/old", "entity", NOW - 365 * DAY);
        let report = super::super::lint::lint_with_index(tmp.path(), Some(&db)).unwrap();
        assert!(report.warnings.iter().any(|w| w.kind == "orphan"));
    }

    #[test]
    fn hygiene_findings_never_block_a_commit() {
        let (tmp, db) = open_db();
        insert_page(&db, "entities/old", "entity", NOW - 365 * DAY);
        let report = super::super::lint::lint_with_index(tmp.path(), Some(&db)).unwrap();
        assert!(report.is_clean());
    }

    #[test]
    fn the_filesystem_lint_does_not_run_the_hygiene_rules() {
        let (tmp, db) = open_db();
        insert_page(&db, "entities/old", "entity", NOW - 365 * DAY);
        let report = super::super::lint::lint(tmp.path()).unwrap();
        assert!(report.warnings.iter().all(|w| w.kind != "orphan"));
    }

    #[test]
    fn an_unreadable_index_becomes_a_hygiene_skipped_note() {
        let tmp = TempDir::new().unwrap();
        crate::vault::layout::ensure_skeleton(tmp.path()).unwrap();
        let mut report = super::super::lint::lint(tmp.path()).unwrap();
        super::super::lint::add_hygiene(&mut report, tmp.path(), Err("index timed out".into()));
        assert!(
            report.notes.len() == 1
                && report.notes[0].kind == "hygiene-skipped"
                && report.notes[0].message.contains("index timed out"),
            "{:?}",
            report.notes
        );
    }
}
