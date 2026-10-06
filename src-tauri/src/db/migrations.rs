//! Schema migrations applied on every `DbHandle::open`. Keeps the schema
//! version in `schema_meta` so we can tell what state the on-disk DB is in
//! without re-running CREATE TABLE statements that already ran.

use rusqlite::Connection;

use super::DbResult;

const CURRENT_VERSION: i64 = 7;

const MIGRATION_V1: &str = r#"
CREATE TABLE IF NOT EXISTS schema_meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);

CREATE TABLE IF NOT EXISTS pages (
    id          TEXT PRIMARY KEY,
    type        TEXT NOT NULL,
    path        TEXT NOT NULL,
    title       TEXT,
    frontmatter TEXT,
    body        TEXT,
    updated_at  TEXT,
    file_mtime  INTEGER,
    file_hash   TEXT
);

CREATE INDEX IF NOT EXISTS idx_pages_type ON pages(type);
CREATE INDEX IF NOT EXISTS idx_pages_updated ON pages(updated_at);

CREATE TABLE IF NOT EXISTS wiki_links (
    src_id  TEXT NOT NULL,
    dst_id  TEXT NOT NULL,
    broken  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (src_id, dst_id)
);

CREATE INDEX IF NOT EXISTS idx_wiki_links_dst ON wiki_links(dst_id);

CREATE TABLE IF NOT EXISTS page_tags (
    page_id TEXT NOT NULL,
    tag     TEXT NOT NULL,
    PRIMARY KEY (page_id, tag)
);

CREATE INDEX IF NOT EXISTS idx_page_tags_tag ON page_tags(tag);

CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
    id UNINDEXED,
    title,
    body,
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TABLE IF NOT EXISTS chunks (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    page_id   TEXT NOT NULL,
    chunk_idx INTEGER NOT NULL,
    text      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_chunks_page ON chunks(page_id);

CREATE TABLE IF NOT EXISTS events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    ts      TEXT NOT NULL,
    kind    TEXT NOT NULL,
    payload TEXT
);
"#;

pub fn apply(conn: &Connection) -> DbResult<()> {
    let current = read_version(conn).unwrap_or(0);
    if current >= CURRENT_VERSION {
        return Ok(());
    }
    if current < 1 {
        conn.execute_batch(MIGRATION_V1)?;
    }
    if current < 2 {
        // Idempotent column add: SQLite has no `ADD COLUMN IF NOT EXISTS`,
        // so we probe and only run the ALTER once.
        if !chunks_has_embedding_column(conn)? {
            conn.execute_batch("ALTER TABLE chunks ADD COLUMN embedding BLOB;")?;
        }
    }
    if current < 3 {
        // chunk_vectors is the sqlite-vec virtual table that backs KNN
        // sub-queries. We try to create it; if the extension isn't loaded
        // (e.g. a test with the old DbHandle code), the ERROR is caught
        // and the migration version still bumps so we don't retry on every
        // open.
        let _ = conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS chunk_vectors USING vec0(\
                embedding float[1024]\
             );",
        );
    }
    if current < 4 {
        // Persistent graph node coordinates so re-opening the viewer
        // skips the fcose layout step entirely — the graph appears
        // instantly with the user's last-arranged layout. The table is
        // intentionally separate from `pages` so that a vault export /
        // graph layout reset doesn't have to touch page rows. NULL
        // page_id is impossible because of the PRIMARY KEY; deleting a
        // page leaves a stale row that the load function filters out
        // by joining against `pages`.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS node_positions (\
                page_id    TEXT PRIMARY KEY,\
                x          REAL NOT NULL,\
                y          REAL NOT NULL,\
                updated_at TEXT NOT NULL\
             );",
        )?;
    }
    if current < 5 {
        // One IMMEDIATE transaction: a second process (GUI + MCP server
        // open the same file) waits instead of racing the column probes,
        // and a failure leaves the version at 4 with nothing half-applied.
        let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        // A2 aliases, C validity/provenance, H3 salience.
        //  - `page_aliases` / `page_sources` and the three `pages`
        //    columns are derived from the frontmatter by `pages_index`
        //    (its metadata version makes the next rebuild fill them for
        //    unchanged pages without re-embedding).
        //  - `page_access` is NOT derived data: local read/search counts.
        //    A rebuild never wipes it; it lives in 03_db, which is never
        //    mirrored or synced.
        for column in ["valid_from", "valid_to", "superseded_by"] {
            if !table_has_column(conn, "pages", column)? {
                conn.execute_batch(&format!("ALTER TABLE pages ADD COLUMN {column} TEXT;"))?;
            }
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS page_aliases (\
                page_id TEXT NOT NULL,\
                alias   TEXT NOT NULL,\
                PRIMARY KEY (page_id, alias)\
             );\
             CREATE TABLE IF NOT EXISTS page_sources (\
                page_id   TEXT NOT NULL,\
                source_id TEXT NOT NULL,\
                PRIMARY KEY (page_id, source_id)\
             );\
             CREATE INDEX IF NOT EXISTS idx_page_sources_source ON page_sources(source_id);\
             CREATE TABLE IF NOT EXISTS page_access (\
                page_id      TEXT PRIMARY KEY,\
                reads        INTEGER NOT NULL DEFAULT 0,\
                search_hits  INTEGER NOT NULL DEFAULT 0,\
                last_read_at INTEGER\
             );",
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('version', '5')",
            [],
        )?;
        tx.commit()?;
    }
    if current < 6 {
        // Same single-IMMEDIATE-transaction pattern as v5.
        let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        apply_v6(&tx)?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('version', '6')",
            [],
        )?;
        tx.commit()?;
    }
    if current < 7 {
        let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        apply_v7(&tx)?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('version', '7')",
            [],
        )?;
        tx.commit()?;
    }
    conn.execute(
        "INSERT OR REPLACE INTO schema_meta(key, value) VALUES ('version', ?1)",
        rusqlite::params![CURRENT_VERSION.to_string()],
    )?;
    Ok(())
}

