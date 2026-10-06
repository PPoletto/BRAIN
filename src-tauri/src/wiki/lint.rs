//! Lint pass over the wiki: frontmatter validity, link integrity, ID
//! uniqueness. Run before each auto-commit. Hard errors block the commit.
//!
//! Two entry points:
//!  - [`lint`] — filesystem only, fast. The watcher's pre-commit gate and
//!    the write tools' page-scoped checks use it.
//!  - [`lint_with_index`] — `lint` plus the index-backed hygiene rules of
//!    [`super::hygiene`] (orphans, duplicate candidates). Slower (it reads
//!    every chunk vector), never blocks commits; used by
//!    `brain_lint_report`, the Integrity page and the scheduled audit.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::db::DbHandle;
use crate::vault::layout::{wiki_dir, WIKI_SUBDIRS};

use regex::Regex;

use super::page::{parse, ParsedPage};
use super::WikiResult;

/// Canonical singular `type:` values accepted in page frontmatter. The
/// project intentionally keeps this list hardcoded: introducing a new
/// page-type is a design decision, not a per-page choice, and a code
/// change makes that conscious. Pages whose frontmatter type falls
/// outside this set are surfaced as `unregistered-type` Warnings — never
/// as Errors — so reads, auto-commits and the indexer continue to see
/// them unchanged. An agent or the user then corrects the field via
/// `brain_write_page` (the typical case is the directory-name plural,
/// e.g. `type: entities` → `type: entity`).
pub const KNOWN_TYPES: &[&str] = &["entity", "concept", "source", "topic"];

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LintReport {
    pub errors: Vec<LintError>,
    pub warnings: Vec<LintWarning>,
    /// Info-level notes that are not findings about a page — e.g. "a
    /// check was skipped because the embedding model is missing". Same
    /// `{path, kind, message}` shape (path may be empty). Omitted from
    /// the JSON when empty, so the filesystem lint's output is unchanged.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<LintWarning>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LintError {
    pub path: String,
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LintWarning {
    pub path: String,
    pub kind: String,
    pub message: String,
}

impl LintReport {
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }
}

/// [`lint`] plus the index-backed hygiene rules (`orphan`,
/// `duplicate-candidate`; see [`super::hygiene`]). All hygiene findings
/// are warnings. Without a DB handle this is exactly [`lint`]. If the
/// index cannot be read, the filesystem findings are still returned
/// together with a note saying the hygiene checks were skipped.
pub fn lint_with_index(vault: &Path, db: Option<&DbHandle>) -> WikiResult<LintReport> {
    let mut report = lint(vault)?;
    let Some(db) = db else {
        return Ok(report);
    };
    let rows = db
        .with(super::hygiene::load_rows)
        .map_err(|err| err.to_string());
    add_hygiene(&mut report, vault, rows);
    Ok(report)
}

/// Append the hygiene findings for `rows` (loaded by
/// [`super::hygiene::load_rows`]) to `report`. When loading failed
/// (`Err` carries the reason, e.g. an index timeout in the MCP server),
/// a `hygiene-skipped` note says so instead of silently omitting them.
pub fn add_hygiene(
    report: &mut LintReport,
    vault: &Path,
    rows: Result<super::hygiene::HygieneRows, String>,
) {
    match rows {
        Ok(rows) => {
            let findings = super::hygiene::evaluate(
                &rows,
                crate::embedding::model_available(vault),
                chrono::Utc::now().timestamp(),
            );
            report.warnings.extend(findings.warnings);
            report.notes.extend(findings.notes);
        }
        Err(err) => {
            tracing::warn!(error = %err, "hygiene lint: cannot read the index");
            report.notes.push(LintWarning {
                path: String::new(),
                kind: "hygiene-skipped".into(),
                message: format!(
                    "orphan and duplicate checks were skipped: the search index could not be read ({err})"
                ),
            });
        }
    }
}

/// Advisory warning kinds that can hit many pages at once (every entity
/// page of an older vault lacks `sources`) or repeat on every commit until
/// a person decides. They are reported by `brain_lint_report`, the write
/// responses, the audit and the Integrity page, but the watcher leaves
/// them out of its after-commit toast ([`toast_warnings`]).
pub const QUIET_WARNING_KINDS: &[&str] = &[
    "missing-sources",
    "missing-summary",
    "alias-collision",
    "expired-but-linked",
];

/// The warnings the watcher shows in its after-commit toast: all but the
/// [`QUIET_WARNING_KINDS`].
pub fn toast_warnings(warnings: Vec<LintWarning>) -> Vec<LintWarning> {
    warnings
        .into_iter()
        .filter(|w| !QUIET_WARNING_KINDS.contains(&w.kind.as_str()))
        .collect()
}

/// Page types whose pages state facts and should name their `sources`
/// (Slice C, `missing-sources`).
const FACT_TYPES: &[&str] = &["entity", "concept"];

