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
        // Type-registry check. Pure warning: parse() already accepted
        // the page and the indexer has indexed it. We only flag that
        // the value isn't one of the canonical singular forms so a
        // follow-up brain_write_page can correct it. No error path —
        // see `lint_does_not_block_commit_on_unregistered_type_warning`.
        if !KNOWN_TYPES.contains(&parsed.frontmatter.page_type.as_str()) {
            warnings.push(LintWarning {
                path: file.to_string_lossy().to_string(),
                kind: "unregistered-type".into(),
                message: format!(
                    "frontmatter type '{}' is not in the registered set {:?}; \
                     change it to one of those, or place the artifact under \
                     01_raw/ if it doesn't fit any wiki category",
                    parsed.frontmatter.page_type, KNOWN_TYPES,
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
    fn lint_warns_when_frontmatter_type_is_not_in_registered_set() {
        let tmp = make_vault();
        // `entities` (plural) is not in KNOWN_TYPES — exactly the
        // schema-drift case the user hit when an MCP agent matched the
        // type-field to the directory name instead of the canonical
        // singular form.
        write_page(
            tmp.path(),
            "entities",
            "alice",
            &page_with_type("entities/alice", "entities", "hi"),
        );
        let report = lint(tmp.path()).unwrap();
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.kind == "unregistered-type"),
            "expected an unregistered-type warning, got warnings = {:#?}",
            report.warnings
        );
    }

    #[test]
    fn lint_does_not_block_commit_on_unregistered_type_warning() {
        let tmp = make_vault();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            &page_with_type("entities/alice", "entities", "hi"),
        );
        let report = lint(tmp.path()).unwrap();
        // The rule is a Warning, not an Error — auto-commit must keep
        // running so the user can correct the field via the next edit
        // or have an agent fix it via brain_write_page. Blocking would
        // strand the page in a half-saved state.
        assert!(
            report.is_clean(),
            "unregistered type must not produce an Error: {:#?}",
            report.errors
        );
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
