//! B1 — retrieval evaluation (`brain eval`, MCP `brain_eval`).
//!
//! The eval set is a YAML list in `00_meta/eval-queries.yaml`:
//!
//! ```yaml
//! - id: q-kunde-a-laufzeit
//!   query: Wie lange läuft der Vertrag mit Kunde A?
//!   expected: [entities/kunde-a]
//!   note: optional free text
//! ```
//!
//! Each query is run through the three retrieval paths of the real search
//! ([`RetrievalMode`]: FTS only, dense only, hybrid RRF — the very code
//! `brain_search` runs) and scored against `expected` with Recall@10,
//! MRR (reciprocal rank of the first expected page within the top 10)
//! and nDCG@10 (binary relevance). Everything is deterministic: the same
//! index and set give the same numbers.
//!
//! **Where the files live, and why.** The set sits FLAT in `00_meta/`
//! (not in `00_meta/eval/`) because the synced meta mirror
//! (`wiki::encryption::MIRRORED_META_FILES`) and the watcher's `00_meta`
//! filter both work on plain file names: listing `eval-queries.yaml`
//! there is all it takes for both PCs to share the set (encrypted like
//! AGENTS.md in an encrypted vault) and for edits to auto-commit. The run
//! history `00_meta/eval-history.md` is machine-local (not mirrored, not
//! watched): numbers depend on the local index and embedder.
//!
//! The eval set is the user's/agent's own test questions — it is NOT the
//! holdout suite (C-16) and must never be mixed with it.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::db::{DbHandle, DbResult};
use crate::embedding::Embedder;
use crate::vault::layout::meta_dir;

use super::search::{Fusion, RetrievalMode, hybrid_ids_on_conn, ranked_ids_on_conn};

/// File name of the eval set inside `00_meta/` (synced).
pub const EVAL_SET_FILENAME: &str = "eval-queries.yaml";

/// File name of the run history inside `00_meta/` (local).
pub const EVAL_HISTORY_FILENAME: &str = "eval-history.md";

/// Cut-off rank of every metric.
pub const EVAL_K: usize = 10;

/// Longest id `brain_eval` (action `add`) generates from a query.
const GENERATED_ID_MAX_CHARS: usize = 48;

const SET_HEADER: &str = "\
# BRAIN retrieval eval set — one entry per test question.
# id: stable name; query: the question as a user would ask it;
# expected: page ids a good search returns in its top 10; note: optional.
# Run with `brain eval <vault>` or the MCP tool brain_eval; add entries
# with brain_eval (action add). Synced between machines like AGENTS.md.
";

const HISTORY_HEADER: &str = "\
# Retrieval eval history

One row per `brain eval` / `brain_eval` run on this machine (R = Recall@10,
nDCG = nDCG@10). Local file — not synced.

| Date | Queries | Index format | Embedder | FTS R | FTS MRR | FTS nDCG | Dense R | Dense MRR | Dense nDCG | Hybrid R | Hybrid MRR | Hybrid nDCG |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
";

#[derive(Debug, Error)]
pub enum EvalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("00_meta/eval-queries.yaml is not a valid eval set: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("{0}")]
    Invalid(String),
}

pub type EvalResult<T> = Result<T, EvalError>;

/// One test question of the eval set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalQuery {
    pub id: String,
    pub query: String,
    #[serde(default)]
    pub expected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Input of [`add_eval_query`]; `id` is generated from the query when
/// absent.
#[derive(Debug, Clone, Default)]
pub struct NewEvalQuery {
    pub id: Option<String>,
    pub query: String,
    pub expected: Vec<String>,
    pub note: Option<String>,
}

/// Recall@10, MRR and nDCG@10 of one query, or their mean over a set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Metrics {
    pub recall_at_10: f64,
    pub mrr: f64,
    pub ndcg_at_10: f64,
}

/// An expected page found in a mode's top 10, with its 1-based rank.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FoundPage {
    pub id: String,
    pub rank: usize,
}

/// One query in one mode.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryModeResult {
    pub mode: RetrievalMode,
    pub found: Vec<FoundPage>,
    pub missed: Vec<String>,
    #[serde(flatten)]
    pub metrics: Metrics,
}

/// One query across all modes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryReport {
    pub id: String,
    pub query: String,
    pub expected: Vec<String>,
    /// Expected ids that are not in the index (renamed/deleted pages) —
    /// fix the eval entry.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unknown_expected: Vec<String>,
    pub results: Vec<QueryModeResult>,
}

/// Mean metrics of one mode over the evaluated queries.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModeReport {
    pub mode: RetrievalMode,
    #[serde(flatten)]
    pub metrics: Metrics,
}

/// Result of [`run_eval`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvalReport {
    /// Number of evaluated queries (entries with at least one expected id).
    pub queries: usize,
    /// Ids of entries skipped because they list no expected page.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    pub index_format_version: Option<i64>,
    /// Embedder that wrote the index's vectors (`schema_meta`).
    pub index_embedder: Option<String>,
    /// Embedder that embedded the test questions in this run.
    pub query_embedder: String,
    /// Set when the two embedders differ (e.g. the index holds bge-m3
    /// vectors but the model failed to load and the questions were
    /// embedded by the hashed fallback): the dense-only and hybrid numbers
    /// are then meaningless.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub modes: Vec<ModeReport>,
    pub per_query: Vec<QueryReport>,
}

