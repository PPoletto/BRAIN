//! S08 — "Ähnliche Seiten": the pages whose content is closest to an open
//! page, by cosine similarity of the stored page vectors (`page_vectors`,
//! the normalised mean of a page's chunk vectors — the same vectors the
//! duplicate rule uses). Across all types; the page itself and the pages
//! it lists in `distinct_from` are left out.
//!
//! [`rank_similar`] is the pure ranking; [`similar_pages`] streams the
//! vectors from the index (one row at a time, no blob is collected) and
//! adds title and type of the winners.

use std::collections::HashSet;

use rusqlite::types::ValueRef;
use serde::Serialize;

use crate::db::DbResult;
use crate::embedding::MeanVector;

/// Default number of similar pages shown.
pub const DEFAULT_LIMIT: usize = 8;

/// Upper bound for a requested limit.
pub const MAX_LIMIT: usize = 50;

/// One similar page.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SimilarPage {
    pub id: String,
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub page_type: String,
    /// Cosine similarity, -1 … 1 (1 = same direction).
    pub score: f32,
}

/// The answer of the `similar_pages` command.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SimilarPages {
    /// False when there is no index yet (nothing to compare with).
    pub index_available: bool,
    /// True when the index vectors come from the embedding model
    /// (bge-m3); otherwise they are the hashed fallback's and the
    /// similarity is word overlap rather than meaning.
    pub semantic: bool,
    pub pages: Vec<SimilarPage>,
}

impl SimilarPages {
    pub fn no_index() -> Self {
        Self {
            index_available: false,
            semantic: false,
            pages: Vec::new(),
        }
    }
}

/// Keeps the best `limit` `(id, score)` pairs by cosine with `target`,
/// skipping ids in `exclude`; highest score first, ties by id. Pure.
pub fn rank_similar<I>(
    target: &[f32],
    candidates: I,
    exclude: &HashSet<String>,
    limit: usize,
) -> Vec<(String, f32)>
where
    I: IntoIterator<Item = (String, Vec<f32>)>,
{
    let mut best: Vec<(String, f32)> = Vec::new();
    for (id, vector) in candidates {
        if exclude.contains(&id) || vector.len() != target.len() {
            continue;
        }
        best.push((id, crate::embedding::cosine(target, &vector)));
        // Keep the working set small: trim once it doubles the limit.
        if best.len() >= limit.saturating_mul(2).max(16) {
            sort_ranked(&mut best);
            best.truncate(limit);
        }
    }
    sort_ranked(&mut best);
    best.truncate(limit);
    best
}

fn sort_ranked(v: &mut [(String, f32)]) {
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
}

/// The normalised vector of one blob (`None` for an unusable blob).
fn normalised(blob: &[u8]) -> Option<Vec<f32>> {
    let mut mean = MeanVector::default();
    mean.add_blob(blob);
    mean.finish()
}

/// The page vector of `id`: its `page_vectors` row, else the mean of its
/// chunk vectors (an index from before `page_vectors` was filled).
fn target_vector(conn: &rusqlite::Connection, id: &str) -> DbResult<Option<Vec<f32>>> {
    let stored: Option<Vec<u8>> = conn
        .query_row(
            "SELECT embedding FROM page_vectors WHERE page_id = ?1",
            [id],
            |row| row.get(0),
        )
        .ok();
    if let Some(v) = stored.as_deref().and_then(normalised) {
        return Ok(Some(v));
    }
    let mut mean = MeanVector::default();
    let mut stmt =
        conn.prepare("SELECT embedding FROM chunks WHERE page_id = ?1 AND embedding IS NOT NULL")?;
    let mut rows = stmt.query([id])?;
    while let Some(row) = rows.next()? {
        if let ValueRef::Blob(blob) = row.get_ref(0)? {
            mean.add_blob(blob);
        }
    }
    Ok(mean.finish())
}

