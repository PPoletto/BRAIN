//! Runs a compiled query against the SQLite index.

use rusqlite::params_from_iter;
use serde::Serialize;

use crate::db::DbHandle;

use super::parser::parse;
use super::sql::compile_query;
use super::QueryError;

#[derive(Debug, Clone, Serialize)]
pub struct QueryHit {
    pub id: String,
    pub r#type: String,
    pub path: String,
    pub title: String,
    pub updated_at: Option<String>,
    /// Salience counters (H3): reads via brain_get_page(s)/get_context
    /// and appearances in a brain_search top 10. 0 when never counted.
    pub reads: i64,
    pub search_hits: i64,
    /// Unix seconds of the last counted read.
    pub last_read_at: Option<i64>,
    /// Validity (Slice C), omitted when the page does not set them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("query: {0}")]
    Query(#[from] QueryError),

    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("no vault is currently mounted")]
    NoVault,
}

pub fn run(db: &DbHandle, query: &str) -> Result<Vec<QueryHit>, ExecError> {
    let expr = parse(query)?;
    let compiled = compile_query(expr, &crate::wiki::lint::today_local())?;
    let hits = db
        .with(|conn| run_compiled_on_conn(conn, &compiled).map_err(crate::db::DbError::from))
        .map_err(|e| match e {
            crate::db::DbError::Rusqlite(r) => ExecError::Db(r),
            crate::db::DbError::Io(e) => {
                ExecError::Db(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            }
        })?;
    Ok(hits)
}

/// Connection-only query runner. Parses + compiles the query string,
/// then executes against a bare `&Connection`. Split out so the MCP
/// server can run it through its timeout/reopen wrapper (`db_op`),
/// which hands closures a `&Connection` rather than a `DbHandle`.
/// Parse errors surface as `ExecError::Query`; DB errors as
/// `rusqlite::Error` (the caller maps them to `DbError` for `db_op`).
pub fn run_on_conn(conn: &rusqlite::Connection, query: &str) -> Result<Vec<QueryHit>, ExecError> {
    run_on_conn_as_of(conn, query, &crate::wiki::lint::today_local())
}

/// [`run_on_conn`] with an explicit reference day (`YYYY-MM-DD`) for the
/// `valid:` filter — the seam the tests use.
pub fn run_on_conn_as_of(
    conn: &rusqlite::Connection,
    query: &str,
    today: &str,
) -> Result<Vec<QueryHit>, ExecError> {
    let expr = parse(query)?;
    let compiled = compile_query(expr, today)?;
    run_compiled_on_conn(conn, &compiled).map_err(ExecError::Db)
}

/// Shared execution core for both `run` and `run_on_conn`.
fn run_compiled_on_conn(
    conn: &rusqlite::Connection,
    compiled: &super::sql::CompiledQuery,
) -> rusqlite::Result<Vec<QueryHit>> {
    let mut stmt = conn.prepare(&compiled.sql)?;
    let rows = stmt
        .query_map(params_from_iter(compiled.params.iter()), |row| {
            Ok(QueryHit {
                id: row.get(0)?,
                r#type: row.get(1)?,
                path: row.get::<_, String>(2)?,
                title: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                updated_at: row.get(6)?,
                reads: row.get(7)?,
                search_hits: row.get(8)?,
                last_read_at: row.get(9)?,
                valid_from: row.get(10)?,
                valid_to: row.get(11)?,
                superseded_by: row.get(12)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::pages_index;
    use crate::vault::layout::{ensure_skeleton, wiki_dir};
    use tempfile::TempDir;

    fn write_page(vault: &std::path::Path, sub: &str, slug: &str, tags: &[&str], updated: &str) {
        let dir = wiki_dir(vault).join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        let tags_yaml = format!(
            "[{}]",
            tags.iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        let ty = match sub {
            "entities" => "entity",
            "concepts" => "concept",
            "sources" => "source",
            "topics" => "topic",
            _ => "entity",
        };
        std::fs::write(
            dir.join(format!("{slug}.md")),
            format!(
                "---\nid: {sub}/{slug}\ntype: {ty}\ntitle: {slug}\ntags: {tags_yaml}\ncreated: 2026-04-29\nupdated: {updated}\n---\n\nbody\n"
            ),
        )
        .unwrap();
    }

    fn fresh_db_with_pages(setup: impl FnOnce(&std::path::Path)) -> (TempDir, DbHandle) {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        setup(tmp.path());
        let db = DbHandle::open(tmp.path()).unwrap();
        pages_index::rebuild(&db, tmp.path()).unwrap();
        (tmp, db)
    }

    #[test]
    fn query_filters_by_type() {
        let (_tmp, db) = fresh_db_with_pages(|root| {
            write_page(root, "entities", "alice", &[], "2026-04-29");
            write_page(root, "concepts", "nlspec", &[], "2026-04-29");
        });
        let hits = run(&db, "type:concept").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "concepts/nlspec");
    }

    #[test]
    fn query_filters_by_tag() {
        let (_tmp, db) = fresh_db_with_pages(|root| {
            write_page(root, "entities", "alice", &["nis2"], "2026-04-29");
            write_page(root, "entities", "bob", &["other"], "2026-04-29");
        });
        let hits = run(&db, "tag:nis2").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "entities/alice");
    }

    #[test]
    fn query_filters_by_updated_greater_than() {
        let (_tmp, db) = fresh_db_with_pages(|root| {
            write_page(root, "entities", "old", &[], "2025-01-01");
            write_page(root, "entities", "new", &[], "2026-05-15");
        });
        let hits = run(&db, "updated:>2026-01-01").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "entities/new");
    }

    #[test]
    fn query_combines_and_or_with_correct_precedence() {
        let (_tmp, db) = fresh_db_with_pages(|root| {
            write_page(root, "entities", "alice", &["nis2"], "2026-04-29");
            write_page(root, "entities", "bob", &["dora"], "2026-04-29");
            write_page(root, "concepts", "nlspec", &["nis2"], "2026-04-29");
        });
        // type:entity AND (tag:nis2 OR tag:dora)
        let hits = run(&db, "type:entity AND (tag:nis2 OR tag:dora)").unwrap();
        let ids: std::collections::HashSet<String> = hits.iter().map(|h| h.id.clone()).collect();
        assert!(ids.contains("entities/alice"));
        assert!(ids.contains("entities/bob"));
        assert!(!ids.contains("concepts/nlspec"));
    }

    #[test]
    fn query_returns_empty_for_no_matches() {
        let (_tmp, db) = fresh_db_with_pages(|root| {
            write_page(root, "entities", "alice", &[], "2026-04-29");
        });
        let hits = run(&db, "tag:nonexistent").unwrap();
        assert!(hits.is_empty());
    }

    /// A page with extra frontmatter lines (validity fields).
    fn write_page_with(vault: &std::path::Path, sub: &str, slug: &str, extra: &str) {
        let dir = wiki_dir(vault).join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{slug}.md")),
            format!(
                "---\nid: {sub}/{slug}\ntype: entity\ntitle: {slug}\n{extra}updated: 2026-04-29\n---\n\nbody\n"
            ),
        )
        .unwrap();
    }

    /// The roadmap's Slice C example: A expired and superseded by B.
    fn validity_vault() -> (TempDir, DbHandle) {
        fresh_db_with_pages(|root| {
            write_page_with(root, "entities", "a", "valid_to: 2025-12-31\nsuperseded_by: entities/b\n");
            write_page_with(root, "entities", "b", "");
            write_page_with(root, "entities", "c", "valid_to: 2025-01-31\n");
        })
    }

    fn ids_as_of(db: &DbHandle, query: &str) -> Vec<String> {
        let mut ids: Vec<String> = db
            .with(|conn| {
                Ok(run_on_conn_as_of(conn, query, "2026-10-06")
                    .map_err(|e| std::io::Error::other(e.to_string()))?)
            })
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn a_query_without_valid_leaves_out_expired_and_superseded_pages() {
        let (_tmp, db) = validity_vault();
        assert_eq!(ids_as_of(&db, "type:entity"), vec!["entities/b"]);
    }

    #[test]
    fn valid_all_returns_expired_and_superseded_pages_too() {
        let (_tmp, db) = validity_vault();
        assert_eq!(
            ids_as_of(&db, "type:entity AND valid:all"),
            vec!["entities/a", "entities/b", "entities/c"]
        );
    }

    #[test]
    fn valid_expired_returns_only_expired_and_superseded_pages() {
        let (_tmp, db) = validity_vault();
        assert_eq!(ids_as_of(&db, "valid:expired"), vec!["entities/a", "entities/c"]);
    }

    #[test]
    fn a_page_whose_valid_to_is_today_is_still_valid_now() {
        let (_tmp, db) = fresh_db_with_pages(|root| {
            write_page_with(root, "entities", "today", "valid_to: 2026-10-06\n");
        });
        assert_eq!(ids_as_of(&db, "type:entity"), vec!["entities/today"]);
    }

    #[test]
    fn sort_salience_puts_the_most_read_page_first() {
        let (_tmp, db) = fresh_db_with_pages(|root| {
            write_page(root, "entities", "newer", &[], "2026-05-01");
            write_page(root, "entities", "older", &[], "2026-01-01");
        });
        db.with(|conn| {
            let ids = vec!["entities/older".to_string()];
            for _ in 0..3 {
                pages_index::record_reads(conn, &ids, 1_000)?;
            }
            Ok(())
        })
        .unwrap();
        let first = db
            .with(|conn| {
                Ok(run_on_conn_as_of(conn, "type:entity AND sort:salience", "2026-10-06")
                    .map_err(|e| std::io::Error::other(e.to_string()))?)
            })
            .unwrap()
            .remove(0);
        assert_eq!((first.id.as_str(), first.reads), ("entities/older", 3));
    }

    #[test]
    fn without_sort_the_newest_updated_page_comes_first() {
        let (_tmp, db) = fresh_db_with_pages(|root| {
            write_page(root, "entities", "newer", &[], "2026-05-01");
            write_page(root, "entities", "older", &[], "2026-01-01");
        });
        db.with(|conn| pages_index::record_reads(conn, &["entities/older".to_string()], 1_000))
            .unwrap();
        let hits = run(&db, "type:entity").unwrap();
        assert_eq!(hits[0].id, "entities/newer");
    }

    #[test]
    fn query_propagates_parse_errors() {
        let (_tmp, db) = fresh_db_with_pages(|_| {});
        let err = run(&db, "foobar:x").unwrap_err();
        assert!(matches!(err, ExecError::Query(QueryError::UnknownField(_))));
    }

    #[test]
    fn a_page_whose_valid_from_lies_in_the_future_is_not_valid_now() {
        let (_tmp, db) = fresh_db_with_pages(|root| {
            write_page_with(root, "entities", "future", "valid_from: 2027-01-01\n");
            write_page_with(root, "entities", "current", "");
        });
        assert_eq!(ids_as_of(&db, "type:entity"), vec!["entities/current"]);
    }

    #[test]
    fn an_invalid_valid_to_counts_as_absent() {
        let (_tmp, db) = fresh_db_with_pages(|root| {
            write_page_with(root, "entities", "odd", "valid_to: someday\n");
        });
        assert_eq!(ids_as_of(&db, "type:entity"), vec!["entities/odd"]);
    }
}