pub fn eval_set_path(vault: &Path) -> PathBuf {
    meta_dir(vault).join(EVAL_SET_FILENAME)
}

pub fn eval_history_path(vault: &Path) -> PathBuf {
    meta_dir(vault).join(EVAL_HISTORY_FILENAME)
}

/// The eval set of `vault`; empty when the file does not exist or holds
/// no entries.
pub fn load_eval_set(vault: &Path) -> EvalResult<Vec<EvalQuery>> {
    match std::fs::read_to_string(eval_set_path(vault)) {
        Ok(text) => parse_eval_set(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err.into()),
    }
}

fn parse_eval_set(text: &str) -> EvalResult<Vec<EvalQuery>> {
    Ok(serde_yaml::from_str::<Option<Vec<EvalQuery>>>(text)?.unwrap_or_default())
}

/// Metrics of one ranked list against the expected ids (binary
/// relevance; duplicates in `expected` count once; only the top
/// [`EVAL_K`] of `ranked` are considered).
pub fn query_metrics(ranked: &[String], expected: &[String]) -> Metrics {
    let relevant: HashSet<&str> = expected.iter().map(String::as_str).collect();
    if relevant.is_empty() {
        return Metrics::default();
    }
    let top = &ranked[..ranked.len().min(EVAL_K)];
    let mut found = 0usize;
    let mut first_rank: Option<usize> = None;
    let mut dcg = 0.0f64;
    let mut seen: HashSet<&str> = HashSet::new();
    for (i, id) in top.iter().enumerate() {
        if relevant.contains(id.as_str()) && seen.insert(id.as_str()) {
            found += 1;
            first_rank.get_or_insert(i + 1);
            dcg += 1.0 / ((i + 2) as f64).log2();
        }
    }
    let ideal: f64 = (0..relevant.len().min(EVAL_K))
        .map(|i| 1.0 / ((i + 2) as f64).log2())
        .sum();
    Metrics {
        recall_at_10: found as f64 / relevant.len() as f64,
        mrr: first_rank.map_or(0.0, |r| 1.0 / r as f64),
        ndcg_at_10: dcg / ideal,
    }
}

/// Mean of each metric (all zero for an empty slice).
pub fn mean_metrics(all: &[Metrics]) -> Metrics {
    if all.is_empty() {
        return Metrics::default();
    }
    let n = all.len() as f64;
    Metrics {
        recall_at_10: all.iter().map(|m| m.recall_at_10).sum::<f64>() / n,
        mrr: all.iter().map(|m| m.mrr).sum::<f64>() / n,
        ndcg_at_10: all.iter().map(|m| m.ndcg_at_10).sum::<f64>() / n,
    }
}

/// An "embedder" that returns one precomputed vector — so the query
/// vectors can be computed BEFORE the (timeout-bounded) DB part and each
/// query is embedded once for both the dense and the hybrid mode.
struct FixedVector<'a>(&'a [f32]);

impl Embedder for FixedVector<'_> {
    fn dim(&self) -> usize {
        self.0.len()
    }
    fn embed(&self, _text: &str) -> Vec<f32> {
        self.0.to_vec()
    }
    fn name(&self) -> &'static str {
        "precomputed"
    }
}

/// Embed every query of `set` (no DB access — run it outside `db_op`).
pub fn embed_queries(embedder: &dyn Embedder, set: &[EvalQuery]) -> Vec<Vec<f32>> {
    set.iter().map(|q| embedder.embed(&q.query)).collect()
}

/// What the report needs from the index besides the per-query results.
#[derive(Debug, Clone, Default)]
pub struct IndexFacts {
    pub index_format_version: Option<i64>,
    pub index_embedder: Option<String>,
    /// Every indexed page id (to flag expected ids that are gone).
    pub page_ids: HashSet<String>,
}

/// Read [`IndexFacts`] (one short DB op).
pub fn index_facts(conn: &rusqlite::Connection) -> DbResult<IndexFacts> {
    let page_ids: HashSet<String> = {
        let mut stmt = conn.prepare("SELECT id FROM pages")?;
        stmt.query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<_, _>>()?
    };
    Ok(IndexFacts {
        index_format_version: crate::db::pages_index::index_format_version(conn),
        index_embedder: crate::db::pages_index::index_embedder(conn),
        page_ids,
    })
}

/// Run ONE eval entry (which must list expected pages) in every mode.
/// `vector` is the embedding of `entry.query` ([`embed_queries`]). The
/// MCP server runs one `db_op` per query so a large set is never one
/// long, timeout-prone lock.
pub fn eval_query_on_conn(
    conn: &rusqlite::Connection,
    entry: &EvalQuery,
    vector: &[f32],
) -> DbResult<Vec<QueryModeResult>> {
    let embedder = FixedVector(vector);
    let mut results = Vec::new();
    for mode in RetrievalMode::ALL {
        let ranked = ranked_ids_on_conn(conn, &embedder, &entry.query, mode, EVAL_K)?;
        let found: Vec<FoundPage> = ranked
            .iter()
            .enumerate()
            .filter(|(_, id)| entry.expected.contains(id))
            .map(|(i, id)| FoundPage {
                id: id.clone(),
                rank: i + 1,
            })
            .collect();
        let missed: Vec<String> = entry
            .expected
            .iter()
            .filter(|e| !ranked.contains(e))
            .cloned()
            .collect();
        results.push(QueryModeResult {
            mode,
            found,
            missed,
            metrics: query_metrics(&ranked, &entry.expected),
        });
    }
    Ok(results)
}