/// Today's local date as `YYYY-MM-DD` — the reference day of the
/// validity rules (`expired-but-linked`) and of `brain_query valid:now`.
pub fn today_local() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// True for a `YYYY-MM-DD`-shaped string (digits and dashes only; the
/// calendar is not checked). Dates of another shape are treated as absent
/// by the validity rules and by `brain_query valid:` (and reported as
/// `invalid-date`).
pub fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

/// True when the page's `valid_to` is a valid date before `today`
/// (`YYYY-MM-DD`; ISO dates compare correctly as strings).
pub fn is_expired(fm: &super::page::PageFrontmatter, today: &str) -> bool {
    fm.valid_to.as_deref().is_some_and(|d| is_iso_date(d) && d < today)
}

/// Runs the lint over `02_wiki/`.
pub fn lint(vault: &Path) -> WikiResult<LintReport> {
    lint_as_of(vault, &today_local())
}

/// [`lint`] with an explicit reference day (`YYYY-MM-DD`) for the
/// validity rules — the seam the tests use.
pub fn lint_as_of(vault: &Path, today: &str) -> WikiResult<LintReport> {
    let wiki = wiki_dir(vault);
    let mut by_id: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut all_pages: Vec<(PathBuf, ParsedPage)> = Vec::new();
    let mut errors: Vec<LintError> = Vec::new();

    for sub in WIKI_SUBDIRS {
        let dir = wiki.join(sub);
        if !dir.exists() {
            continue;
        }
        for entry in walk_md(&dir)? {
            let raw = std::fs::read_to_string(&entry)?;
            match parse(&raw) {
                Ok(parsed) => {
                    by_id
                        .entry(parsed.frontmatter.id.clone())
                        .or_default()
                        .push(entry.clone());
                    all_pages.push((entry, parsed));
                }
                Err(err) => errors.push(LintError {
                    path: entry.to_string_lossy().to_string(),
                    kind: "frontmatter".into(),
                    message: err.to_string(),
                }),
            }
        }
    }

    for (id, files) in &by_id {
        if files.len() > 1 {
            errors.push(LintError {
                path: files[0].to_string_lossy().to_string(),
                kind: "duplicate-id".into(),
                message: format!(
                    "id '{}' is used by {} pages",
                    id,
                    files.len()
                ),
            });
        }
    }

    let known_ids: HashSet<&String> = by_id.keys().collect();
    let mut warnings: Vec<LintWarning> = Vec::new();
    for (file, parsed) in &all_pages {
        for link in &parsed.wiki_links {
            // `[[id#heading]]` points at a section of `id`: resolve the
            // page part too. A bare `[[#heading]]` is an in-page anchor.
            // The raw link still counts first, so an id that itself
            // contains `#` keeps resolving.
            let target = link.split('#').next().unwrap_or(link).trim();
            let resolves = known_ids.contains(link)
                || target.is_empty()
                || known_ids.contains(&target.to_string());
            if !resolves {
                errors.push(LintError {
                    path: file.to_string_lossy().to_string(),
                    kind: "broken-link".into(),
                    message: format!("wiki link '[[{}]]' has no target page", link),
                });
            }
        }
        if parsed.frontmatter.title.is_none() {
            warnings.push(LintWarning {
                path: file.to_string_lossy().to_string(),
                kind: "missing-title".into(),
                message: "frontmatter has no 'title' field".into(),
            });
        }
        // Table-cell wikilink-alias collision. Markdown tables split
        // cells on `|`; the aliased wikilink form `[[id|alias]]`
        // shares that separator, so a line that is both a table row
        // *and* contains an aliased wikilink renders broken cells.
        // We approximate "table row" by the cheap heuristic
        // `trimmed line starts with '|'` — this catches the standard
        // GFM table syntax (header / separator / body rows all start
        // with `|`) without needing a full Markdown AST. False
        // positives on text that *looks* like a row but isn't (e.g. a
        // markdown blockquote-with-pipes) are acceptable: the
        // warning's fix advice (use un-aliased `[[id]]` form here)
        // is harmless even in that edge case.
        if body_has_aliased_wikilink_in_table_row(&parsed.body) {
            warnings.push(LintWarning {
                path: file.to_string_lossy().to_string(),
                kind: "wikilink-pipe-in-table-cell".into(),
                message:
                    "aliased wikilink `[[id|alias]]` found inside a Markdown table row — \
                     the `|` collides with the cell separator and breaks rendering. \
                     Use the un-aliased form `[[id]]` inside table cells, or move the \
                     reference out of the table."
                        .into(),
            });
        }
        // Type-registry check, promoted to Error in 0.2.17. The page
        // is still on disk (parse() accepted it, indexer ran), but
        // the auto-commit watcher will refuse to commit while any
        // page carries an unregistered type, and brain_write_page
        // surfaces this directly to the writing agent as a hard
        // failure on the next round-trip. The intent is to make
        // schema drift impossible to ignore — LLM agents that see
        // the explicit, actionable error in their write-response
        // converge on the correct singular form within one extra
        // call, instead of letting plural values accumulate silently.
        if !KNOWN_TYPES.contains(&parsed.frontmatter.page_type.as_str()) {
            errors.push(LintError {
                path: file.to_string_lossy().to_string(),
                kind: "unregistered-type".into(),
                message: format!(
                    "frontmatter type '{}' is not a registered page type. \
                     Valid types are singular: 'entity', 'concept', 'source', 'topic'. \
                     Fix: rewrite this page via brain_write_page with the corrected \
                     singular form in the YAML frontmatter — the directory name is \
                     plural (entities/, concepts/, ...) but the frontmatter type \
                     must be the singular. If the artifact does not fit any of the \
                     four categories, place it under 01_raw/ instead of inventing \
                     a fifth type — new types are a deliberate code change, not a \
                     per-page choice.",
                    parsed.frontmatter.page_type,
                ),
            });
        }
        // Soft warning: markdown-style links pointing at a wiki page
        // (e.g. `[Dan](entities/dan-shapiro)`) are tolerated by the
        // indexer but should be normalised to `[[wiki-link]]` form for
        // refactor-friendly grep and consistency with the rest of the
        // vault. The MCP `brain_write_page` tool auto-normalises before
        // write; this warning catches manual edits (VS Code, Obsidian
        // without the wiki-link plugin, etc.) that slipped through.
        let non_canonical = count_non_canonical_links(&parsed.body);
        if non_canonical > 0 {
            warnings.push(LintWarning {
                path: file.to_string_lossy().to_string(),
                kind: "non-canonical-wiki-link".into(),
                message: format!(
                    "{non_canonical} markdown link(s) point at wiki pages; \
                     prefer [[type/slug]] form. Run \"Rebuild index\" or \
                     re-save through brain_write_page to auto-normalise."
                ),
            });
        }
    }

    validity_rules(&all_pages, &known_ids, today, &mut errors, &mut warnings);
    alias_collisions(&all_pages, &mut warnings);

    Ok(LintReport {
        errors,
        warnings,
        notes: Vec::new(),
    })
}