/// B2 `summary` and H1 dream-queue / page-vector schema:
///  - `pages.summary`, `pages.body_hash`, `pages.summary_body_hash` —
///    the indexed summary, the SHA-256 of the indexed body, and the body
///    hash at the time the summary text last changed. A summary is stale
///    when the two hashes differ (`wiki::dream`). Derived data, filled by
///    `pages_index` (its metadata version refreshes unchanged pages).
///  - `pages_fts` gains a `summary` column (weighted above the body in
///    `bm25`). FTS5 tables cannot `ALTER … ADD COLUMN`, so the table is
///    recreated and refilled from `pages` (title + body; the summary
///    column starts empty and is written when the page is re-indexed —
///    pages that HAVE a summary are re-indexed once by the next rebuild
///    because their stored `pages.summary` is still NULL).
///  - `page_vectors(page_id, embedding)` — the L2-normalised mean of a
///    page's chunk vectors, maintained by the rebuild; read by the
///    duplicate lint instead of re-summing every chunk.
fn apply_v6(conn: &Connection) -> DbResult<()> {
    for column in ["summary", "body_hash", "summary_body_hash"] {
        if !table_has_column(conn, "pages", column)? {
            conn.execute_batch(&format!("ALTER TABLE pages ADD COLUMN {column} TEXT;"))?;
        }
    }
    if !table_has_column(conn, "pages_fts", "summary")? {
        conn.execute_batch(
            "DROP TABLE IF EXISTS pages_fts;\
             CREATE VIRTUAL TABLE pages_fts USING fts5(\
                id UNINDEXED,\
                title,\
                body,\
                summary,\
                tokenize = 'unicode61 remove_diacritics 2'\
             );\
             INSERT INTO pages_fts(id, title, body, summary) \
                SELECT id, COALESCE(title, ''), COALESCE(body, ''), '' FROM pages;",
        )?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS page_vectors (\
            page_id   TEXT PRIMARY KEY,\
            embedding BLOB NOT NULL\
         );",
    )?;
    Ok(())
}

/// Version-skew guard for a vault shared by two PCs where one still runs
/// a pre-summary BRAIN: `pages.summary_indexed_hash` is the `file_hash`
/// that the summary-aware indexer last fully wrote for the page. An older
/// client re-indexing a page updates `file_hash` but not this column, so
/// the next run of a current client sees the mismatch and re-indexes the
/// page itself (`pages_index::is_unchanged`).
///
/// Existing rows are initialised with their `file_hash`: up to this
/// migration every row was written by a pre-summary indexer, whose
/// output differs from the current one only for pages WITH a summary —
/// and those are re-indexed anyway, because their `pages.summary` is
/// still NULL (v6). Initialising with NULL instead would re-embed every
/// page of the vault once.
fn apply_v7(conn: &Connection) -> DbResult<()> {
    if !table_has_column(conn, "pages", "summary_indexed_hash")? {
        conn.execute_batch(
            "ALTER TABLE pages ADD COLUMN summary_indexed_hash TEXT;\
             UPDATE pages SET summary_indexed_hash = file_hash;",
        )?;
    }
    Ok(())
}

fn has_chunk_vectors_table(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='chunk_vectors'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|c| c > 0)
    .unwrap_or(false)
}

pub fn chunk_vectors_available(conn: &Connection) -> bool {
    has_chunk_vectors_table(conn)
}