/// Assemble the report from the per-entry results (`None` = the entry
/// lists no expected page and was skipped), in set order.
pub fn assemble_report(
    set: &[EvalQuery],
    results: Vec<Option<Vec<QueryModeResult>>>,
    facts: &IndexFacts,
    query_embedder: &str,
) -> EvalReport {
    let mut per_query = Vec::new();
    let mut skipped = Vec::new();
    for (entry, result) in set.iter().zip(results) {
        let Some(results) = result else {
            skipped.push(entry.id.clone());
            continue;
        };
        per_query.push(QueryReport {
            id: entry.id.clone(),
            query: entry.query.clone(),
            expected: entry.expected.clone(),
            unknown_expected: entry
                .expected
                .iter()
                .filter(|e| !facts.page_ids.contains(*e))
                .cloned()
                .collect(),
            results,
        });
    }
    let modes = RetrievalMode::ALL
        .into_iter()
        .map(|mode| {
            let all: Vec<Metrics> = per_query
                .iter()
                .flat_map(|q| {
                    q.results
                        .iter()
                        .filter(|r| r.mode == mode)
                        .map(|r| r.metrics)
                })
                .collect();
            ModeReport {
                mode,
                metrics: mean_metrics(&all),
            }
        })
        .collect();
    let warning = (facts.index_embedder.as_deref() != Some(query_embedder)).then(|| {
        format!(
            "the index vectors come from '{}' but the questions were embedded with \
             '{query_embedder}' — the dense-only and hybrid numbers are meaningless until both \
             match (install the embedding model and rebuild the index)",
            facts.index_embedder.as_deref().unwrap_or("unknown")
        )
    });
    EvalReport {
        queries: per_query.len(),
        skipped,
        index_format_version: facts.index_format_version,
        index_embedder: facts.index_embedder.clone(),
        query_embedder: query_embedder.to_string(),
        warning,
        modes,
        per_query,
    }
}

/// Run the whole eval on one connection. `query_vectors[i]` is the
/// embedding of `set[i].query` ([`embed_queries`]).
pub fn run_eval_on_conn(
    conn: &rusqlite::Connection,
    set: &[EvalQuery],
    query_vectors: &[Vec<f32>],
    query_embedder: &str,
) -> DbResult<EvalReport> {
    let facts = index_facts(conn)?;
    let mut results = Vec::with_capacity(set.len());
    for (entry, vector) in set.iter().zip(query_vectors) {
        results.push(if entry.expected.is_empty() {
            None
        } else {
            Some(eval_query_on_conn(conn, entry, vector)?)
        });
    }
    Ok(assemble_report(set, results, &facts, query_embedder))
}

/// The eval on a GUI/CLI handle with the process-cached embedder of
/// `vault`. The questions are embedded with no lock held, then each one
/// runs in its own `db.with`, so searches and index batches can run
/// between them — the lock is never held across the whole set.
pub fn run_eval(db: &DbHandle, vault: &Path, set: &[EvalQuery]) -> DbResult<EvalReport> {
    let (vectors, embedder_name) = embed_set(vault, set);
    run_eval_with_vectors(db, set, &vectors, embedder_name)
}

/// The query vectors of `set`, embedded once with `vault`'s cached
/// embedder, and that embedder's name.
pub fn embed_set(vault: &Path, set: &[EvalQuery]) -> (Vec<Vec<f32>>, &'static str) {
    let embedder = crate::embedding::cached_for_vault(vault);
    (embed_queries(embedder.as_ref(), set), embedder.name())
}

/// [`run_eval`] with query vectors already computed ([`embed_set`]), so
/// the CLI can reuse them for the fusion comparison.
pub fn run_eval_with_vectors(
    db: &DbHandle,
    set: &[EvalQuery],
    vectors: &[Vec<f32>],
    query_embedder: &str,
) -> DbResult<EvalReport> {
    let facts = db.with(index_facts)?;
    let mut results = Vec::with_capacity(set.len());
    for (entry, vector) in set.iter().zip(vectors) {
        results.push(if entry.expected.is_empty() {
            None
        } else {
            Some(db.with(|conn| eval_query_on_conn(conn, entry, vector))?)
        });
    }
    Ok(assemble_report(set, results, &facts, query_embedder))
}