/// `distinct_from` ids of `id`'s indexed frontmatter.
fn distinct_from(conn: &rusqlite::Connection, id: &str) -> Vec<String> {
    let frontmatter: Option<String> = conn
        .query_row("SELECT frontmatter FROM pages WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .ok()
        .flatten();
    frontmatter
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|fm| {
            fm.get("distinct_from")
                .and_then(|v| v.as_array())
                .map(|ids| {
                    ids.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
        })
        .unwrap_or_default()
}

/// The pages most similar to `id` (at most `limit`, see the module
/// docs). `index_available` is false when the index has no pages; an
/// unindexed page or one without vectors gives an empty list.
pub fn similar_pages(
    conn: &rusqlite::Connection,
    id: &str,
    limit: usize,
) -> DbResult<SimilarPages> {
    let indexed: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM pages)", [], |r| r.get(0))?;
    if !indexed {
        return Ok(SimilarPages::no_index());
    }
    let semantic = crate::db::pages_index::index_embedder(conn).as_deref() == Some("bge-m3");
    let limit = limit.clamp(1, MAX_LIMIT);
    let Some(target) = target_vector(conn, id)? else {
        return Ok(SimilarPages {
            index_available: true,
            semantic,
            pages: Vec::new(),
        });
    };
    let mut exclude: HashSet<String> = distinct_from(conn, id).into_iter().collect();
    exclude.insert(id.to_string());

    let ranked = {
        let mut stmt = conn.prepare(
            "SELECT pv.page_id, pv.embedding FROM page_vectors pv \
             JOIN pages p ON p.id = pv.page_id",
        )?;
        let mut rows = stmt.query([])?;
        let mut candidates: Vec<(String, Vec<f32>)> = Vec::new();
        let mut ranked: Vec<(String, f32)> = Vec::new();
        // Stream in slices so at most a few hundred vectors are in memory.
        while let Some(row) = rows.next()? {
            let ValueRef::Blob(blob) = row.get_ref(1)? else {
                continue;
            };
            if let Some(v) = normalised(blob) {
                candidates.push((row.get(0)?, v));
            }
            if candidates.len() >= 256 {
                ranked.extend(rank_similar(&target, candidates.drain(..), &exclude, limit));
                sort_ranked(&mut ranked);
                ranked.truncate(limit);
            }
        }
        ranked.extend(rank_similar(&target, candidates, &exclude, limit));
        sort_ranked(&mut ranked);
        ranked.truncate(limit);
        ranked
    };

    let mut pages = Vec::with_capacity(ranked.len());
    let mut stmt = conn.prepare("SELECT title, type FROM pages WHERE id = ?1")?;
    for (page_id, score) in ranked {
        let (title, page_type): (Option<String>, String) =
            stmt.query_row([&page_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        pages.push(SimilarPage {
            id: page_id,
            title,
            page_type,
            score,
        });
    }
    Ok(SimilarPages {
        index_available: true,
        semantic,
        pages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbHandle;
    use crate::embedding::vec_to_bytes;
    use tempfile::TempDir;

    fn unit(v: &[f32]) -> Vec<f32> {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    }

    fn fixture() -> Vec<(String, Vec<f32>)> {
        vec![
            ("entities/a".to_string(), unit(&[1.0, 0.0, 0.0])),
            ("entities/b".to_string(), unit(&[0.9, 0.1, 0.0])),
            ("concepts/c".to_string(), unit(&[0.5, 0.5, 0.0])),
            ("topics/d".to_string(), unit(&[0.0, 0.0, 1.0])),
        ]
    }

    fn ids(ranked: &[(String, f32)]) -> Vec<&str> {
        ranked.iter().map(|(id, _)| id.as_str()).collect()
    }

    #[test]
    fn pages_are_ranked_by_cosine_similarity_highest_first() {
        let ranked = rank_similar(&unit(&[1.0, 0.0, 0.0]), fixture(), &HashSet::new(), 8);
        assert_eq!(
            ids(&ranked),
            vec!["entities/a", "entities/b", "concepts/c", "topics/d"]
        );
    }

    #[test]
    fn excluded_pages_are_left_out_of_the_ranking() {
        let exclude: HashSet<String> = ["entities/a".to_string()].into();
        let ranked = rank_similar(&unit(&[1.0, 0.0, 0.0]), fixture(), &exclude, 8);
        assert_eq!(ids(&ranked), vec!["entities/b", "concepts/c", "topics/d"]);
    }

    #[test]
    fn the_ranking_keeps_at_most_the_limit() {
        let ranked = rank_similar(&unit(&[1.0, 0.0, 0.0]), fixture(), &HashSet::new(), 2);
        assert_eq!(ids(&ranked), vec!["entities/a", "entities/b"]);
    }

    #[test]
    fn the_score_is_the_cosine_of_the_two_vectors() {
        let ranked = rank_similar(&unit(&[1.0, 0.0, 0.0]), fixture(), &HashSet::new(), 8);
        let c = ranked.iter().find(|(id, _)| id == "concepts/c").unwrap().1;
        assert!((c - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5, "{c}");
    }

    #[test]
    fn equal_scores_are_ordered_by_id() {
        let candidates = vec![
            ("entities/z".to_string(), unit(&[1.0, 0.0])),
            ("entities/m".to_string(), unit(&[1.0, 0.0])),
        ];
        let ranked = rank_similar(&unit(&[1.0, 0.0]), candidates, &HashSet::new(), 8);
        assert_eq!(ids(&ranked), vec!["entities/m", "entities/z"]);
    }

    #[test]
    fn vectors_of_another_dimension_are_skipped() {
        let candidates = vec![("entities/x".to_string(), unit(&[1.0, 0.0, 0.0, 0.0]))];
        let ranked = rank_similar(&unit(&[1.0, 0.0, 0.0]), candidates, &HashSet::new(), 8);
        assert!(ranked.is_empty());
    }

    #[test]
    fn a_long_candidate_stream_keeps_the_true_top_pages() {
        let mut candidates: Vec<(String, Vec<f32>)> = (0..100)
            .map(|i| {
                (
                    format!("entities/p{i:03}"),
                    unit(&[0.0, 1.0, i as f32 / 100.0]),
                )
            })
            .collect();
        candidates.push(("entities/best".to_string(), unit(&[1.0, 0.0, 0.0])));
        let ranked = rank_similar(&unit(&[1.0, 0.0, 0.0]), candidates, &HashSet::new(), 3);
        assert_eq!(ranked[0].0, "entities/best");
    }

    // ---- against an index -------------------------------------------------

    fn open_db() -> (TempDir, DbHandle) {
        let tmp = TempDir::new().unwrap();
        crate::vault::layout::ensure_skeleton(tmp.path()).unwrap();
        let db = DbHandle::open(tmp.path()).unwrap();
        (tmp, db)
    }

    fn page(db: &DbHandle, id: &str, page_type: &str, v: &[f32], frontmatter: Option<&str>) {
        db.with(|conn| {
            conn.execute(
                "INSERT INTO pages(id, type, path, title, frontmatter) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    id,
                    page_type,
                    format!("02_wiki/{id}.md"),
                    format!("T {id}"),
                    frontmatter
                ],
            )?;
            conn.execute(
                "INSERT INTO page_vectors(page_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![id, vec_to_bytes(&unit(v))],
            )?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn an_empty_index_is_reported_as_unavailable() {
        let (_tmp, db) = open_db();
        let result = db.with(|c| similar_pages(c, "entities/a", 8)).unwrap();
        assert!(!result.index_available);
    }

    #[test]
    fn similar_pages_span_types_and_leave_out_the_page_itself() {
        let (_tmp, db) = open_db();
        page(&db, "entities/a", "entity", &[1.0, 0.0], None);
        page(&db, "concepts/b", "concept", &[0.9, 0.1], None);
        page(&db, "topics/c", "topic", &[0.0, 1.0], None);
        let result = db.with(|c| similar_pages(c, "entities/a", 8)).unwrap();
        let got: Vec<(String, String)> = result
            .pages
            .into_iter()
            .map(|p| (p.id, p.page_type))
            .collect();
        assert_eq!(
            got,
            vec![
                ("concepts/b".to_string(), "concept".to_string()),
                ("topics/c".to_string(), "topic".to_string())
            ]
        );
    }

    #[test]
    fn pages_listed_in_distinct_from_are_not_similar_pages() {
        let (_tmp, db) = open_db();
        page(
            &db,
            "entities/a",
            "entity",
            &[1.0, 0.0],
            Some(r#"{"distinct_from":["entities/b"]}"#),
        );
        page(&db, "entities/b", "entity", &[1.0, 0.0], None);
        let result = db.with(|c| similar_pages(c, "entities/a", 8)).unwrap();
        assert!(result.pages.is_empty());
    }

    #[test]
    fn a_page_without_a_vector_has_no_similar_pages() {
        let (_tmp, db) = open_db();
        page(&db, "entities/a", "entity", &[1.0, 0.0], None);
        let result = db.with(|c| similar_pages(c, "entities/new", 8)).unwrap();
        assert!(result.index_available && result.pages.is_empty());
    }
}