/// Slice C: `dangling-supersede` and `supersede-cycle` (errors);
/// `missing-sources`, `broken-source`, `invalid-date` and
/// `expired-but-linked` (warnings).
fn validity_rules(
    pages: &[(PathBuf, ParsedPage)],
    known_ids: &HashSet<&String>,
    today: &str,
    errors: &mut Vec<LintError>,
    warnings: &mut Vec<LintWarning>,
) {
    // Inbound links from CURRENT pages (not expired, not superseded):
    // target id → linking ids.
    let mut current_linkers: HashMap<&str, Vec<&str>> = HashMap::new();
    // id → successor, for the cycle check.
    let mut successor_of: HashMap<&str, &str> = HashMap::new();
    for (_, parsed) in pages {
        let fm = &parsed.frontmatter;
        if let Some(s) = fm.superseded_by.as_deref() {
            successor_of.insert(fm.id.as_str(), s.split('#').next().unwrap_or(s).trim());
        }
        if is_expired(fm, today) || fm.superseded_by.is_some() {
            continue;
        }
        let mut targets: Vec<&str> = parsed
            .wiki_links
            .iter()
            .map(|l| l.split('#').next().unwrap_or(l).trim())
            .filter(|t| !t.is_empty() && *t != fm.id)
            .collect();
        targets.sort_unstable();
        targets.dedup();
        for t in targets {
            current_linkers.entry(t).or_default().push(fm.id.as_str());
        }
    }

    for (file, parsed) in pages {
        let fm = &parsed.frontmatter;
        let path = file.to_string_lossy().to_string();
        if let Some(successor) = fm.superseded_by.as_deref() {
            let target = successor.split('#').next().unwrap_or(successor).trim();
            if !known_ids.contains(&target.to_string()) {
                errors.push(LintError {
                    path: path.clone(),
                    kind: "dangling-supersede".into(),
                    message: format!(
                        "superseded_by '{successor}' points at a page that does not exist — \
                         write the successor page first, or fix the id"
                    ),
                });
            } else if let Some(chain) = supersede_cycle(fm.id.as_str(), &successor_of) {
                errors.push(LintError {
                    path: path.clone(),
                    kind: "supersede-cycle".into(),
                    message: format!(
                        "superseded_by forms a cycle ({}) — a page cannot (indirectly) replace \
                         itself; point superseded_by at the page that is current",
                        chain.join(" → ")
                    ),
                });
            }
        }
        if FACT_TYPES.contains(&fm.page_type.as_str()) && fm.sources.is_empty() {
            warnings.push(LintWarning {
                path: path.clone(),
                kind: "missing-sources".into(),
                message: format!(
                    "{} page without `sources` — list the source pages its facts come from \
                     (frontmatter `sources: [sources/…]`)",
                    fm.page_type
                ),
            });
        }
        if fm.summary.is_none() {
            warnings.push(LintWarning {
                path: path.clone(),
                kind: "missing-summary".into(),
                message: "page without `summary` — add one or two sentences saying what the page \
                          is about (frontmatter `summary: …`); search ranks summary hits higher \
                          and embeds every chunk with it"
                    .into(),
            });
        }
        for source in &fm.sources {
            if !known_ids.contains(source) {
                warnings.push(LintWarning {
                    path: path.clone(),
                    kind: "broken-source".into(),
                    message: format!(
                        "sources entry '{source}' has no page — fix the id, create the source \
                         page, or remove the entry"
                    ),
                });
            }
        }
        let mut bad_dates: Vec<String> = [("valid_from", &fm.valid_from), ("valid_to", &fm.valid_to)]
            .into_iter()
            .filter_map(|(key, v)| {
                v.as_deref()
                    .filter(|d| !is_iso_date(d))
                    .map(|d| format!("{key} '{d}' is not a YYYY-MM-DD date"))
            })
            .collect();
        if let (Some(from), Some(to)) = (fm.valid_from.as_deref(), fm.valid_to.as_deref()) {
            if is_iso_date(from) && is_iso_date(to) && from > to {
                bad_dates.push(format!("valid_from {from} is after valid_to {to}"));
            }
        }
        if !bad_dates.is_empty() {
            warnings.push(LintWarning {
                path: path.clone(),
                kind: "invalid-date".into(),
                message: format!(
                    "{} — until fixed, the validity filter ignores the date",
                    bad_dates.join("; ")
                ),
            });
        }
        if is_expired(fm, today) {
            if let Some(linkers) = current_linkers.get(fm.id.as_str()) {
                let mut linkers = linkers.clone();
                linkers.sort_unstable();
                let shown: Vec<&str> = linkers.iter().copied().take(5).collect();
                let more = linkers.len().saturating_sub(shown.len());
                let successor = fm
                    .superseded_by
                    .as_deref()
                    .map(|s| format!(" — point them at '{s}'"))
                    .unwrap_or_else(|| {
                        " — point them at the current page, or set `superseded_by`".to_string()
                    });
                warnings.push(LintWarning {
                    path,
                    kind: "expired-but-linked".into(),
                    message: format!(
                        "valid_to {} has passed, but {} current page(s) still link here: {}{}{successor}",
                        fm.valid_to.as_deref().unwrap_or_default(),
                        linkers.len(),
                        shown.join(", "),
                        if more > 0 { format!(" and {more} more") } else { String::new() },
                    ),
                });
            }
        }
    }
}