/// Mean hybrid metrics of every [`Fusion`] variant over the entries of
/// `set` that list expected pages, on one connection. `query_vectors[i]`
/// is the embedding of `set[i].query`. The CLI prints it under the main
/// table so a real vault shows which fusion wins; the eval history and
/// the Settings card keep reporting the default fusion only.
pub fn fusion_comparison_on_conn(
    conn: &rusqlite::Connection,
    set: &[EvalQuery],
    query_vectors: &[Vec<f32>],
) -> DbResult<Vec<(Fusion, Metrics)>> {
    let mut per_variant: Vec<Vec<Metrics>> = vec![Vec::new(); Fusion::ALL.len()];
    for (entry, vector) in set.iter().zip(query_vectors) {
        if entry.expected.is_empty() {
            continue;
        }
        let embedder = FixedVector(vector);
        for (i, fusion) in Fusion::ALL.into_iter().enumerate() {
            let ranked = hybrid_ids_on_conn(conn, &embedder, &entry.query, fusion, EVAL_K)?;
            per_variant[i].push(query_metrics(&ranked, &entry.expected));
        }
    }
    Ok(Fusion::ALL
        .into_iter()
        .zip(per_variant)
        .map(|(fusion, metrics)| (fusion, mean_metrics(&metrics)))
        .collect())
}

/// [`fusion_comparison_on_conn`] on a CLI handle, with the query vectors
/// the main eval already computed ([`embed_set`]).
pub fn fusion_comparison(
    db: &DbHandle,
    set: &[EvalQuery],
    vectors: &[Vec<f32>],
) -> DbResult<Vec<(Fusion, Metrics)>> {
    db.with(|conn| fusion_comparison_on_conn(conn, set, vectors))
}

/// The fusion comparison as a plain-text table; the default fusion is
/// marked.
pub fn render_fusion_table(rows: &[(Fusion, Metrics)]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Hybrid fusion variants (* = default):");
    let _ = writeln!(
        out,
        "{:<20}{:>11}{:>9}{:>10}",
        "fusion", "Recall@10", "MRR", "nDCG@10"
    );
    for (fusion, m) in rows {
        let marker = if *fusion == super::search::FUSION {
            "*"
        } else {
            " "
        };
        let _ = writeln!(
            out,
            "{marker}{:<19}{:>11.3}{:>9.3}{:>10.3}",
            fusion.label(),
            m.recall_at_10,
            m.mrr,
            m.ndcg_at_10
        );
    }
    out
}

/// The report as a plain-text table (CLI output).
pub fn render_table(report: &EvalReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Retrieval eval — {} queries, index format {}, index embedder {}, query embedder {}",
        report.queries,
        report
            .index_format_version
            .map_or_else(|| "unknown".to_string(), |v| v.to_string()),
        report.index_embedder.as_deref().unwrap_or("unknown"),
        report.query_embedder,
    );
    if let Some(warning) = &report.warning {
        let _ = writeln!(out, "WARNING: {warning}");
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "{:<12}{:>11}{:>9}{:>10}",
        "mode", "Recall@10", "MRR", "nDCG@10"
    );
    for m in &report.modes {
        let _ = writeln!(
            out,
            "{:<12}{:>11.3}{:>9.3}{:>10.3}",
            m.mode.label(),
            m.metrics.recall_at_10,
            m.metrics.mrr,
            m.metrics.ndcg_at_10
        );
    }
    let misses: Vec<String> = report
        .per_query
        .iter()
        .filter_map(|q| {
            let hybrid = q.results.iter().find(|r| r.mode == RetrievalMode::Hybrid)?;
            (!hybrid.missed.is_empty())
                .then(|| format!("  {}: hybrid misses {}", q.id, hybrid.missed.join(", ")))
        })
        .collect();
    if !misses.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Queries whose expected pages hybrid search misses in its top 10:"
        );
        for line in misses {
            let _ = writeln!(out, "{line}");
        }
    }
    let unknown: Vec<String> = report
        .per_query
        .iter()
        .filter(|q| !q.unknown_expected.is_empty())
        .map(|q| format!("  {}: {}", q.id, q.unknown_expected.join(", ")))
        .collect();
    if !unknown.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Expected ids that are not in the index (fix the eval entry):"
        );
        for line in unknown {
            let _ = writeln!(out, "{line}");
        }
    }
    if !report.skipped.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Skipped (no expected pages): {}",
            report.skipped.join(", ")
        );
    }
    out
}

/// Append one table row for `report` to `00_meta/eval-history.md`
/// (created with a header on first use).
pub fn append_history(
    vault: &Path,
    report: &EvalReport,
    when: chrono::DateTime<chrono::Local>,
) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    let path = eval_history_path(vault);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut row = format!(
        "| {} | {} | {} | {} |",
        when.format("%Y-%m-%d %H:%M"),
        report.queries,
        report
            .index_format_version
            .map_or_else(|| "?".to_string(), |v| v.to_string()),
        report.index_embedder.as_deref().unwrap_or("?"),
    );
    for mode in RetrievalMode::ALL {
        let m = report
            .modes
            .iter()
            .find(|r| r.mode == mode)
            .map(|r| r.metrics)
            .unwrap_or_default();
        let _ = write!(
            row,
            " {:.3} | {:.3} | {:.3} |",
            m.recall_at_10, m.mrr, m.ndcg_at_10
        );
    }
    row.push('\n');
    let is_new = !path.is_file();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if is_new {
        file.write_all(HISTORY_HEADER.as_bytes())?;
    }
    file.write_all(row.as_bytes())?;
    Ok(path)
}

