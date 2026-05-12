//! Lint pass over the wiki: frontmatter validity, link integrity, ID
//! uniqueness. Run before each auto-commit. Hard errors block the commit.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

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

/// Runs the lint over `02_wiki/`.
pub fn lint(vault: &Path) -> WikiResult<LintReport> {
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
            if !known_ids.contains(link) {
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

    Ok(LintReport { errors, warnings })
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
}