/// The supersede chain starting at `start` when it leads back to `start`
/// (self-supersede, A → B → A, …), else `None`.
fn supersede_cycle<'a>(start: &'a str, successor_of: &HashMap<&'a str, &'a str>) -> Option<Vec<&'a str>> {
    let mut chain = vec![start];
    let mut seen: HashSet<&str> = HashSet::from([start]);
    let mut current = start;
    while let Some(&next) = successor_of.get(current) {
        chain.push(next);
        if next == start {
            return Some(chain);
        }
        if !seen.insert(next) {
            return None; // a cycle further down the chain, not through `start`
        }
        current = next;
    }
    None
}

/// A2: `alias-collision` — two pages of one type directory share a name
/// through an alias or an identical slug (see
/// [`super::duplicates::name_collisions`]; `distinct_from` opts a pair
/// out). Reported on both pages.
fn alias_collisions(pages: &[(PathBuf, ParsedPage)], warnings: &mut Vec<LintWarning>) {
    let entries: Vec<super::duplicates::NameEntry> = pages
        .iter()
        .map(|(_, p)| super::duplicates::NameEntry {
            id: p.frontmatter.id.clone(),
            title: p.frontmatter.title.clone(),
            aliases: p.frontmatter.aliases.clone(),
            distinct_from: p.frontmatter.distinct_from.clone(),
        })
        .collect();
    let mut others_of: HashMap<&str, Vec<&str>> = HashMap::new();
    let pairs = super::duplicates::name_collisions(&entries);
    for (a, b) in &pairs {
        others_of.entry(a.as_str()).or_default().push(b.as_str());
        others_of.entry(b.as_str()).or_default().push(a.as_str());
    }
    for (file, parsed) in pages {
        let id = parsed.frontmatter.id.as_str();
        let Some(others) = others_of.get(id) else {
            continue;
        };
        warnings.push(LintWarning {
            path: file.to_string_lossy().to_string(),
            kind: "alias-collision".into(),
            message: format!(
                "'{id}' has the same name as {} (via id or alias) — if they are the same \
                 thing, fold one into the other with brain_refactor (action merge); if they are different, \
                 change the clashing alias or add `distinct_from: [<other id>]`",
                others.iter().map(|o| format!("'{o}'")).collect::<Vec<_>>().join(", ")
            ),
        });
    }
}

/// True iff any line of `body` looks like a Markdown table row
/// (starts with `|` after trimming whitespace) *and* contains an
/// aliased wikilink (`[[...|...]]`). The two together break GFM
/// table rendering because the alias-pipe is consumed by the cell
/// splitter. Plain `[[id]]` (un-aliased) inside a table row is fine
/// — only the pipe-carrying form is flagged.
fn body_has_aliased_wikilink_in_table_row(body: &str) -> bool {
    let aliased = Regex::new(r"\[\[[^\]]+\|[^\]]+\]\]").expect("regex");
    body.lines()
        .any(|line| line.trim_start().starts_with('|') && aliased.is_match(line))
}