/// Whitespace-collapsed, lowercase form of a query, for the duplicate
/// check.
fn query_key(q: &str) -> String {
    q.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Validate `new` and append it to the eval set. Refuses an empty query,
/// an empty `expected`, an expected id that is malformed or has no page,
/// an id that is already taken and a query that is already in the set
/// (case and whitespace ignored). Without an id, one is generated from
/// the query (`q-<slug>`, numbered on collision). Existing entries and
/// comments in the file are kept: the entry is appended as text.
pub fn add_eval_query(vault: &Path, new: NewEvalQuery) -> EvalResult<EvalQuery> {
    let query = new.query.trim().to_string();
    if query.is_empty() {
        return Err(EvalError::Invalid("query must not be empty".into()));
    }
    let mut expected: Vec<String> = Vec::new();
    for raw in &new.expected {
        let id = raw.trim().to_string();
        crate::wiki::refactor::validate_page_id(&id)
            .map_err(|e| EvalError::Invalid(format!("expected: {e}")))?;
        let exists = crate::wiki::encryption::page_path(vault, &id)
            .map(|p| p.is_file())
            .unwrap_or(false);
        if !exists {
            return Err(EvalError::Invalid(format!(
                "expected page '{id}' does not exist — use the id of an existing page \
                 (brain_lookup)"
            )));
        }
        if !expected.contains(&id) {
            expected.push(id);
        }
    }
    if expected.is_empty() {
        return Err(EvalError::Invalid(
            "expected must list at least one page id".into(),
        ));
    }

    let path = eval_set_path(vault);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Read-modify-write under a lock file, so two concurrent adds (two
    // agent sessions, two MCP processes) cannot lose an entry.
    let _lock = lock_eval_set(&path)?;
    let existing_text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err.into()),
    };
    let set = parse_eval_set(&existing_text)?;
    let key = query_key(&query);
    if let Some(dup) = set.iter().find(|q| query_key(&q.query) == key) {
        return Err(EvalError::Invalid(format!(
            "the eval set already has this query as '{}'",
            dup.id
        )));
    }
    let id = match new.id.map(|s| s.trim().to_string()) {
        Some(id) if id.is_empty() || id.chars().any(char::is_control) => {
            return Err(EvalError::Invalid(
                "id must be non-empty text without control characters".into(),
            ));
        }
        Some(id) => {
            if set.iter().any(|q| q.id == id) {
                return Err(EvalError::Invalid(format!(
                    "the eval set already has an entry with id '{id}'"
                )));
            }
            id
        }
        None => generated_id(&query, &set),
    };
    let entry = EvalQuery {
        id,
        query,
        expected,
        note: new
            .note
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty()),
    };

    // Append as text so comments and formatting survive; if the result
    // does not parse as the old set plus this entry (e.g. the file is a
    // flow-style `[...]` list), rewrite the whole set instead.
    let block = serde_yaml::to_string(&vec![entry.clone()])?;
    let base = if existing_text.trim().is_empty() {
        SET_HEADER.to_string()
    } else if existing_text.ends_with('\n') {
        existing_text.clone()
    } else {
        format!("{existing_text}\n")
    };
    let appended = format!("{base}{block}");
    let mut expected_set = set.clone();
    expected_set.push(entry.clone());
    let text = match parse_eval_set(&appended) {
        Ok(parsed) if parsed == expected_set => appended,
        _ => format!("{SET_HEADER}{}", serde_yaml::to_string(&expected_set)?),
    };
    // Write-then-rename: a reader (the watcher's commit, a sync) never
    // sees a half-written set. The temp name is unique per call and the
    // rename retries through a transient Windows lock (a reader that
    // still has the set open).
    crate::fsutil::atomic_write(&path, text.as_bytes())?;
    Ok(entry)
}

/// How long [`add_eval_query`] waits for another writer's lock.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// A lock file older than this is left over from a crashed writer and is
/// taken over.
const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(30);

/// Held lock on the eval set; removes the lock file when dropped.
struct EvalSetLock(PathBuf);