fn chunks_has_embedding_column(conn: &Connection) -> DbResult<bool> {
    table_has_column(conn, "chunks", "embedding")
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> DbResult<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info('{table}')"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for r in rows {
        if r? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn read_version(conn: &Connection) -> DbResult<i64> {
    let exists: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='schema_meta'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if exists == 0 {
        return Ok(0);
    }
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key='version'",
            [],
            |row| row.get(0),
        )
        .ok();
    Ok(v.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn apply_creates_schema_at_current_version() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key='version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v, CURRENT_VERSION.to_string());
    }

    #[test]
    fn apply_v2_adds_embedding_column_to_chunks() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info('chunks')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(cols.iter().any(|c| c == "embedding"));
    }

    #[test]
    fn apply_is_idempotent_when_run_twice() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='pages'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn apply_v5_adds_the_validity_columns_to_pages() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info('pages')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let wanted = ["valid_from", "valid_to", "superseded_by"];
        assert!(wanted.iter().all(|w| cols.iter().any(|c| c == w)), "columns: {cols:?}");
    }

    #[test]
    fn apply_v5_creates_the_alias_source_and_access_tables() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' \
                 AND name IN ('page_aliases', 'page_sources', 'page_access')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn re_running_the_v5_migration_on_a_v5_schema_succeeds() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        conn.execute("UPDATE schema_meta SET value = '4' WHERE key = 'version'", [])
            .unwrap();
        assert!(apply(&conn).is_ok());
    }

    #[test]
    fn upgrading_a_v4_database_to_v5_keeps_existing_page_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch(
            "INSERT INTO schema_meta(key, value) VALUES ('version', '4'); \
             INSERT INTO pages(id, type, path) VALUES ('entities/a', 'entity', 'p');",
        )
        .unwrap();
        apply(&conn).unwrap();
        let ids: i64 = conn
            .query_row("SELECT count(*) FROM pages WHERE valid_to IS NULL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ids, 1);
    }

    fn columns_of(conn: &Connection, table: &str) -> Vec<String> {
        conn.prepare(&format!("PRAGMA table_info('{table}')"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn apply_v6_adds_the_summary_and_body_hash_columns_to_pages() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        let cols = columns_of(&conn, "pages");
        let wanted = ["summary", "body_hash", "summary_body_hash"];
        assert!(wanted.iter().all(|w| cols.iter().any(|c| c == w)), "columns: {cols:?}");
    }

    #[test]
    fn apply_v6_gives_the_fts_table_a_summary_column() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        assert!(columns_of(&conn, "pages_fts").iter().any(|c| c == "summary"));
    }

    #[test]
    fn apply_v6_creates_the_page_vectors_table() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        assert!(columns_of(&conn, "page_vectors").iter().any(|c| c == "embedding"));
    }

    #[test]
    fn upgrading_a_v5_database_to_v6_keeps_existing_pages_full_text_searchable() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        // Back to the v5 shape: the three-column FTS table.
        conn.execute_batch(
            "DROP TABLE pages_fts; \
             CREATE VIRTUAL TABLE pages_fts USING fts5(id UNINDEXED, title, body); \
             INSERT INTO pages(id, type, path, title, body) \
                VALUES ('entities/alice', 'entity', 'p', 'Alice', 'lives in Berlin'); \
             INSERT INTO pages_fts(id, title, body) VALUES ('entities/alice', 'Alice', 'lives in Berlin'); \
             UPDATE schema_meta SET value = '5' WHERE key = 'version';",
        )
        .unwrap();
        apply(&conn).unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM pages_fts WHERE pages_fts MATCH 'berlin'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn apply_v7_adds_the_summary_indexed_hash_column_to_pages() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        assert!(columns_of(&conn, "pages").iter().any(|c| c == "summary_indexed_hash"));
    }

    #[test]
    fn upgrading_a_v6_database_initialises_summary_indexed_hash_with_the_file_hash() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch(
            "INSERT INTO schema_meta(key, value) VALUES ('version', '4'); \
             INSERT INTO pages(id, type, path, file_hash) VALUES ('entities/a', 'entity', 'p', 'h1');",
        )
        .unwrap();
        apply(&conn).unwrap();
        let hash: Option<String> = conn
            .query_row("SELECT summary_indexed_hash FROM pages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(hash.as_deref(), Some("h1"));
    }

    #[test]
    fn fts5_table_is_created_and_supports_match_queries() {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        conn.execute(
            "INSERT INTO pages_fts(id, title, body) VALUES ('entities/alice', 'Alice', 'lives in Berlin')",
            [],
        )
        .unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM pages_fts WHERE pages_fts MATCH 'berlin'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
    }
}