fn count_non_canonical_links(body: &str) -> usize {
    let re = Regex::new(
        r#"\[(?:[^\]]*)\]\(\s*([^)\s]+)(?:\s+"[^"]*")?\s*\)"#,
    )
    .expect("regex");
    re.captures_iter(body)
        .filter(|cap| {
            let target = cap[1].trim();
            super::page::looks_like_wiki_page_target(target)
        })
        .count()
}

fn walk_md(dir: &Path) -> WikiResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    visit(dir, &mut out)?;
    Ok(out)
}

fn visit(dir: &Path, out: &mut Vec<PathBuf>) -> WikiResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            visit(&p, out)?;
        } else if p.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(p);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::ensure_skeleton;
    use tempfile::TempDir;

    fn write_page(vault: &Path, sub: &str, slug: &str, body: &str) {
        let dir = wiki_dir(vault).join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{slug}.md")), body).unwrap();
    }

    fn make_vault() -> TempDir {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        tmp
    }

    fn page(id: &str, body: &str) -> String {
        format!(
            "---\nid: {id}\ntype: entity\ntitle: t\ncreated: 2026-04-29\nupdated: 2026-04-29\n---\n\n{body}\n"
        )
    }

    #[test]
    fn lint_accepts_well_formed_pages_without_errors() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "alice", &page("entities/alice", "hi"));
        let report = lint(tmp.path()).unwrap();
        assert!(report.is_clean(), "expected clean, got {:?}", report.errors);
    }

    #[test]
    fn lint_rejects_duplicate_page_ids() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "alice1", &page("entities/alice", "hi"));
        write_page(tmp.path(), "entities", "alice2", &page("entities/alice", "hi"));
        let report = lint(tmp.path()).unwrap();
        assert!(report.errors.iter().any(|e| e.kind == "duplicate-id"));
    }

    #[test]
    fn lint_flags_broken_wiki_links_when_target_does_not_exist() {
        let tmp = make_vault();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            &page("entities/alice", "see [[entities/missing]]"),
        );
        let report = lint(tmp.path()).unwrap();
        assert!(report.errors.iter().any(|e| e.kind == "broken-link"));
    }

    #[test]
    fn lint_resolves_a_link_to_a_heading_of_an_existing_page() {
        let tmp = make_vault();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            &page("entities/alice", "see [[entities/bob#Contract]]"),
        );
        write_page(tmp.path(), "entities", "bob", &page("entities/bob", "hi"));
        let report = lint(tmp.path()).unwrap();
        assert!(report.is_clean(), "unexpected errors: {:?}", report.errors);
    }

    #[test]
    fn lint_resolves_a_link_to_an_existing_id_that_contains_a_hash() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "alice", &page("entities/alice", "see [[entities/c#]]"));
        write_page(tmp.path(), "entities", "c-sharp", &page("entities/c#", "hi"));
        let report = lint(tmp.path()).unwrap();
        assert!(report.is_clean(), "unexpected errors: {:?}", report.errors);
    }

    #[test]
    fn lint_flags_a_heading_link_whose_page_does_not_exist() {
        let tmp = make_vault();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            &page("entities/alice", "see [[entities/missing#Contract]]"),
        );
        let report = lint(tmp.path()).unwrap();
        assert!(report.errors.iter().any(|e| e.kind == "broken-link"));
    }

    #[test]
    fn lint_without_index_serialises_no_notes_field() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "alice", &page("entities/alice", "hi"));
        let json = serde_json::to_value(lint(tmp.path()).unwrap()).unwrap();
        assert!(json.get("notes").is_none());
    }

    #[test]
    fn lint_accepts_well_formed_links_when_target_exists() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "alice", &page("entities/alice", "see [[entities/bob]]"));
        write_page(tmp.path(), "entities", "bob", &page("entities/bob", "hi"));
        let report = lint(tmp.path()).unwrap();
        assert!(report.is_clean());
    }

    #[test]
    fn lint_reports_frontmatter_errors_per_file() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "broken", "no frontmatter here");
        let report = lint(tmp.path()).unwrap();
        assert!(report.errors.iter().any(|e| e.kind == "frontmatter"));
    }

    #[test]
    fn lint_warns_about_non_canonical_markdown_wiki_links() {
        let tmp = make_vault();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            &page("entities/alice", "see [Bob](entities/bob)"),
        );
        write_page(
            tmp.path(),
            "entities",
            "bob",
            &page("entities/bob", "hi"),
        );
        let report = lint(tmp.path()).unwrap();
        // Lint must NOT block the commit (warnings only).
        assert!(report.is_clean());
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.kind == "non-canonical-wiki-link"),
            "missing non-canonical-wiki-link warning: {:#?}",
            report.warnings
        );
    }

    /// Helper: writes a page with an arbitrary `type:` field, so the type-
    /// registry tests can exercise registered and unregistered values
    /// without the standard `page()` helper forcing `entity` on them.
    fn page_with_type(id: &str, page_type: &str, body: &str) -> String {
        format!(
            "---\nid: {id}\ntype: {page_type}\ntitle: t\ncreated: 2026-04-29\nupdated: 2026-04-29\n---\n\n{body}\n"
        )
    }

    #[test]
    fn lint_warns_when_aliased_wikilink_appears_in_a_markdown_table_cell() {
        // Real-world trap: Markdown tables use `|` as cell separator,
        // but the aliased wiki-link form `[[entities/foo|Display]]`
        // also uses `|` between the target and the alias. When an
        // agent writes a comparison table like
        //   | Person | Role |
        //   |--------|------|
        //   | [[entities/dan|Dan]] | CEO |
        // the table parser splits the cell at the alias-pipe and
        // renders broken cells. The fix is for the agent to use the
        // un-aliased form `[[entities/dan]]` (or escape) inside table
        // cells — this lint surfaces the issue at write time so the
        // agent can switch to the safe form without the user
        // discovering broken rendering later.
        let tmp = make_vault();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            &page(
                "entities/alice",
                "| col1 | col2 |\n|------|------|\n| [[entities/bob|Bob]] | yes |\n",
            ),
        );
        write_page(
            tmp.path(),
            "entities",
            "bob",
            &page("entities/bob", "hi"),
        );
        let report = lint(tmp.path()).unwrap();
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.kind == "wikilink-pipe-in-table-cell"),
            "expected wikilink-pipe-in-table-cell warning, got: {:#?}",
            report.warnings
        );
        // It is a warning, not an error — auto-commit must keep running.
        assert!(report.is_clean(), "must not block commits: {:#?}", report.errors);
    }

    #[test]
    fn lint_does_not_warn_for_aliased_wikilinks_outside_tables() {
        // Sanity: outside a table-row context the aliased form is
        // perfectly fine and is exactly what `normalize_internal_links`
        // produces for `[Text](path)` rewrites. We must not double-
        // flag every alias in the vault.
        let tmp = make_vault();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            &page(
                "entities/alice",
                "Some prose linking to [[entities/bob|Bob]] inline.\n",
            ),
        );
        write_page(
            tmp.path(),
            "entities",
            "bob",
            &page("entities/bob", "hi"),
        );
        let report = lint(tmp.path()).unwrap();
        assert!(
            report
                .warnings
                .iter()
                .all(|w| w.kind != "wikilink-pipe-in-table-cell"),
            "inline aliased links must not trigger the table-cell warning: {:#?}",
            report.warnings
        );
    }

    #[test]
    fn lint_errors_when_frontmatter_type_is_not_in_registered_set() {
        // Promoted from Warning to Error in 0.2.17. The user's
        // experience with the warning-level rule was that schema
        // drift kept accumulating because agents could ignore the
        // signal. Error severity flips that: drift blocks the auto-
        // commit and the agent gets an explicit, actionable failure
        // in its write-response, which it can correct on the next
        // call. The trade-off — a stale unregistered-type anywhere
        // in the vault blocks ALL auto-commits until fixed — is
        // accepted: schema integrity over commit availability.
        let tmp = make_vault();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            &page_with_type("entities/alice", "entities", "hi"),
        );
        let report = lint(tmp.path()).unwrap();
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.kind == "unregistered-type"),
            "expected an unregistered-type error, got errors = {:#?}",
            report.errors
        );
    }

    #[test]
    fn lint_blocks_commit_on_unregistered_type_error() {
        // Companion to the rule above: the auto-commit watcher uses
        // `report.is_clean()` to decide whether to commit, and the
        // MCP write_page tool surfaces page-scoped errors as a hard
        // failure. Both paths key off the Error severity — verify it
        // here so a future "soften to warning again" refactor
        // immediately trips this test.
        let tmp = make_vault();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            &page_with_type("entities/alice", "entities", "hi"),
        );
        let report = lint(tmp.path()).unwrap();
        assert!(
            !report.is_clean(),
            "unregistered type must produce an Error that blocks the commit, not a passive warning"
        );
    }

    #[test]
    fn lint_unregistered_type_error_message_names_the_valid_singular_forms_and_the_fix() {
        // The error message is the *only* thing an LLM agent sees on
        // failure. It has to spell out exactly what the registered
        // singular forms are, and how to fix the page in one round-
        // trip — otherwise the agent burns calls guessing. This
        // test pins the message contract so a future code cleanup
        // doesn't trim away the actionable parts by accident.
        let tmp = make_vault();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            &page_with_type("entities/alice", "entities", "hi"),
        );
        let report = lint(tmp.path()).unwrap();
        let msg = report
            .errors
            .iter()
            .find(|e| e.kind == "unregistered-type")
            .map(|e| e.message.as_str())
            .expect("unregistered-type error present");
        // The offending value must appear, so the LLM knows which
        // page-write was the culprit.
        assert!(msg.contains("entities"), "message must name the offending value: {msg}");
        // The four singular forms must be listed — otherwise the
        // agent has to fetch them from somewhere else.
        for t in &["entity", "concept", "source", "topic"] {
            assert!(msg.contains(t), "valid type '{t}' must be listed in error message: {msg}");
        }
    }

    #[test]
    fn lint_accepts_all_registered_types_without_unregistered_warning() {
        let tmp = make_vault();
        // One page per canonical singular type — none of them should
        // trip the new rule. Other warnings (missing-title etc.) are
        // not under test here.
        write_page(
            tmp.path(),
            "entities",
            "a",
            &page_with_type("entities/a", "entity", "x"),
        );
        write_page(
            tmp.path(),
            "concepts",
            "b",
            &page_with_type("concepts/b", "concept", "x"),
        );
        write_page(
            tmp.path(),
            "sources",
            "c",
            &page_with_type("sources/c", "source", "x"),
        );
        write_page(
            tmp.path(),
            "topics",
            "d",
            &page_with_type("topics/d", "topic", "x"),
        );
        let report = lint(tmp.path()).unwrap();
        assert!(
            report
                .warnings
                .iter()
                .all(|w| w.kind != "unregistered-type"),
            "no unregistered-type warning expected for canonical types, \
             got: {:#?}",
            report.warnings
        );
    }

    #[test]
    fn lint_does_not_warn_for_external_or_canonical_links() {
        let tmp = make_vault();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            &page(
                "entities/alice",
                "external [GitHub](https://github.com/x), \
                 canonical [[entities/bob]]",
            ),
        );
        write_page(tmp.path(), "entities", "bob", &page("entities/bob", "hi"));
        let report = lint(tmp.path()).unwrap();
        assert!(report
            .warnings
            .iter()
            .all(|w| w.kind != "non-canonical-wiki-link"));
    }

    // ---- A2 / C ------------------------------------------------------------

    /// A page with extra frontmatter lines (each ending in a newline).
    fn page_with(id: &str, page_type: &str, extra: &str, body: &str) -> String {
        format!("---\nid: {id}\ntype: {page_type}\ntitle: t\n{extra}---\n\n{body}\n")
    }

    fn kinds_for(report: &LintReport, kind: &str) -> usize {
        report.errors.iter().filter(|e| e.kind == kind).count()
            + report.warnings.iter().filter(|w| w.kind == kind).count()
    }

    const TODAY: &str = "2026-10-06";

    #[test]
    fn two_pages_sharing_a_name_through_an_alias_get_an_alias_collision_warning_each() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "acme", &page_with("entities/acme", "entity", "aliases: [ACME Corp]\n", "x"));
        write_page(tmp.path(), "entities", "acme-corp", &page_with("entities/acme-corp", "entity", "", "y"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "alias-collision"), 2);
    }

    #[test]
    fn a_superseded_by_pointing_at_a_missing_page_is_a_dangling_supersede_error() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "a", &page_with("entities/a", "entity", "superseded_by: entities/gone\n", "x"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert!(report.errors.iter().any(|e| e.kind == "dangling-supersede"));
    }

    #[test]
    fn a_superseded_by_pointing_at_an_existing_page_is_not_an_error() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "a", &page_with("entities/a", "entity", "superseded_by: entities/b\n", "x"));
        write_page(tmp.path(), "entities", "b", &page_with("entities/b", "entity", "", "y"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "dangling-supersede"), 0);
    }

    #[test]
    fn an_entity_page_without_sources_gets_a_missing_sources_warning() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "a", &page_with("entities/a", "entity", "", "x"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert!(report.warnings.iter().any(|w| w.kind == "missing-sources"));
    }

    #[test]
    fn a_topic_page_without_sources_gets_no_missing_sources_warning() {
        let tmp = make_vault();
        write_page(tmp.path(), "topics", "t", &page_with("topics/t", "topic", "", "x"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "missing-sources"), 0);
    }

    #[test]
    fn missing_sources_never_blocks_a_commit() {
        let tmp = make_vault();
        write_page(tmp.path(), "concepts", "c", &page_with("concepts/c", "concept", "", "x"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert!(report.is_clean(), "unexpected errors: {:?}", report.errors);
    }

    #[test]
    fn an_expired_page_linked_from_a_current_page_gets_an_expired_but_linked_warning() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "old", &page_with("entities/old", "entity", "valid_to: 2025-12-31\n", "x"));
        write_page(tmp.path(), "entities", "cur", &page_with("entities/cur", "entity", "", "see [[entities/old]]"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "expired-but-linked"), 1);
    }

    #[test]
    fn an_expired_page_linked_only_from_expired_pages_gets_no_expired_but_linked_warning() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "old", &page_with("entities/old", "entity", "valid_to: 2025-12-31\n", "x"));
        write_page(
            tmp.path(),
            "entities",
            "older",
            &page_with("entities/older", "entity", "valid_to: 2024-12-31\n", "see [[entities/old]]"),
        );
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "expired-but-linked"), 0);
    }

    #[test]
    fn a_page_valid_until_today_is_not_expired() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "old", &page_with("entities/old", "entity", "valid_to: 2026-10-06\n", "x"));
        write_page(tmp.path(), "entities", "cur", &page_with("entities/cur", "entity", "", "see [[entities/old]]"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "expired-but-linked"), 0);
    }

    // ---- review fixes: dates, cycles, sources, collisions, toast ----------

    #[test]
    fn a_sources_entry_without_a_page_gets_a_broken_source_warning() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "a", &page_with("entities/a", "entity", "sources: [sources/gone]\n", "x"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "broken-source"), 1);
    }

    #[test]
    fn a_valid_to_that_is_not_an_iso_date_gets_an_invalid_date_warning() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "a", &page_with("entities/a", "entity", "valid_to: end of 2025\n", "x"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "invalid-date"), 1);
    }

    #[test]
    fn a_valid_from_after_valid_to_gets_an_invalid_date_warning() {
        let tmp = make_vault();
        write_page(
            tmp.path(),
            "entities",
            "a",
            &page_with("entities/a", "entity", "valid_from: 2026-02-01\nvalid_to: 2026-01-01\n", "x"),
        );
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "invalid-date"), 1);
    }

    #[test]
    fn an_invalid_valid_to_does_not_make_a_page_expired() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "old", &page_with("entities/old", "entity", "valid_to: 1999\n", "x"));
        write_page(tmp.path(), "entities", "cur", &page_with("entities/cur", "entity", "", "see [[entities/old]]"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "expired-but-linked"), 0);
    }

    #[test]
    fn a_page_superseded_by_itself_is_a_supersede_cycle_error() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "a", &page_with("entities/a", "entity", "superseded_by: entities/a\n", "x"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert!(report.errors.iter().any(|e| e.kind == "supersede-cycle"));
    }

    #[test]
    fn two_pages_superseding_each_other_are_both_supersede_cycle_errors() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "a", &page_with("entities/a", "entity", "superseded_by: entities/b\n", "x"));
        write_page(tmp.path(), "entities", "b", &page_with("entities/b", "entity", "superseded_by: entities/a\n", "y"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "supersede-cycle"), 2);
    }

    #[test]
    fn a_supersede_chain_without_a_cycle_is_no_error() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "a", &page_with("entities/a", "entity", "superseded_by: entities/b\n", "x"));
        write_page(tmp.path(), "entities", "b", &page_with("entities/b", "entity", "superseded_by: entities/c\n", "y"));
        write_page(tmp.path(), "entities", "c", &page_with("entities/c", "entity", "", "z"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "supersede-cycle"), 0);
    }

    #[test]
    fn distinct_from_silences_the_alias_collision_lint() {
        let tmp = make_vault();
        write_page(
            tmp.path(),
            "entities",
            "acme",
            &page_with("entities/acme", "entity", "aliases: [ACME Corp]\ndistinct_from: [entities/acme-corp]\n", "x"),
        );
        write_page(tmp.path(), "entities", "acme-corp", &page_with("entities/acme-corp", "entity", "", "y"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "alias-collision"), 0);
    }

    #[test]
    fn two_ids_that_only_match_after_umlaut_folding_get_no_alias_collision_warning() {
        let tmp = make_vault();
        write_page(tmp.path(), "entities", "koehler", &page_with("entities/koehler", "entity", "", "x"));
        write_page(tmp.path(), "entities", "kohler", &page_with("entities/kohler", "entity", "", "y"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "alias-collision"), 0);
    }

    #[test]
    fn the_commit_toast_leaves_out_the_quiet_warning_kinds() {
        let w = |kind: &str| LintWarning {
            path: "p".into(),
            kind: kind.into(),
            message: "m".into(),
        };
        let shown: Vec<String> = toast_warnings(vec![
            w("missing-sources"),
            w("alias-collision"),
            w("expired-but-linked"),
            w("missing-summary"),
            w("missing-title"),
        ])
        .into_iter()
        .map(|w| w.kind)
        .collect();
        assert_eq!(shown, vec!["missing-title"]);
    }

    #[test]
    fn a_page_without_a_summary_gets_a_missing_summary_warning() {
        let tmp = make_vault();
        write_page(tmp.path(), "topics", "t", &page_with("topics/t", "topic", "", "x"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "missing-summary"), 1);
    }

    #[test]
    fn a_page_with_a_summary_gets_no_missing_summary_warning() {
        let tmp = make_vault();
        write_page(tmp.path(), "topics", "t", &page_with("topics/t", "topic", "summary: About t.\n", "x"));
        let report = lint_as_of(tmp.path(), TODAY).unwrap();
        assert_eq!(kinds_for(&report, "missing-summary"), 0);
    }

    #[test]
    fn missing_summary_is_a_quiet_warning_kind() {
        assert!(QUIET_WARNING_KINDS.contains(&"missing-summary"));
    }

    #[test]
    fn missing_summary_never_blocks_a_commit() {
        let tmp = make_vault();
        write_page(tmp.path(), "topics", "t", &page_with("topics/t", "topic", "", "x"));
        assert!(lint_as_of(tmp.path(), TODAY).unwrap().is_clean());
    }
}