impl Drop for EvalSetLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Take `<set>.lock` (created exclusively), waiting up to [`LOCK_WAIT`].
///
/// On Windows, creating the lock file while the previous holder is just
/// deleting it fails with "access denied" (the old file is still "delete
/// pending") instead of "already exists" — the cause of the old flaky
/// `concurrent_adds_keep_every_entry`. That error is treated as "busy"
/// too; only if it persists until the deadline is it returned as is.
fn lock_eval_set(set_path: &Path) -> EvalResult<EvalSetLock> {
    let lock = set_path.with_extension("yaml.lock");
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(_) => return Ok(EvalSetLock(lock)),
            Err(err) if crate::fsutil::is_transient_lock_error(&err) => {
                if std::time::Instant::now() >= deadline {
                    return Err(err.into());
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&lock)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > LOCK_STALE);
                if stale {
                    let _ = std::fs::remove_file(&lock);
                    continue;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(EvalError::Invalid(
                        "the eval set is being changed by another session — try again".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

/// `q-<slug of the query>` (at most [`GENERATED_ID_MAX_CHARS`]), with
/// `-2`, `-3`, … appended while the id is taken.
fn generated_id(query: &str, set: &[EvalQuery]) -> String {
    let slug: String = crate::wiki::page::slug_key(query)
        .chars()
        .take(GENERATED_ID_MAX_CHARS)
        .collect();
    let slug = slug.trim_end_matches('-');
    let base = if slug.is_empty() {
        "q".to_string()
    } else {
        format!("q-{slug}")
    };
    let taken = |id: &str| set.iter().any(|q| q.id == id);
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !taken(id))
        .unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::{ensure_skeleton, wiki_dir};
    use tempfile::TempDir;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// The three fixture queries: a perfect hit, a half hit at rank 2, a
    /// miss.
    fn fixture_metrics() -> Vec<Metrics> {
        vec![
            query_metrics(&ids(&["a", "b", "c"]), &ids(&["a"])),
            query_metrics(&ids(&["x", "a", "y"]), &ids(&["a", "b"])),
            query_metrics(&ids(&["x", "y"]), &ids(&["z"])),
        ]
    }

    #[test]
    fn a_first_rank_hit_scores_one_on_every_metric() {
        let m = query_metrics(&ids(&["a", "b"]), &ids(&["a"]));
        assert_eq!(
            m,
            Metrics {
                recall_at_10: 1.0,
                mrr: 1.0,
                ndcg_at_10: 1.0
            }
        );
    }

    #[test]
    fn a_miss_scores_zero_on_every_metric() {
        let m = query_metrics(&ids(&["x"]), &ids(&["a"]));
        assert_eq!(m, Metrics::default());
    }

    #[test]
    fn a_hit_beyond_rank_ten_does_not_count() {
        let mut ranked: Vec<String> = (0..10).map(|i| format!("x{i}")).collect();
        ranked.push("a".into());
        assert_eq!(query_metrics(&ranked, &ids(&["a"])).recall_at_10, 0.0);
    }

    #[test]
    fn the_mean_recall_of_the_fixture_is_one_half() {
        assert!(close(mean_metrics(&fixture_metrics()).recall_at_10, 0.5));
    }

    #[test]
    fn the_mean_reciprocal_rank_of_the_fixture_is_one_half() {
        assert!(close(mean_metrics(&fixture_metrics()).mrr, 0.5));
    }

    #[test]
    fn the_mean_ndcg_of_the_fixture_is_the_average_of_one_and_the_rank_two_ndcg() {
        // Query 2: DCG = 1/log2(3); ideal DCG for two relevant = 1 + 1/log2(3).
        let q2 = (1.0 / 3f64.log2()) / (1.0 + 1.0 / 3f64.log2());
        assert!(close(
            mean_metrics(&fixture_metrics()).ndcg_at_10,
            (1.0 + q2) / 3.0
        ));
    }

    // ---- end to end on an index ------------------------------------------

    fn write(vault: &Path, id: &str, title: &str, body: &str) {
        let (sub, slug) = id.split_once('/').unwrap();
        let dir = wiki_dir(vault).join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{slug}.md")),
            format!("---\nid: {id}\ntype: entity\ntitle: {title}\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    fn fixture_vault() -> (TempDir, DbHandle) {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write(
            tmp.path(),
            "entities/kunde-a",
            "Kunde A",
            "Der Vertrag läuft zwölf Monate.",
        );
        write(
            tmp.path(),
            "entities/kunde-b",
            "Kunde B",
            "Kunde B kauft Pizza.",
        );
        write(
            tmp.path(),
            "concepts/nlspec",
            "NLSpec",
            "Specs in natural language.",
        );
        let db = DbHandle::open(tmp.path()).unwrap();
        crate::db::pages_index::rebuild_with(
            &db,
            tmp.path(),
            &crate::embedding::hashed::HashedEmbedder::new(),
        )
        .unwrap();
        (tmp, db)
    }

    fn fixture_set() -> Vec<EvalQuery> {
        vec![
            EvalQuery {
                id: "q1".into(),
                query: "Vertrag".into(),
                expected: ids(&["entities/kunde-a"]),
                note: None,
            },
            EvalQuery {
                id: "q2".into(),
                query: "Pizza".into(),
                expected: ids(&["entities/kunde-b"]),
                note: None,
            },
            EvalQuery {
                id: "q3".into(),
                query: "natural language".into(),
                expected: ids(&["concepts/nlspec"]),
                note: None,
            },
        ]
    }

    fn run(db: &DbHandle, set: &[EvalQuery]) -> EvalReport {
        let embedder = crate::embedding::hashed::HashedEmbedder::new();
        let vectors = embed_queries(&embedder, set);
        db.with(|conn| run_eval_on_conn(conn, set, &vectors, embedder.name()))
            .unwrap()
    }

    #[test]
    fn the_fusion_comparison_lists_every_variant() {
        let (_tmp, db) = fixture_vault();
        let set = fixture_set();
        let vectors = embed_queries(&crate::embedding::hashed::HashedEmbedder::new(), &set);
        let rows = db
            .with(|conn| fusion_comparison_on_conn(conn, &set, &vectors))
            .unwrap();
        assert_eq!(rows.len(), Fusion::ALL.len());
    }

    #[test]
    fn the_default_fusion_row_equals_the_hybrid_mode_of_the_report() {
        let (_tmp, db) = fixture_vault();
        let set = fixture_set();
        let vectors = embed_queries(&crate::embedding::hashed::HashedEmbedder::new(), &set);
        let rows = db
            .with(|conn| fusion_comparison_on_conn(conn, &set, &vectors))
            .unwrap();
        let default_row = rows
            .iter()
            .find(|(f, _)| *f == super::super::search::FUSION)
            .unwrap()
            .1;
        let hybrid = run(&db, &set)
            .modes
            .iter()
            .find(|m| m.mode == RetrievalMode::Hybrid)
            .unwrap()
            .metrics;
        assert_eq!(default_row, hybrid);
    }

    #[test]
    fn matching_index_and_query_embedders_give_no_warning() {
        let (_tmp, db) = fixture_vault();
        assert_eq!(run(&db, &fixture_set()).warning, None);
    }

    #[test]
    fn a_query_embedder_other_than_the_index_embedder_gives_a_warning() {
        let (_tmp, db) = fixture_vault();
        let set = fixture_set();
        let vectors = embed_queries(&crate::embedding::hashed::HashedEmbedder::new(), &set);
        let report = db
            .with(|conn| run_eval_on_conn(conn, &set, &vectors, "bge-m3"))
            .unwrap();
        assert!(
            report
                .warning
                .as_deref()
                .is_some_and(|w| w.contains("meaningless")),
            "{:?}",
            report.warning
        );
    }

    #[test]
    fn full_text_recall_is_perfect_when_every_query_word_is_unique_to_its_page() {
        let (_tmp, db) = fixture_vault();
        let report = run(&db, &fixture_set());
        let fts = report
            .modes
            .iter()
            .find(|m| m.mode == RetrievalMode::FtsOnly)
            .unwrap();
        assert_eq!(fts.metrics.recall_at_10, 1.0);
    }

    #[test]
    fn a_second_run_on_an_unchanged_index_gives_identical_numbers() {
        let (_tmp, db) = fixture_vault();
        let first = run(&db, &fixture_set());
        assert_eq!(first, run(&db, &fixture_set()));
    }

    #[test]
    fn the_report_covers_the_three_retrieval_modes() {
        let (_tmp, db) = fixture_vault();
        let modes: Vec<RetrievalMode> = run(&db, &fixture_set())
            .modes
            .iter()
            .map(|m| m.mode)
            .collect();
        assert_eq!(modes, RetrievalMode::ALL.to_vec());
    }

    #[test]
    fn an_entry_without_expected_pages_is_skipped() {
        let (_tmp, db) = fixture_vault();
        let mut set = fixture_set();
        set[2].expected.clear();
        assert_eq!(run(&db, &set).skipped, vec!["q3".to_string()]);
    }

    #[test]
    fn an_expected_id_missing_from_the_index_is_reported() {
        let (_tmp, db) = fixture_vault();
        let mut set = fixture_set();
        set[0].expected.push("entities/gone".into());
        assert_eq!(
            run(&db, &set).per_query[0].unknown_expected,
            vec!["entities/gone".to_string()]
        );
    }

    #[test]
    fn the_report_records_the_index_embedder() {
        let (_tmp, db) = fixture_vault();
        assert_eq!(
            run(&db, &fixture_set()).index_embedder.as_deref(),
            Some(crate::embedding::hashed::HashedEmbedder::new().name())
        );
    }

    // ---- history -------------------------------------------------------------

    fn when() -> chrono::DateTime<chrono::Local> {
        use chrono::TimeZone as _;
        chrono::Local
            .with_ymd_and_hms(2026, 10, 6, 14, 3, 0)
            .unwrap()
    }

    #[test]
    fn each_run_appends_one_row_to_the_history() {
        let (tmp, db) = fixture_vault();
        let report = run(&db, &fixture_set());
        append_history(tmp.path(), &report, when()).unwrap();
        append_history(tmp.path(), &report, when()).unwrap();
        let text = std::fs::read_to_string(eval_history_path(tmp.path())).unwrap();
        assert_eq!(
            text.lines()
                .filter(|l| l.starts_with("| 2026-10-06 14:03 |"))
                .count(),
            2
        );
    }

    #[test]
    fn a_history_row_carries_date_query_count_format_embedder_and_nine_numbers() {
        let (tmp, db) = fixture_vault();
        let report = run(&db, &fixture_set());
        append_history(tmp.path(), &report, when()).unwrap();
        let text = std::fs::read_to_string(eval_history_path(tmp.path())).unwrap();
        let row = text.lines().last().unwrap();
        assert!(
            row.starts_with("| 2026-10-06 14:03 | 3 | 3 | hashed")
                && row.matches(" | ").count() == 12,
            "{row}"
        );
    }

    // ---- add_eval_query ----------------------------------------------------

    fn new_query(query: &str, expected: &[&str]) -> NewEvalQuery {
        NewEvalQuery {
            id: None,
            query: query.into(),
            expected: ids(expected),
            note: None,
        }
    }

    #[test]
    fn an_added_query_can_be_loaded_back_from_the_eval_set() {
        let (tmp, _db) = fixture_vault();
        let added = add_eval_query(
            tmp.path(),
            new_query("Wer kauft Pizza?", &["entities/kunde-b"]),
        )
        .unwrap();
        assert_eq!(load_eval_set(tmp.path()).unwrap(), vec![added]);
    }

    #[test]
    fn an_added_query_without_an_id_gets_one_generated_from_the_query() {
        let (tmp, _db) = fixture_vault();
        let added = add_eval_query(
            tmp.path(),
            new_query("Wer kauft Pizza?", &["entities/kunde-b"]),
        )
        .unwrap();
        assert_eq!(added.id, "q-wer-kauft-pizza");
    }

    #[test]
    fn adding_a_query_keeps_the_comments_of_the_eval_set_file() {
        let (tmp, _db) = fixture_vault();
        add_eval_query(tmp.path(), new_query("Pizza?", &["entities/kunde-b"])).unwrap();
        add_eval_query(tmp.path(), new_query("Vertrag?", &["entities/kunde-a"])).unwrap();
        let text = std::fs::read_to_string(eval_set_path(tmp.path())).unwrap();
        assert!(text.starts_with(SET_HEADER), "{text}");
    }

    #[test]
    fn adding_a_query_to_a_flow_style_set_rewrites_it_as_a_valid_set() {
        let (tmp, _db) = fixture_vault();
        std::fs::write(
            eval_set_path(tmp.path()),
            "[{id: q1, query: Vertrag, expected: [entities/kunde-a]}]",
        )
        .unwrap();
        add_eval_query(tmp.path(), new_query("Pizza?", &["entities/kunde-b"])).unwrap();
        assert_eq!(load_eval_set(tmp.path()).unwrap().len(), 2);
    }

    #[test]
    fn adding_a_query_whose_expected_page_does_not_exist_is_refused() {
        let (tmp, _db) = fixture_vault();
        let err =
            add_eval_query(tmp.path(), new_query("Pizza?", &["entities/nobody"])).unwrap_err();
        assert!(
            err.to_string().contains("'entities/nobody' does not exist"),
            "{err}"
        );
    }

    #[test]
    fn adding_a_query_with_a_malformed_expected_id_is_refused() {
        let (tmp, _db) = fixture_vault();
        assert!(add_eval_query(tmp.path(), new_query("Pizza?", &["../etc/passwd"])).is_err());
    }

    #[test]
    fn adding_a_query_without_expected_pages_is_refused() {
        let (tmp, _db) = fixture_vault();
        assert!(add_eval_query(tmp.path(), new_query("Pizza?", &[])).is_err());
    }

    #[test]
    fn adding_the_same_query_twice_is_refused_naming_the_existing_entry() {
        let (tmp, _db) = fixture_vault();
        add_eval_query(
            tmp.path(),
            new_query("Wer kauft Pizza?", &["entities/kunde-b"]),
        )
        .unwrap();
        let err = add_eval_query(
            tmp.path(),
            new_query("  wer KAUFT  pizza? ", &["entities/kunde-b"]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("'q-wer-kauft-pizza'"), "{err}");
    }

    #[test]
    fn adding_an_entry_with_a_taken_id_is_refused() {
        let (tmp, _db) = fixture_vault();
        let mut first = new_query("Pizza?", &["entities/kunde-b"]);
        first.id = Some("q-x".into());
        add_eval_query(tmp.path(), first).unwrap();
        let mut second = new_query("Vertrag?", &["entities/kunde-a"]);
        second.id = Some("q-x".into());
        assert!(add_eval_query(tmp.path(), second).is_err());
    }

    #[test]
    fn a_generated_id_that_is_taken_gets_a_number() {
        let set = vec![EvalQuery {
            id: "q-pizza".into(),
            query: "other".into(),
            expected: ids(&["entities/kunde-b"]),
            note: None,
        }];
        assert_eq!(generated_id("Pizza", &set), "q-pizza-2");
    }

    #[test]
    fn concurrent_adds_keep_every_entry() {
        let (tmp, _db) = fixture_vault();
        let vault = tmp.path().to_path_buf();
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let vault = vault.clone();
                std::thread::spawn(move || {
                    add_eval_query(
                        &vault,
                        new_query(&format!("Frage {i}"), &["entities/kunde-a"]),
                    )
                    .unwrap()
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(load_eval_set(&vault).unwrap().len(), 4);
    }

    #[test]
    fn adding_a_query_leaves_no_lock_or_temporary_file_behind() {
        let (tmp, _db) = fixture_vault();
        add_eval_query(tmp.path(), new_query("Pizza?", &["entities/kunde-b"])).unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(meta_dir(tmp.path()))
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with("eval-queries.yaml."))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_stale_lock_left_by_a_crashed_writer_is_taken_over() {
        let (tmp, _db) = fixture_vault();
        let lock = eval_set_path(tmp.path()).with_extension("yaml.lock");
        let file = std::fs::File::create(&lock).unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .unwrap();
        drop(file);
        assert!(add_eval_query(tmp.path(), new_query("Pizza?", &["entities/kunde-b"])).is_ok());
    }

    #[test]
    fn a_missing_eval_set_file_is_an_empty_set() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        assert!(load_eval_set(tmp.path()).unwrap().is_empty());
    }
}
