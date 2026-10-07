//! Page refactoring — rename, delete and merge wiki pages with vault-wide
//! link maintenance.
//!
//! Before this module a page created under a wrong id was permanent: the
//! MCP toolset could create, patch and restore pages but never move or
//! remove one, and every `[[link]]` kept pointing at the bad id. The three
//! operations here are the shared core behind the MCP tool
//! `brain_refactor` (actions rename, delete and merge); its arms are thin.
//!
//! Invariants every operation keeps:
//!
//! - **Ids never escape the wiki.** Every id — existing or new — passes
//!   [`validate_page_id`] (relative `<type-dir>/<slug>`, no drive letters,
//!   `..`, backslashes or control characters) before any filesystem
//!   access, and every resolved path is checked to lie under the wiki
//!   directory. The MCP server applies the same validator (via
//!   `check_page_id`) to every arm that resolves a page id:
//!   `brain_get_pages` (per id), `brain_lookup`, `brain_write_page`,
//!   `brain_write_batch` (per page), `brain_patch_page`, `brain_history`
//!   (list and restore) and `brain_refactor` (rename, merge, delete).
//! - **Every file location goes through the id→path resolver**
//!   ([`page_relpath_with_store`]). An encrypted vault stores pages under
//!   opaque HMAC filenames derived from the id, so a rename moves the file
//!   to a NEW opaque path — paths are never computed by hand.
//! - **Ids are case-sensitive, disks may not be.** On exFAT/NTFS/APFS two
//!   ids differing only in case resolve to the same file, so every page
//!   that is read is checked against its frontmatter id, and a case-only
//!   rename is carried out through a temporary name.
//! - **Link rewriting is textual and byte-preserving.** Only the matched
//!   link is replaced; surrounding text, frontmatter and line endings stay
//!   exactly as they were, and only pages that actually change are
//!   written. Both link syntaxes the indexer counts as edges are handled:
//!   `[[id]]` / `[[id|alias]]` and the markdown form `[text](id)` (also as
//!   an image, `![alt](id)`). Matching is on the exact id, so
//!   `[[entities/old-2]]` is never touched by a rename of `entities/old`.
//!   Rewriting happens everywhere in the body — code fences included —
//!   because [`extract_wiki_links`] also counts links inside code fences.
//!   Heading anchors (`[[id#section]]`) are not supported: the link
//!   extractor does not resolve them either.
//! - **Nothing is lost.** Before the first change, any uncommitted work in
//!   the wiki is committed as a path-free checkpoint, so everything a
//!   delete or merge removes is in git history and can be brought back
//!   with `brain_history` (action restore). The file changes themselves run as a small
//!   journal: if one fails, the ones already made are undone.
//! - **One operation, one commit** through [`commit_wiki_with_store`], the
//!   single commit entry point (plus the checkpoint when there was pending
//!   work). On an encrypted vault the commit message is path-free (it gets
//!   pushed), mirroring the watcher. If the commit itself fails after the
//!   files were changed, the operation still reports success with
//!   `commit: null` and a `note` — the changes are on disk and the watcher
//!   commits them later, so an agent must not retry.

use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde::Serialize;
use thiserror::Error;

use crate::crypto::keychain::{KeyringStore, MasterKeyStore};
use crate::vault::layout::{WIKI_SUBDIRS, wiki_dir};

use super::WikiError;
use super::encryption::{commit_wiki_with_store, is_encrypted, page_relpath_with_store};
use super::page::{extract_wiki_links, page_id_from_markdown_target, parse};

#[derive(Debug, Error)]
pub enum RefactorError {
    #[error("invalid page id '{id}': {reason}")]
    InvalidId { id: String, reason: String },

    #[error("page not found: {0}")]
    NotFound(String),

    #[error(
        "no page with id '{requested}': the file at that location belongs to '{found}' (page \
         ids are case-sensitive)"
    )]
    IdMismatch { requested: String, found: String },

    #[error(
        "a page with id '{0}' already exists — choose a different id, or use brain_refactor (action merge) \
         to fold one page into the other"
    )]
    TargetExists(String),

    #[error("cannot merge page '{0}' into itself — from_id and into_id must differ")]
    SameId(String),

    #[error(
        "'{a}' and '{b}' are the same file on this disk (the ids differ only in letter case) — \
         use brain_refactor (action rename) to fix the case instead of merging"
    )]
    SameFile { a: String, b: String },

    #[error(
        "page '{id}' is still linked (or named in superseded_by / sources) from {count} page(s): {list}. Rename it \
         (brain_refactor, action rename) if only its id is wrong, merge it into the right page \
         (brain_refactor, action merge) if it is a duplicate, or pass force=true to delete it anyway and \
         turn those links into plain text",
        count = referrers.len(),
        list = referrers.join(", ")
    )]
    StillLinked { id: String, referrers: Vec<String> },

    #[error("page '{id}' cannot be refactored: {reason}")]
    Malformed { id: String, reason: String },

    #[error(transparent)]
    Wiki(#[from] WikiError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type RefactorResult<T> = Result<T, RefactorError>;

/// Result of [`rename_page`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RenameOutcome {
    pub old_id: String,
    pub new_id: String,
    /// Ids (after the rename) of the pages whose links were rewritten.
    pub rewritten_pages: Vec<String>,
    pub rewritten_links: usize,
    /// `superseded_by` / `sources` frontmatter entries pointed at the new id.
    pub rewritten_references: usize,
    /// The commit sha, or `None` if git saw nothing to commit or the
    /// commit failed (then `note` says so).
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Result of [`delete_page`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DeleteOutcome {
    pub deleted: String,
    /// Ids of the pages whose links to the deleted page became plain text.
    pub defused_in: Vec<String>,
    pub defused_links: usize,
    /// `superseded_by` / `sources` frontmatter entries naming the deleted
    /// page that were removed (only with `force`).
    pub removed_references: usize,
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Result of [`merge_pages`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MergeOutcome {
    pub from_id: String,
    pub into_id: String,
    /// Ids of the pages (other than the two merged ones) whose links to
    /// `from_id` were redirected to `into_id`.
    pub rewritten_pages: Vec<String>,
    pub rewritten_links: usize,
    /// `superseded_by` / `sources` frontmatter entries pointed at `into_id`.
    pub rewritten_references: usize,
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Wiki type directory → the singular `type:` value the lint requires.
/// Must list exactly `WIKI_SUBDIRS` / `lint::KNOWN_TYPES` (a test pins it).
const TYPE_DIRS: &[(&str, &str)] = &[
    ("entities", "entity"),
    ("concepts", "concept"),
    ("sources", "source"),
    ("topics", "topic"),
];

/// Message of the commit that preserves pending work before a refactor.
/// Path-free on purpose — it may be pushed from an encrypted vault.
pub const CHECKPOINT_MESSAGE: &str = "wiki: checkpoint before refactor";

// ---------------------------------------------------------------------------
// Public entry points (real OS keychain)
// ---------------------------------------------------------------------------

/// Rename page `old_id` to `new_id`: move the file to the resolved path of
/// `new_id`, set its frontmatter `id` (and `type`, when the type directory
/// changes), and rewrite every link to `old_id` in every page of the
/// vault. One commit.
pub fn rename_page(vault: &Path, old_id: &str, new_id: &str) -> RefactorResult<RenameOutcome> {
    rename_page_with_store(vault, old_id, new_id, &KeyringStore)
}

/// Delete page `id`. Refuses while other pages link to it unless `force`
/// is set; with `force` those links are turned into plain text. One
/// commit; the content stays recoverable from git history.
pub fn delete_page(vault: &Path, id: &str, force: bool) -> RefactorResult<DeleteOutcome> {
    delete_page_with_store(vault, id, force, &KeyringStore)
}

/// Merge page `from_id` into `into_id`: append the source body under a
/// `## Merged from <from_id>` heading, union the tags, redirect every link
/// to `from_id` to `into_id` and remove the source file. One commit.
pub fn merge_pages(vault: &Path, from_id: &str, into_id: &str) -> RefactorResult<MergeOutcome> {
    merge_pages_with_store(vault, from_id, into_id, &KeyringStore)
}

// ---------------------------------------------------------------------------
// Implementations (injectable key store for tests)
// ---------------------------------------------------------------------------

pub(crate) fn rename_page_with_store(
    vault: &Path,
    old_id: &str,
    new_id: &str,
    store: &impl MasterKeyStore,
) -> RefactorResult<RenameOutcome> {
    validate_page_id(old_id)?;
    validate_new_page_id(new_id)?;
    if old_id == new_id {
        return Err(RefactorError::InvalidId {
            id: new_id.to_string(),
            reason: "new_id is the same as the current id".to_string(),
        });
    }
    checkpoint(vault, store)?;

    let src_path = resolve(vault, old_id, store)?;
    let src_raw = read_page(&src_path, old_id)?;
    let src = parse(&src_raw).map_err(|e| malformed(old_id, e))?;
    expect_id(old_id, &src.frontmatter.id)?;

    let dst_path = resolve(vault, new_id, store)?;
    // On a case-insensitive disk a case-only rename resolves to the very
    // file being renamed — that is not a conflict.
    let dst_taken = dst_path.exists() && !same_file(&dst_path, &src_path);
    let pages = collect_pages(vault)?;
    if dst_taken || pages.iter().any(|p| p.id == new_id) {
        return Err(RefactorError::TargetExists(new_id.to_string()));
    }

    // New frontmatter: id always, type only when the type directory moved.
    let mut moved = set_frontmatter_scalar(&src_raw, "id", new_id)
        .ok_or_else(|| malformed(old_id, "frontmatter could not be located"))?;
    let old_dir = old_id.split_once('/').map(|(d, _)| d);
    let new_dir = new_id.split_once('/').map(|(d, _)| d);
    if old_dir != new_dir {
        if let Some(singular) = new_dir.and_then(singular_type) {
            moved = set_frontmatter_scalar(&moved, "type", singular)
                .ok_or_else(|| malformed(old_id, "frontmatter could not be located"))?;
        }
    }
    let (moved, self_links) = rewrite_page_links(&moved, old_id, &LinkAction::Retarget(new_id));
    let (moved, self_refs) =
        rewrite_frontmatter_refs(&moved, new_id, old_id, RefAction::Retarget(new_id));
    verify_frontmatter_id(&moved, new_id)?;

    let mut rewritten_pages: Vec<String> = Vec::new();
    let mut rewritten_links = self_links;
    let mut rewritten_references = self_refs;
    if self_links + self_refs > 0 {
        rewritten_pages.push(new_id.to_string());
    }
    let mut ops = vec![
        Op::Move {
            from: src_path.clone(),
            to: dst_path.clone(),
        },
        Op::Write {
            path: dst_path,
            contents: moved,
        },
    ];
    for p in pages.iter().filter(|p| !same_file(&p.path, &src_path)) {
        let (new_raw, n) = rewrite_page_links(&p.raw, old_id, &LinkAction::Retarget(new_id));
        let (new_raw, r) =
            rewrite_frontmatter_refs(&new_raw, &p.id, old_id, RefAction::Retarget(new_id));
        if n + r > 0 {
            rewritten_links += n;
            rewritten_references += r;
            rewritten_pages.push(p.id.clone());
            ops.push(Op::Write {
                path: p.path.clone(),
                contents: new_raw,
            });
        }
    }
    apply_journaled(ops)?;

    // A case-only rename on a case-insensitive disk would otherwise keep
    // the old spelling in the git index forever (git's `ignorecase`
    // matches the renamed file to the old entry). Dropping the old entry
    // makes the commit record the new name.
    forget_index_entry(vault, &src_path);
    let message = commit_message(vault, "rename page", &format!("{old_id} -> {new_id}"));
    let (commit, note) = commit_changes(vault, &message, store);
    rewritten_pages.sort();
    Ok(RenameOutcome {
        old_id: old_id.to_string(),
        new_id: new_id.to_string(),
        rewritten_pages,
        rewritten_links,
        rewritten_references,
        commit,
        note,
    })
}

pub(crate) fn delete_page_with_store(
    vault: &Path,
    id: &str,
    force: bool,
    store: &impl MasterKeyStore,
) -> RefactorResult<DeleteOutcome> {
    validate_page_id(id)?;
    checkpoint(vault, store)?;
    let path = resolve(vault, id, store)?;
    let raw = read_page(&path, id)?;
    // A junk page may well be malformed — deleting it must still work.
    // A parseable page must carry exactly this id; an unparseable one has
    // no id to check, so its file name must match exactly instead (on a
    // case-insensitive disk `entities/Junk` would otherwise delete
    // `entities/junk.md`).
    let label = match parse(&raw) {
        Ok(parsed) => {
            expect_id(id, &parsed.frontmatter.id)?;
            title_or_slug(parsed.frontmatter.title.as_deref(), id)
        }
        Err(_) => {
            if !file_name_matches_exactly(&path) {
                return Err(RefactorError::NotFound(id.to_string()));
            }
            slug_tail(id).to_string()
        }
    };

    let pages = collect_pages(vault)?;
    let referrers: Vec<&PageFile> = pages
        .iter()
        .filter(|p| {
            !same_file(&p.path, &path) && (links_to(&p.raw, id) || names_in_frontmatter(&p.raw, id))
        })
        .collect();
    if !referrers.is_empty() && !force {
        let mut ids: Vec<String> = referrers.iter().map(|p| p.id.clone()).collect();
        ids.sort();
        ids.dedup();
        return Err(RefactorError::StillLinked {
            id: id.to_string(),
            referrers: ids,
        });
    }

    let mut defused_in: Vec<String> = Vec::new();
    let mut defused_links = 0usize;
    let mut removed_references = 0usize;
    let mut ops: Vec<Op> = Vec::new();
    for p in referrers {
        let (new_raw, n) = rewrite_page_links(&p.raw, id, &LinkAction::Defuse(&label));
        let (new_raw, r) = rewrite_frontmatter_refs(&new_raw, &p.id, id, RefAction::Remove);
        if n + r > 0 {
            defused_links += n;
            removed_references += r;
            defused_in.push(p.id.clone());
            ops.push(Op::Write {
                path: p.path.clone(),
                contents: new_raw,
            });
        }
    }
    ops.push(Op::Remove { path });
    apply_journaled(ops)?;

    let message = commit_message(vault, "delete page", id);
    let (commit, note) = commit_changes(vault, &message, store);
    defused_in.sort();
    Ok(DeleteOutcome {
        deleted: id.to_string(),
        defused_in,
        defused_links,
        removed_references,
        commit,
        note,
    })
}

pub(crate) fn merge_pages_with_store(
    vault: &Path,
    from_id: &str,
    into_id: &str,
    store: &impl MasterKeyStore,
) -> RefactorResult<MergeOutcome> {
    validate_page_id(from_id)?;
    validate_page_id(into_id)?;
    if from_id == into_id {
        return Err(RefactorError::SameId(from_id.to_string()));
    }
    checkpoint(vault, store)?;
    let from_path = resolve(vault, from_id, store)?;
    let into_path = resolve(vault, into_id, store)?;
    if same_file(&from_path, &into_path) {
        return Err(RefactorError::SameFile {
            a: from_id.to_string(),
            b: into_id.to_string(),
        });
    }
    let from_raw = read_page(&from_path, from_id)?;
    let into_raw = read_page(&into_path, into_id)?;
    let from = parse(&from_raw).map_err(|e| malformed(from_id, e))?;
    let into = parse(&into_raw).map_err(|e| malformed(into_id, e))?;
    expect_id(from_id, &from.frontmatter.id)?;
    expect_id(into_id, &into.frontmatter.id)?;

    let into_label = title_or_slug(into.frontmatter.title.as_deref(), into_id);
    let from_label = title_or_slug(from.frontmatter.title.as_deref(), from_id);
    let eol = dominant_eol(&into_raw);

    // The appended source body must not end up linking the target to
    // itself: links to `into_id`, and the source's own self-links (which
    // the redirect would turn into `into_id` links), become plain text.
    let (appended, _) = rewrite_links(&from.body, into_id, &LinkAction::Defuse(&into_label));
    let (appended, _) = rewrite_links(&appended, from_id, &LinkAction::Defuse(&into_label));
    // Same for the target's own links to the source.
    let (target, _) = rewrite_page_links(&into_raw, from_id, &LinkAction::Defuse(&from_label));

    let mut merged = target.trim_end().to_string();
    merged.push_str(&format!("{eol}{eol}## Merged from {from_id}{eol}"));
    let appended = with_eol(appended.trim(), eol);
    if !appended.is_empty() {
        merged.push_str(eol);
        merged.push_str(&appended);
        merged.push_str(eol);
    }

    // Union tags, keeping the target's order and appending new ones.
    let mut tags = into.frontmatter.tags.clone();
    for t in &from.frontmatter.tags {
        if !tags.contains(t) {
            tags.push(t.clone());
        }
    }
    if tags != into.frontmatter.tags {
        merged = set_frontmatter_tags(&merged, &tags)
            .ok_or_else(|| malformed(into_id, "frontmatter could not be located"))?;
    }
    // The surviving page keeps the source's other names: its aliases and
    // its old id (so a later create under that name is caught as a
    // duplicate), and the union of both pages' sources.
    let mut aliases = into.frontmatter.aliases.clone();
    for a in from
        .frontmatter
        .aliases
        .iter()
        .chain(std::iter::once(&from.frontmatter.id))
    {
        if a != into_id && !aliases.contains(a) {
            aliases.push(a.clone());
        }
    }
    if aliases != into.frontmatter.aliases {
        merged = set_frontmatter_list(&merged, "aliases", &aliases)
            .ok_or_else(|| malformed(into_id, "frontmatter could not be located"))?;
    }
    let mut sources = into.frontmatter.sources.clone();
    for s in &from.frontmatter.sources {
        if s != into_id && s != from_id && !sources.contains(s) {
            sources.push(s.clone());
        }
    }
    if sources != into.frontmatter.sources {
        merged = set_frontmatter_list(&merged, "sources", &sources)
            .ok_or_else(|| malformed(into_id, "frontmatter could not be located"))?;
    }
    // `keep: true` ("leave this page alone") survives when either page
    // carried it.
    if from.frontmatter.keep == Some(true) && into.frontmatter.keep != Some(true) {
        // A bare YAML boolean, not the quoted string `set_frontmatter_scalar`
        // would write.
        merged = set_frontmatter_entry(&merged, "keep", Some("keep: true"))
            .ok_or_else(|| malformed(into_id, "frontmatter could not be located"))?;
    }
    // References from the target to the source would now point at itself.
    let (merged_refs, mut rewritten_references) =
        rewrite_frontmatter_refs(&merged, into_id, from_id, RefAction::Retarget(into_id));
    merged = merged_refs;
    let check = parse(&merged).map_err(|e| malformed(into_id, e))?;
    if check.frontmatter.id != into_id || check.frontmatter.tags != tags {
        return Err(malformed(
            into_id,
            "updating its frontmatter produced unexpected YAML; nothing was changed",
        ));
    }

    let pages = collect_pages(vault)?;
    let mut rewritten_pages: Vec<String> = Vec::new();
    let mut rewritten_links = 0usize;
    let mut ops = vec![Op::Write {
        path: into_path.clone(),
        contents: merged,
    }];
    for p in pages
        .iter()
        .filter(|p| !same_file(&p.path, &from_path) && !same_file(&p.path, &into_path))
    {
        let (new_raw, n) = rewrite_page_links(&p.raw, from_id, &LinkAction::Retarget(into_id));
        let (new_raw, r) =
            rewrite_frontmatter_refs(&new_raw, &p.id, from_id, RefAction::Retarget(into_id));
        if n + r > 0 {
            rewritten_links += n;
            rewritten_references += r;
            rewritten_pages.push(p.id.clone());
            ops.push(Op::Write {
                path: p.path.clone(),
                contents: new_raw,
            });
        }
    }
    ops.push(Op::Remove { path: from_path });
    apply_journaled(ops)?;

    let message = commit_message(vault, "merge pages", &format!("{from_id} into {into_id}"));
    let (commit, note) = commit_changes(vault, &message, store);
    rewritten_pages.sort();
    Ok(MergeOutcome {
        from_id: from_id.to_string(),
        into_id: into_id.to_string(),
        rewritten_pages,
        rewritten_links,
        rewritten_references,
        commit,
        note,
    })
}

// ---------------------------------------------------------------------------
// Id validation
// ---------------------------------------------------------------------------

/// THE check for any page id that is about to be turned into a path —
/// existing or new, by the refactor operations and by the MCP page tools.
/// An id is a relative `<type-dir>/<slug>` path under the wiki: no drive
/// letters or `:` at all (on Windows `wiki.join("C:/x")` *replaces* the
/// base path, and `:` also opens NTFS alternate data streams), no NUL or
/// control characters, no backslashes, no `..`, no empty or `.` segments,
/// and a registered type directory as the first segment. Runs before any
/// filesystem access; [`resolve`] re-checks the joined path as a backstop.
pub fn validate_page_id(id: &str) -> RefactorResult<()> {
    let invalid = |reason: String| {
        Err(RefactorError::InvalidId {
            id: id.to_string(),
            reason,
        })
    };
    if id.is_empty() {
        return invalid("must not be empty".into());
    }
    if id.chars().any(char::is_control) {
        return invalid("must not contain NUL or control characters".into());
    }
    if id.contains("..") {
        return invalid("may not contain '..'".into());
    }
    if id.contains(':') {
        return invalid("must not contain ':' (no drive letters, URLs or stream names)".into());
    }
    if id.contains('\\') {
        return invalid("must use forward slashes".into());
    }
    if id.starts_with('/') {
        return invalid("must be relative, like 'entities/alice'".into());
    }
    if id.split('/').any(|seg| seg.is_empty() || seg == ".") {
        return invalid("must not contain empty or '.' path segments".into());
    }
    if !Path::new(id)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return invalid("must be a plain relative path".into());
    }
    match id.split_once('/') {
        Some((dir, _)) if WIKI_SUBDIRS.contains(&dir) => Ok(()),
        _ => invalid(format!(
            "must have the form <type>/<slug> with <type> one of: {}",
            WIKI_SUBDIRS.join(", ")
        )),
    }
}

/// Stricter rules for an id a page is about to RECEIVE: on top of
/// [`validate_page_id`], every segment must match
/// `[A-Za-z0-9][A-Za-z0-9._-]*` and must not end in `.` (Windows drops
/// trailing dots), and the id must not end in `.md`. Whitespace and
/// parentheses would break markdown links, `<>"*:|?#` are invalid in
/// file names or break `[[link]]` syntax.
fn validate_new_page_id(id: &str) -> RefactorResult<()> {
    validate_page_id(id)?;
    let invalid = |reason: String| {
        Err(RefactorError::InvalidId {
            id: id.to_string(),
            reason,
        })
    };
    if id.ends_with(".md") {
        return invalid("must not end in '.md' — pass the bare id".into());
    }
    for seg in id.split('/') {
        let mut chars = seg.chars();
        let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
        let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !first_ok || !rest_ok || seg.ends_with('.') {
            return invalid(format!(
                "segment '{seg}' must start with a letter or digit and contain only letters, \
                 digits, '.', '_' and '-' (no spaces), and must not end in '.'"
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Link rewriting
// ---------------------------------------------------------------------------

/// What to do with a link whose target is the id being refactored.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LinkAction<'a> {
    /// Point the link at another id; aliases / link text are kept.
    Retarget(&'a str),
    /// Replace the link with plain text: the alias (or markdown link
    /// text) when present, otherwise the given label.
    Defuse(&'a str),
}

/// `[[target]]` / `[[target|alias]]` — same shape `extract_wiki_links`
/// recognises, with the parts captured separately.
static WIKI_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[\[(?P<target>[^\[\]\|]+?)(?P<alias>\|[^\]]*)?\]\]").expect("regex")
});

/// `[text](destination "title")`, optionally an image (`![alt](…)`) — the
/// shape `extract_wiki_links` uses for markdown links with a wiki-page
/// destination.
static MD_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?P<bang>!)?\[(?P<label>[^\]]*)\]\(\s*(?P<target>[^)\s]+)(?:\s+"[^"]*")?\s*\)"#)
        .expect("regex")
});

/// Rewrite every link to `id` in `text` according to `action`. Returns
/// the new text and the number of links changed. Matching is on the exact
/// id (after the same trimming the link extractor applies); everything
/// outside a matched link is copied byte-for-byte.
pub(crate) fn rewrite_links(text: &str, id: &str, action: &LinkAction<'_>) -> (String, usize) {
    let mut count = 0usize;
    let pass1 = WIKI_LINK.replace_all(text, |caps: &Captures<'_>| {
        let raw_target = &caps["target"];
        if raw_target.trim() != id {
            return caps[0].to_string();
        }
        count += 1;
        let alias = caps.name("alias").map(|m| m.as_str());
        match action {
            LinkAction::Retarget(new_id) => {
                let lead = &raw_target[..raw_target.len() - raw_target.trim_start().len()];
                let trail = &raw_target[raw_target.trim_end().len()..];
                format!("[[{lead}{new_id}{trail}{}]]", alias.unwrap_or(""))
            }
            LinkAction::Defuse(label) => match alias.map(|a| a[1..].trim()) {
                Some(a) if !a.is_empty() => a.to_string(),
                _ => (*label).to_string(),
            },
        }
    });
    let pass2 = MD_LINK.replace_all(&pass1, |caps: &Captures<'_>| {
        let target = caps.name("target").expect("target group");
        if page_id_from_markdown_target(target.as_str()).as_deref() != Some(id) {
            return caps[0].to_string();
        }
        count += 1;
        match action {
            LinkAction::Retarget(new_id) => {
                let whole = caps.get(0).expect("whole match");
                let raw = target.as_str();
                // The id is a contiguous substring of the destination
                // (optional tauri.localhost prefix, then the id, then an
                // optional `.md` / query / fragment), so swap just that.
                let new_raw = match raw.find(id) {
                    Some(pos) => format!("{}{new_id}{}", &raw[..pos], &raw[pos + id.len()..]),
                    None => (*new_id).to_string(),
                };
                let start = target.start() - whole.start();
                let end = target.end() - whole.start();
                let s = whole.as_str();
                format!("{}{new_raw}{}", &s[..start], &s[end..])
            }
            // The image marker `!` goes too — plain text, no stray `!`.
            LinkAction::Defuse(label) => {
                let text = caps["label"].trim();
                if text.is_empty() {
                    (*label).to_string()
                } else {
                    text.to_string()
                }
            }
        }
    });
    (pass2.into_owned(), count)
}

/// [`rewrite_links`] applied to a whole page file, touching only the part
/// after the frontmatter so YAML values that happen to look like links
/// stay as they are. A file without recognisable frontmatter is
/// rewritten as a whole.
fn rewrite_page_links(raw: &str, id: &str, action: &LinkAction<'_>) -> (String, usize) {
    match frontmatter_span(raw) {
        Some((_, yaml_end)) => {
            let (rest, n) = rewrite_links(&raw[yaml_end..], id, action);
            if n == 0 {
                return (raw.to_string(), 0);
            }
            (format!("{}{rest}", &raw[..yaml_end]), n)
        }
        None => rewrite_links(raw, id, action),
    }
}

/// What to do with a frontmatter reference (`superseded_by`, `sources`)
/// to the id being refactored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RefAction<'a> {
    /// Point it at another id.
    Retarget(&'a str),
    /// Remove it (the `superseded_by` line / the `sources` entry).
    Remove,
}

/// Whether a page file's frontmatter names `id` in `superseded_by` or
/// `sources`.
fn names_in_frontmatter(raw: &str, id: &str) -> bool {
    match parse(raw) {
        Ok(p) => {
            p.frontmatter.superseded_by.as_deref() == Some(id)
                || p.frontmatter.sources.iter().any(|s| s == id)
        }
        Err(_) => false,
    }
}

/// Rewrite the frontmatter references to `id` of the page file `raw`
/// (whose own id is `own_id`): `superseded_by: <id>` and `id` entries of
/// `sources` (flow list, block list or single value; `[[id]]` forms too).
/// A retarget onto the page itself becomes a removal. Only the touched
/// entries change (a rewritten `sources` is written as a flow list);
/// every other byte stays. Returns the new text and the number of
/// references changed; an unparseable file is returned unchanged.
pub(crate) fn rewrite_frontmatter_refs(
    raw: &str,
    own_id: &str,
    id: &str,
    action: RefAction<'_>,
) -> (String, usize) {
    let Ok(parsed) = parse(raw) else {
        return (raw.to_string(), 0);
    };
    let fm = &parsed.frontmatter;
    let action = match action {
        RefAction::Retarget(target) if target == own_id => RefAction::Remove,
        other => other,
    };
    let mut out = raw.to_string();
    let mut changed = 0usize;
    if fm.superseded_by.as_deref() == Some(id) {
        let line = match action {
            RefAction::Retarget(target) => Some(format!("superseded_by: {}", yaml_scalar(target))),
            RefAction::Remove => None,
        };
        if let Some(new) = set_frontmatter_entry(&out, "superseded_by", line.as_deref()) {
            out = new;
            changed += 1;
        }
    }
    let hits = fm.sources.iter().filter(|s| *s == id).count();
    if hits > 0 {
        let mut list: Vec<String> = Vec::new();
        for s in &fm.sources {
            let kept = if s == id {
                match action {
                    RefAction::Retarget(target) => Some(target.to_string()),
                    RefAction::Remove => None,
                }
            } else {
                Some(s.clone())
            };
            if let Some(v) = kept {
                if !list.contains(&v) {
                    list.push(v);
                }
            }
        }
        let new = if list.is_empty() {
            set_frontmatter_entry(&out, "sources", None)
        } else {
            set_frontmatter_list(&out, "sources", &list)
        };
        if let Some(new) = new {
            out = new;
            changed += hits;
        }
    }
    (out, changed)
}

/// Whether a page file's body links to `id` — the same edge definition
/// the backlinks view and the indexer use.
fn links_to(raw: &str, id: &str) -> bool {
    match parse(raw) {
        Ok(p) => extract_wiki_links(&p.body).iter().any(|l| l == id),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Frontmatter editing (line-level, byte-preserving elsewhere)
// ---------------------------------------------------------------------------

/// Byte range `(start, end)` of the YAML between the frontmatter fences,
/// located exactly like [`parse`] does. `raw[end..]` starts with the
/// closing `\n---`.
fn frontmatter_span(raw: &str) -> Option<(usize, usize)> {
    let trimmed = raw.trim_start_matches('\u{feff}');
    let after_first = trimmed.strip_prefix("---")?.trim_start_matches('\n');
    let end = after_first
        .find("\n---\n")
        .or_else(|| after_first.find("\n---"))?;
    let start = raw.len() - after_first.len();
    Some((start, start + end))
}

/// Set a top-level scalar `key` in the page's frontmatter to `value`,
/// replacing the existing `key:` line (line ending preserved) or appending
/// one. Everything else in the file is untouched. `None` when the file has
/// no frontmatter.
fn set_frontmatter_scalar(raw: &str, key: &str, value: &str) -> Option<String> {
    let (start, end) = frontmatter_span(raw)?;
    let yaml = &raw[start..end];
    let line = format!("{key}: {}", yaml_scalar(value));
    let new_yaml = replace_top_level_entry(yaml, key, &line, false, dominant_eol(raw));
    Some(format!("{}{new_yaml}{}", &raw[..start], &raw[end..]))
}

/// Set the frontmatter `tags` to `tags` (flow style), replacing an
/// existing `tags:` entry in either flow or block style.
fn set_frontmatter_tags(raw: &str, tags: &[String]) -> Option<String> {
    set_frontmatter_list(raw, "tags", tags)
}

/// Set the top-level frontmatter list `key` to `items` (flow style,
/// JSON-quoted — valid YAML), replacing an existing entry in flow, block
/// or single-value style, or appending one.
fn set_frontmatter_list(raw: &str, key: &str, items: &[String]) -> Option<String> {
    let rendered = serde_json::to_string(items).ok()?;
    set_frontmatter_entry(raw, key, Some(&format!("{key}: {rendered}")))
}

/// Replace the top-level frontmatter entry `key` (with its block
/// continuation lines) by `line`, or remove it when `line` is `None`
/// (a no-op for a missing key). `None` when the file has no frontmatter.
fn set_frontmatter_entry(raw: &str, key: &str, line: Option<&str>) -> Option<String> {
    let (start, end) = frontmatter_span(raw)?;
    let yaml = &raw[start..end];
    let new_yaml = match line {
        Some(line) => replace_top_level_entry(yaml, key, line, true, dominant_eol(raw)),
        None => remove_top_level_entry(yaml, key),
    };
    Some(format!("{}{new_yaml}{}", &raw[..start], &raw[end..]))
}

/// `yaml` without the top-level `key:` entry and its block continuation
/// lines. When the entry was the last one, the line break before it goes
/// too (the closing fence supplies its own), so no blank line is left.
fn remove_top_level_entry(yaml: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    let lines: Vec<&str> = yaml.split_inclusive('\n').collect();
    let Some(idx) = lines.iter().position(|l| l.starts_with(&prefix)) else {
        return yaml.to_string();
    };
    let mut last = idx;
    while last + 1 < lines.len() {
        let next = lines[last + 1];
        if next.starts_with(' ') || next.starts_with('\t') || next.starts_with("- ") {
            last += 1;
        } else {
            break;
        }
    }
    let mut out: String = lines[..idx].concat();
    out.push_str(&lines[last + 1..].concat());
    if last + 1 == lines.len() {
        // The removed entry closed the YAML; its predecessor's line break
        // would now double the closing fence's.
        if out.ends_with('\n') {
            out.pop();
            if lines[last].ends_with('\r') && out.ends_with('\r') {
                // CRLF file: keep the `\r` the fence's `\n` pairs with.
            } else if out.ends_with('\r') {
                out.pop();
            }
        }
    }
    out
}

/// Replace the top-level `key:` entry in `yaml` with `line`. With
/// `with_block` the entry's continuation lines (indented, or `- ` block
/// sequence items) are replaced too. Appends when the key is absent, in
/// the file's line-ending style `eol` (the `\n` of the closing fence
/// follows the YAML slice, so a CRLF file's last line ends in a bare
/// `\r` here).
fn replace_top_level_entry(
    yaml: &str,
    key: &str,
    line: &str,
    with_block: bool,
    eol: &str,
) -> String {
    let prefix = format!("{key}:");
    let lines: Vec<&str> = yaml.split_inclusive('\n').collect();
    let Some(idx) = lines.iter().position(|l| l.starts_with(&prefix)) else {
        let mut out = yaml.to_string();
        if out.ends_with('\r') {
            out.push('\n');
        } else if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(line);
        if eol == "\r\n" {
            out.push('\r');
        }
        return out;
    };
    let mut last = idx;
    if with_block {
        while last + 1 < lines.len() {
            let next = lines[last + 1];
            if next.starts_with(' ') || next.starts_with('\t') || next.starts_with("- ") {
                last += 1;
            } else {
                break;
            }
        }
    }
    let ending = line_ending(lines[last]);
    let mut out = String::with_capacity(yaml.len() + line.len());
    for l in &lines[..idx] {
        out.push_str(l);
    }
    out.push_str(line);
    out.push_str(ending);
    for l in &lines[last + 1..] {
        out.push_str(l);
    }
    out
}

fn line_ending(line: &str) -> &str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else if line.ends_with('\r') {
        "\r"
    } else {
        ""
    }
}

/// `"\r\n"` when most line breaks in `raw` are CRLF, `"\n"` otherwise.
fn dominant_eol(raw: &str) -> &'static str {
    let lf = raw.matches('\n').count();
    let crlf = raw.matches("\r\n").count();
    if crlf * 2 > lf { "\r\n" } else { "\n" }
}

/// `text` with every line break converted to `eol`.
fn with_eol(text: &str, eol: &str) -> String {
    let lf = text.replace("\r\n", "\n");
    if eol == "\n" {
        lf
    } else {
        lf.replace('\n', eol)
    }
}

/// Render a scalar for YAML: plain when it is unambiguously safe (the
/// usual `type/slug` id shape), JSON-quoted (valid YAML) otherwise.
fn yaml_scalar(value: &str) -> String {
    let plain = value
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'));
    if plain {
        value.to_string()
    } else {
        serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
    }
}

fn verify_frontmatter_id(raw: &str, id: &str) -> RefactorResult<()> {
    let parsed = parse(raw).map_err(|e| malformed(id, e))?;
    if parsed.frontmatter.id == id {
        Ok(())
    } else {
        Err(malformed(
            id,
            "updating its frontmatter id produced unexpected YAML; nothing was changed",
        ))
    }
}

// ---------------------------------------------------------------------------
// Journaled file changes
// ---------------------------------------------------------------------------

/// One planned file change.
enum Op {
    /// Create or overwrite `path` with `contents`.
    Write { path: PathBuf, contents: String },
    /// Remove the existing file `path`.
    Remove { path: PathBuf },
    /// Move `from` to `to` (handles a case-only rename on a
    /// case-insensitive disk).
    Move { from: PathBuf, to: PathBuf },
}

/// How to take back one applied [`Op`].
enum Undo {
    Restore { path: PathBuf, bytes: Vec<u8> },
    RemoveNew { path: PathBuf },
    MoveBack { from: PathBuf, to: PathBuf },
}

/// Apply `ops` in order. If one fails, every change already made is undone
/// in reverse order (overwritten files get their original bytes back,
/// created files are removed, moves are reversed) and the error is
/// returned — the vault is left as it was before the call.
fn apply_journaled(ops: Vec<Op>) -> RefactorResult<()> {
    let mut journal: Vec<Undo> = Vec::new();
    for (i, op) in ops.into_iter().enumerate() {
        if let Err(err) = failpoint(i).and_then(|()| apply_one(op, &mut journal)) {
            roll_back(journal);
            return Err(err.into());
        }
    }
    Ok(())
}

fn apply_one(op: Op, journal: &mut Vec<Undo>) -> std::io::Result<()> {
    match op {
        Op::Write { path, contents } => {
            // Journal BEFORE writing, so a half-written file is restored too.
            let undo = if path.is_file() {
                Undo::Restore {
                    bytes: std::fs::read(&path)?,
                    path: path.clone(),
                }
            } else {
                Undo::RemoveNew { path: path.clone() }
            };
            journal.push(undo);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, contents)
        }
        Op::Remove { path } => {
            let bytes = std::fs::read(&path)?;
            journal.push(Undo::Restore {
                path: path.clone(),
                bytes,
            });
            std::fs::remove_file(&path)
        }
        Op::Move { from, to } => {
            move_file(&from, &to)?;
            journal.push(Undo::MoveBack { from: to, to: from });
            Ok(())
        }
    }
}

fn roll_back(journal: Vec<Undo>) {
    for undo in journal.into_iter().rev() {
        let result = match &undo {
            Undo::Restore { path, bytes } => path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(path, bytes)),
            Undo::RemoveNew { path } => match std::fs::remove_file(path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
            Undo::MoveBack { from, to } => move_file(from, to),
        };
        if let Err(err) = result {
            tracing::error!(%err, "refactor rollback step failed — the vault may need a manual check");
        }
    }
}

/// Move a file, creating the destination directory. A case-only rename on
/// a case-insensitive disk (`from` and `to` are the same file) goes through
/// a temporary name so the new spelling actually lands on disk. Refuses to
/// overwrite a different existing file.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if to.exists() && !same_file(from, to) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("refusing to overwrite {}", to.display()),
        ));
    }
    if from != to && to.exists() {
        let name = from.file_name().and_then(|n| n.to_str()).unwrap_or("page");
        let tmp = from.with_file_name(format!(".{name}.brain-rename.tmp"));
        std::fs::rename(from, &tmp)?;
        if let Err(err) = std::fs::rename(&tmp, to) {
            let _ = std::fs::rename(&tmp, from);
            return Err(err);
        }
        return Ok(());
    }
    std::fs::rename(from, to)
}

#[cfg(test)]
thread_local! {
    /// Test-only failpoint: the journal op index (or [`COMMIT_FAILPOINT`])
    /// at which to inject an I/O error on this thread.
    static FAIL_AT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Failpoint index that makes the final commit fail.
#[cfg(test)]
const COMMIT_FAILPOINT: usize = usize::MAX;

#[cfg(test)]
fn failpoint(i: usize) -> std::io::Result<()> {
    if FAIL_AT.with(|f| f.get()) == Some(i) {
        return Err(std::io::Error::other("injected failure"));
    }
    Ok(())
}

#[cfg(not(test))]
fn failpoint(_i: usize) -> std::io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Commits
// ---------------------------------------------------------------------------

/// Commit any uncommitted work in the wiki before a refactor touches it,
/// so everything the refactor removes or rewrites is in history. A clean
/// tree produces no commit. Also places pages created outside the app at
/// their opaque path on an encrypted vault, before paths are resolved.
fn checkpoint(vault: &Path, store: &impl MasterKeyStore) -> RefactorResult<()> {
    commit_wiki_with_store(&wiki_dir(vault), CHECKPOINT_MESSAGE, store)?;
    Ok(())
}

/// The refactor's single commit. The files are already changed when this
/// runs, so a failure here must not turn into a tool error (an agent would
/// retry into "page not found"): it is logged and reported as a note.
fn commit_changes(
    vault: &Path,
    message: &str,
    store: &impl MasterKeyStore,
) -> (Option<String>, Option<String>) {
    let result = failpoint_commit()
        .map_err(WikiError::from)
        .and_then(|()| commit_wiki_with_store(&wiki_dir(vault), message, store));
    match result {
        Ok(commit) => (commit, None),
        Err(err) => {
            tracing::warn!(%err, "refactor applied but its commit failed");
            (
                None,
                Some(format!(
                    "the changes are on disk but could not be committed yet ({err}); the \
                     watcher commits them once lint is clean — do not repeat the operation"
                )),
            )
        }
    }
}

fn failpoint_commit() -> std::io::Result<()> {
    #[cfg(test)]
    {
        failpoint(COMMIT_FAILPOINT)
    }
    #[cfg(not(test))]
    {
        Ok(())
    }
}

/// Drop `path`'s entry from the wiki repo index (best effort). See the
/// case-only rename note in [`rename_page_with_store`].
fn forget_index_entry(vault: &Path, path: &Path) {
    let wiki = wiki_dir(vault);
    let Ok(rel) = path.strip_prefix(&wiki) else {
        return;
    };
    let result = git2::Repository::open(&wiki).and_then(|repo| {
        let mut index = repo.index()?;
        if index.get_path(rel, 0).is_some() {
            index.remove_path(rel)?;
            index.write()?;
        }
        Ok(())
    });
    if let Err(err) = result {
        tracing::warn!(%err, "could not drop the renamed page's old index entry");
    }
}

/// Commit message for a refactor. Path-free on an encrypted vault (commit
/// messages are pushed and must not reveal page names); detailed on a
/// plaintext vault — the same policy as the watcher's auto-commit.
fn commit_message(vault: &Path, action: &str, detail: &str) -> String {
    if is_encrypted(vault) {
        format!("wiki: {action}")
    } else {
        format!("wiki: {action} {detail}")
    }
}

// ---------------------------------------------------------------------------
// Vault walking and helpers
// ---------------------------------------------------------------------------

/// A parseable page file found on disk.
struct PageFile {
    id: String,
    path: PathBuf,
    raw: String,
}

/// Every parseable page under the wiki type directories, at whatever path
/// it currently lives (plaintext or opaque layout).
fn collect_pages(vault: &Path) -> RefactorResult<Vec<PageFile>> {
    let mut out = Vec::new();
    let wiki = wiki_dir(vault);
    for sub in WIKI_SUBDIRS {
        let dir = wiki.join(sub);
        if dir.is_dir() {
            visit(&dir, &mut out)?;
        }
    }
    Ok(out)
}

fn visit(dir: &Path, out: &mut Vec<PageFile>) -> RefactorResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            visit(&path, out)?;
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = parse(&raw) else {
            continue;
        };
        out.push(PageFile {
            id: parsed.frontmatter.id,
            path,
            raw,
        });
    }
    Ok(())
}

/// The on-disk path of a (validated) id, via the encryption-aware
/// resolver. Backstop: the result must lie under the wiki directory.
fn resolve(vault: &Path, id: &str, store: &impl MasterKeyStore) -> RefactorResult<PathBuf> {
    let wiki = wiki_dir(vault);
    let path = wiki.join(page_relpath_with_store(vault, id, store)?);
    if !path.starts_with(&wiki) {
        return Err(RefactorError::InvalidId {
            id: id.to_string(),
            reason: "resolves outside the wiki directory".to_string(),
        });
    }
    Ok(path)
}

fn read_page(path: &Path, id: &str) -> RefactorResult<String> {
    if !path.is_file() {
        return Err(RefactorError::NotFound(id.to_string()));
    }
    Ok(std::fs::read_to_string(path)?)
}

/// Refuse a page whose frontmatter id differs from the requested one —
/// typically the same file reached through a differently-cased id on a
/// case-insensitive disk.
fn expect_id(requested: &str, found: &str) -> RefactorResult<()> {
    if requested == found {
        Ok(())
    } else {
        Err(RefactorError::IdMismatch {
            requested: requested.to_string(),
            found: found.to_string(),
        })
    }
}

/// Whether the directory listing of `path`'s parent contains exactly
/// `path`'s file name (byte-for-byte, regardless of disk case rules).
fn file_name_matches_exactly(path: &Path) -> bool {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    std::fs::read_dir(parent)
        .map(|entries| entries.flatten().any(|e| e.file_name() == name))
        .unwrap_or(false)
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || match (a.canonicalize(), b.canonicalize()) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
}

fn singular_type(dir: &str) -> Option<&'static str> {
    TYPE_DIRS.iter().find(|(d, _)| *d == dir).map(|(_, t)| *t)
}

fn slug_tail(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

fn title_or_slug(title: Option<&str>, id: &str) -> String {
    title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| slug_tail(id).to_string())
}

fn malformed(id: &str, reason: impl std::fmt::Display) -> RefactorError {
    RefactorError::Malformed {
        id: id.to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::keychain::KeychainError;
    use crate::vault::layout::ensure_skeleton;
    use crate::vault::marker::{VaultMarker, write_marker};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // In-memory MasterKeyStore (same pattern as the encryption tests).
    #[derive(Default)]
    struct MemStore(Mutex<HashMap<String, String>>);
    impl MasterKeyStore for MemStore {
        fn set_hex(&self, account: &str, hex: &str) -> Result<(), KeychainError> {
            self.0.lock().unwrap().insert(account.into(), hex.into());
            Ok(())
        }
        fn get_hex(&self, account: &str) -> Result<Option<String>, KeychainError> {
            Ok(self.0.lock().unwrap().get(account).cloned())
        }
        fn delete(&self, account: &str) -> Result<(), KeychainError> {
            self.0.lock().unwrap().remove(account);
            Ok(())
        }
    }

    fn page(id: &str, ty: &str, title: &str, body: &str) -> String {
        format!("---\nid: {id}\ntype: {ty}\ntitle: {title}\n---\n\n{body}\n")
    }

    /// A plaintext vault with a git repo and a baseline commit holding
    /// `pages` (id, raw content).
    fn vault_with(pages: &[(&str, String)]) -> TempDir {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_marker(tmp.path(), &VaultMarker::new("0.0.0-test")).unwrap();
        let wiki = wiki_dir(tmp.path());
        crate::wiki::git::init_repo(&wiki).unwrap();
        for (id, raw) in pages {
            put(&wiki.join(format!("{id}.md")), raw);
        }
        crate::wiki::git::commit_all(&wiki, "baseline").unwrap();
        tmp
    }

    fn standard_vault() -> TempDir {
        vault_with(&[
            (
                "entities/old",
                page("entities/old", "entity", "Old Name", "Old body."),
            ),
            (
                "entities/alice",
                page(
                    "entities/alice",
                    "entity",
                    "Alice",
                    "Knows [[entities/old]] and [[entities/old|the old one]].",
                ),
            ),
            (
                "entities/bob",
                page(
                    "entities/bob",
                    "entity",
                    "Bob",
                    "Sibling [[entities/old-2]] only.",
                ),
            ),
            (
                "entities/old-2",
                page("entities/old-2", "entity", "Old 2", "Other."),
            ),
        ])
    }

    fn read(vault: &Path, id: &str) -> String {
        std::fs::read_to_string(wiki_dir(vault).join(format!("{id}.md"))).unwrap()
    }

    fn commit_count(vault: &Path) -> usize {
        let repo = git2::Repository::open(wiki_dir(vault)).unwrap();
        let mut walk = repo.revwalk().unwrap();
        walk.push_head().unwrap();
        walk.count()
    }

    fn head_has(vault: &Path, rel: &str) -> bool {
        let repo = git2::Repository::open(wiki_dir(vault)).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        tree.get_path(Path::new(rel)).is_ok()
    }

    fn store() -> MemStore {
        MemStore::default()
    }

    // --- link rewriting ---------------------------------------------------

    #[test]
    fn rewrite_retargets_plain_and_aliased_wiki_links() {
        let (out, _) = rewrite_links(
            "a [[entities/old]] b [[entities/old|Alias]] c",
            "entities/old",
            &LinkAction::Retarget("entities/new"),
        );
        assert_eq!(out, "a [[entities/new]] b [[entities/new|Alias]] c");
    }

    #[test]
    fn rewrite_leaves_links_to_ids_that_merely_share_a_prefix_untouched() {
        let text = "see [[entities/old-2]] and [[entities/oldx]] and [[entities/old/sub]]";
        let (out, _) = rewrite_links(text, "entities/old", &LinkAction::Retarget("entities/new"));
        assert_eq!(out, text);
    }

    #[test]
    fn rewrite_keeps_text_directly_after_a_link_byte_for_byte() {
        let (out, _) = rewrite_links(
            "[[entities/old]]x",
            "entities/old",
            &LinkAction::Retarget("entities/new"),
        );
        assert_eq!(out, "[[entities/new]]x");
    }

    #[test]
    fn rewrite_retargets_markdown_links_and_keeps_the_md_suffix() {
        let (out, _) = rewrite_links(
            "| [Old](entities/old.md) | x |",
            "entities/old",
            &LinkAction::Retarget("entities/new"),
        );
        assert_eq!(out, "| [Old](entities/new.md) | x |");
    }

    #[test]
    fn rewrite_counts_every_changed_link() {
        let (_, n) = rewrite_links(
            "[[entities/old]] [[entities/old|A]] [B](entities/old) [[entities/other]]",
            "entities/old",
            &LinkAction::Retarget("entities/new"),
        );
        assert_eq!(n, 3);
    }

    #[test]
    fn defuse_turns_links_into_alias_or_label_text() {
        let (out, _) = rewrite_links(
            "[[entities/old]], [[entities/old|Alias]], [Text](entities/old).",
            "entities/old",
            &LinkAction::Defuse("Old Name"),
        );
        assert_eq!(out, "Old Name, Alias, Text.");
    }

    #[test]
    fn set_frontmatter_scalar_preserves_every_other_byte() {
        let raw = "---\nid: entities/old\ntype: entity\ntitle: T\n---\n\nbody\n";
        let out = set_frontmatter_scalar(raw, "id", "entities/new").unwrap();
        assert_eq!(
            out,
            "---\nid: entities/new\ntype: entity\ntitle: T\n---\n\nbody\n"
        );
    }

    #[test]
    fn set_frontmatter_tags_replaces_a_block_style_list() {
        let raw = "---\nid: a/b\ntags:\n  - x\n  - y\ntitle: T\n---\nbody\n";
        let out = set_frontmatter_tags(raw, &["x".into(), "y".into(), "z".into()]).unwrap();
        assert_eq!(
            out,
            "---\nid: a/b\ntags: [\"x\",\"y\",\"z\"]\ntitle: T\n---\nbody\n"
        );
    }

    // --- rename -----------------------------------------------------------

    #[test]
    fn rename_moves_the_file_to_the_new_id_path() {
        let v = standard_vault();
        rename_page_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        let wiki = wiki_dir(v.path());
        assert!(
            wiki.join("entities/new.md").is_file() && !wiki.join("entities/old.md").exists(),
            "file must live at the new path only"
        );
    }

    #[test]
    fn rename_updates_the_frontmatter_id() {
        let v = standard_vault();
        rename_page_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        assert_eq!(
            read(v.path(), "entities/new"),
            page("entities/new", "entity", "Old Name", "Old body.")
        );
    }

    #[test]
    fn rename_into_another_type_directory_updates_the_type_field() {
        let v = standard_vault();
        rename_page_with_store(v.path(), "entities/old", "concepts/old", &store()).unwrap();
        assert_eq!(
            read(v.path(), "concepts/old"),
            page("concepts/old", "concept", "Old Name", "Old body.")
        );
    }

    #[test]
    fn rename_rewrites_plain_and_aliased_links_in_other_pages() {
        let v = standard_vault();
        rename_page_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        assert_eq!(
            read(v.path(), "entities/alice"),
            page(
                "entities/alice",
                "entity",
                "Alice",
                "Knows [[entities/new]] and [[entities/new|the old one]]."
            )
        );
    }

    #[test]
    fn rename_reports_the_rewritten_pages_and_link_count() {
        let v = standard_vault();
        let out =
            rename_page_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        assert_eq!(
            (out.rewritten_pages, out.rewritten_links),
            (vec!["entities/alice".to_string()], 2)
        );
    }

    #[test]
    fn rename_leaves_links_to_a_similarly_named_page_untouched() {
        let v = standard_vault();
        let before = read(v.path(), "entities/bob");
        rename_page_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        assert_eq!(read(v.path(), "entities/bob"), before);
    }

    #[test]
    fn rename_refuses_when_the_target_id_already_exists() {
        let v = standard_vault();
        let err = rename_page_with_store(v.path(), "entities/old", "entities/alice", &store())
            .unwrap_err();
        assert!(matches!(err, RefactorError::TargetExists(ref id) if id == "entities/alice"));
    }

    #[test]
    fn rename_refuses_when_the_source_page_is_missing() {
        let v = standard_vault();
        let err = rename_page_with_store(v.path(), "entities/ghost", "entities/new", &store())
            .unwrap_err();
        assert!(matches!(err, RefactorError::NotFound(_)));
    }

    #[test]
    fn rename_refuses_a_new_id_outside_the_registered_type_directories() {
        let v = standard_vault();
        let err =
            rename_page_with_store(v.path(), "entities/old", "people/new", &store()).unwrap_err();
        assert!(matches!(err, RefactorError::InvalidId { .. }));
    }

    #[test]
    fn rename_produces_exactly_one_new_commit() {
        let v = standard_vault();
        let before = commit_count(v.path());
        rename_page_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        assert_eq!(commit_count(v.path()), before + 1);
    }

    #[test]
    fn rename_commit_no_longer_contains_the_old_path() {
        let v = standard_vault();
        rename_page_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        assert!(!head_has(v.path(), "entities/old.md") && head_has(v.path(), "entities/new.md"));
    }

    // --- rename on an encrypted vault -------------------------------------

    /// An encrypted vault with `entities/old` and a page linking to it,
    /// both committed at their opaque paths.
    fn encrypted_vault(store: &MemStore) -> TempDir {
        let v = vault_with(&[
            (
                "entities/old",
                page("entities/old", "entity", "Old", "Body."),
            ),
            (
                "entities/alice",
                page("entities/alice", "entity", "Alice", "See [[entities/old]]."),
            ),
        ]);
        crate::wiki::encryption::enable_encryption(
            v.path(),
            store,
            &PathBuf::from("/opt/brain/brain"),
        )
        .unwrap();
        commit_wiki_with_store(&wiki_dir(v.path()), "encrypt", store).unwrap();
        v
    }

    #[test]
    fn rename_on_an_encrypted_vault_moves_the_file_to_the_new_opaque_path() {
        let s = store();
        let v = encrypted_vault(&s);
        let old_rel = page_relpath_with_store(v.path(), "entities/old", &s).unwrap();
        let new_rel = page_relpath_with_store(v.path(), "entities/new", &s).unwrap();
        rename_page_with_store(v.path(), "entities/old", "entities/new", &s).unwrap();
        assert!(head_has(v.path(), &new_rel) && !head_has(v.path(), &old_rel));
    }

    #[test]
    fn rename_on_an_encrypted_vault_rewrites_links_in_opaque_named_pages() {
        let s = store();
        let v = encrypted_vault(&s);
        let alice = wiki_dir(v.path())
            .join(page_relpath_with_store(v.path(), "entities/alice", &s).unwrap());
        rename_page_with_store(v.path(), "entities/old", "entities/new", &s).unwrap();
        assert!(
            std::fs::read_to_string(alice)
                .unwrap()
                .contains("See [[entities/new]].")
        );
    }

    #[test]
    fn rename_on_an_encrypted_vault_commits_a_path_free_message() {
        let s = store();
        let v = encrypted_vault(&s);
        rename_page_with_store(v.path(), "entities/old", "entities/new", &s).unwrap();
        let repo = git2::Repository::open(wiki_dir(v.path())).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.message(), Some("wiki: rename page"));
    }

    // --- delete -----------------------------------------------------------

    #[test]
    fn delete_refuses_and_lists_the_referrers_when_backlinks_exist() {
        let v = standard_vault();
        let err = delete_page_with_store(v.path(), "entities/old", false, &store()).unwrap_err();
        assert!(matches!(
            err,
            RefactorError::StillLinked { ref referrers, .. } if referrers == &vec!["entities/alice".to_string()]
        ));
    }

    #[test]
    fn delete_refusal_leaves_the_page_on_disk() {
        let v = standard_vault();
        let _ = delete_page_with_store(v.path(), "entities/old", false, &store());
        assert!(wiki_dir(v.path()).join("entities/old.md").is_file());
    }

    #[test]
    fn delete_with_force_defuses_links_to_plain_text() {
        let v = standard_vault();
        delete_page_with_store(v.path(), "entities/old", true, &store()).unwrap();
        assert_eq!(
            read(v.path(), "entities/alice"),
            page(
                "entities/alice",
                "entity",
                "Alice",
                "Knows Old Name and the old one."
            )
        );
    }

    #[test]
    fn delete_with_force_reports_where_links_were_defused() {
        let v = standard_vault();
        let out = delete_page_with_store(v.path(), "entities/old", true, &store()).unwrap();
        assert_eq!(
            (out.defused_in, out.defused_links),
            (vec!["entities/alice".to_string()], 2)
        );
    }

    #[test]
    fn delete_of_an_unlinked_page_removes_it_from_the_next_commit() {
        let v = standard_vault();
        delete_page_with_store(v.path(), "entities/alice", false, &store()).unwrap();
        assert!(!head_has(v.path(), "entities/alice.md"));
    }

    #[test]
    fn delete_produces_exactly_one_new_commit() {
        let v = standard_vault();
        let before = commit_count(v.path());
        delete_page_with_store(v.path(), "entities/old", true, &store()).unwrap();
        assert_eq!(commit_count(v.path()), before + 1);
    }

    // --- merge ------------------------------------------------------------

    fn merge_vault() -> TempDir {
        vault_with(&[
            (
                "entities/dup",
                "---\nid: entities/dup\ntype: entity\ntitle: Dup\ntags: [a, c]\n---\n\nDup facts, see [[entities/main]].\n".to_string(),
            ),
            (
                "entities/main",
                "---\nid: entities/main\ntype: entity\ntitle: Main\ntags: [a, b]\n---\n\nMain facts.\n".to_string(),
            ),
            (
                "entities/carol",
                page("entities/carol", "entity", "Carol", "Met [[entities/dup|the dup]]."),
            ),
        ])
    }

    #[test]
    fn merge_appends_the_source_body_under_the_merged_from_heading() {
        let v = merge_vault();
        merge_pages_with_store(v.path(), "entities/dup", "entities/main", &store()).unwrap();
        assert_eq!(
            read(v.path(), "entities/main"),
            "---\nid: entities/main\ntype: entity\ntitle: Main\ntags: [\"a\",\"b\",\"c\"]\naliases: [\"entities/dup\"]\n---\n\nMain facts.\n\n## Merged from entities/dup\n\nDup facts, see Main.\n"
        );
    }

    #[test]
    fn merge_redirects_links_to_the_source_and_keeps_aliases() {
        let v = merge_vault();
        merge_pages_with_store(v.path(), "entities/dup", "entities/main", &store()).unwrap();
        assert_eq!(
            read(v.path(), "entities/carol"),
            page(
                "entities/carol",
                "entity",
                "Carol",
                "Met [[entities/main|the dup]]."
            )
        );
    }

    #[test]
    fn merge_removes_the_source_page() {
        let v = merge_vault();
        merge_pages_with_store(v.path(), "entities/dup", "entities/main", &store()).unwrap();
        assert!(!wiki_dir(v.path()).join("entities/dup.md").exists());
    }

    #[test]
    fn merge_reports_the_redirected_pages_and_link_count() {
        let v = merge_vault();
        let out =
            merge_pages_with_store(v.path(), "entities/dup", "entities/main", &store()).unwrap();
        assert_eq!(
            (out.rewritten_pages, out.rewritten_links),
            (vec!["entities/carol".to_string()], 1)
        );
    }

    #[test]
    fn merge_refuses_to_merge_a_page_into_itself() {
        let v = merge_vault();
        let err = merge_pages_with_store(v.path(), "entities/main", "entities/main", &store())
            .unwrap_err();
        assert!(matches!(err, RefactorError::SameId(_)));
    }

    #[test]
    fn merge_produces_exactly_one_new_commit() {
        let v = merge_vault();
        let before = commit_count(v.path());
        merge_pages_with_store(v.path(), "entities/dup", "entities/main", &store()).unwrap();
        assert_eq!(commit_count(v.path()), before + 1);
    }

    fn put(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// Runs `f` with the thread-local failpoint armed at `at`.
    fn with_failure_at<T>(at: usize, f: impl FnOnce() -> T) -> T {
        FAIL_AT.with(|c| c.set(Some(at)));
        let out = f();
        FAIL_AT.with(|c| c.set(None));
        out
    }

    fn head_message(vault: &Path, back: usize) -> String {
        let repo = git2::Repository::open(wiki_dir(vault)).unwrap();
        let mut commit = repo.head().unwrap().peel_to_commit().unwrap();
        for _ in 0..back {
            commit = commit.parent(0).unwrap();
        }
        commit.message().unwrap_or("").to_string()
    }

    fn head_entity_names(vault: &Path) -> Vec<String> {
        let repo = git2::Repository::open(wiki_dir(vault)).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let sub = tree.get_path(Path::new("entities")).unwrap();
        let sub = repo.find_tree(sub.id()).unwrap();
        sub.iter()
            .filter_map(|e| e.name().map(str::to_string))
            .collect()
    }

    fn disk_entity_names(vault: &Path) -> Vec<String> {
        std::fs::read_dir(wiki_dir(vault).join("entities"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect()
    }

    // --- id validation (B1 / S3) --------------------------------------------

    /// A file outside the vault that a hostile id tries to reach.
    fn outside_file() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("notes.md");
        put(&file, "---\nid: entities/x\ntype: entity\n---\nprivate\n");
        (dir, file)
    }

    #[test]
    fn delete_rejects_an_absolute_drive_path_id_and_leaves_the_outside_file_alone() {
        let v = standard_vault();
        let (_dir, file) = outside_file();
        let id = file.with_extension("").to_string_lossy().replace('\\', "/");
        let _ = delete_page_with_store(v.path(), &id, true, &store());
        assert!(file.is_file(), "outside file must survive id '{id}'");
    }

    #[test]
    fn merge_rejects_an_absolute_path_source_and_leaves_the_outside_file_alone() {
        let v = standard_vault();
        let (_dir, file) = outside_file();
        let id = file.with_extension("").to_string_lossy().replace('\\', "/");
        let _ = merge_pages_with_store(v.path(), &id, "entities/alice", &store());
        assert!(file.is_file(), "outside file must survive id '{id}'");
    }

    #[test]
    fn validate_page_id_rejects_every_path_escape_shape() {
        let rejected: Vec<&str> = [
            "C:/Users/x/notes",
            "C:notes",
            "/abs",
            "..",
            "entities/../../x",
            "entities\\x",
            "entities/x\0",
            "entities/x\u{7}",
            "people/x",
            "entities//x",
            "entities/./x",
            "entities",
            "",
        ]
        .into_iter()
        .filter(|id| validate_page_id(id).is_ok())
        .collect();
        assert!(rejected.is_empty(), "accepted: {rejected:?}");
    }

    #[test]
    fn validate_page_id_accepts_an_ordinary_existing_id() {
        assert!(validate_page_id("entities/Dan Shapiro (CEO)").is_ok());
    }

    #[test]
    fn validate_new_page_id_rejects_characters_that_break_links_or_file_names() {
        let accepted: Vec<&str> = [
            "entities/dan shapiro",
            "entities/dan(ceo)",
            "entities/a<b",
            "entities/a\"b",
            "entities/a*b",
            "entities/a|b",
            "entities/a?b",
            "entities/a#b",
            "entities/trailing.",
            "entities/-lead",
            "entities/x.md",
        ]
        .into_iter()
        .filter(|id| validate_new_page_id(id).is_ok())
        .collect();
        assert!(accepted.is_empty(), "accepted: {accepted:?}");
    }

    #[test]
    fn validate_new_page_id_accepts_a_dotted_dashed_slug() {
        assert!(validate_new_page_id("sources/2026-05-11_v1.2-notes").is_ok());
    }

    #[test]
    fn type_dirs_match_the_vault_layout_and_the_lint_registry() {
        let dirs: Vec<&str> = TYPE_DIRS.iter().map(|(d, _)| *d).collect();
        let types: Vec<&str> = TYPE_DIRS.iter().map(|(_, t)| *t).collect();
        assert_eq!(
            (dirs.as_slice(), types.as_slice()),
            (WIKI_SUBDIRS, crate::wiki::lint::KNOWN_TYPES)
        );
    }

    // --- case-insensitive disks (B2) ----------------------------------------

    #[test]
    fn merge_refuses_ids_that_differ_only_in_case_and_keeps_the_page() {
        let v = merge_vault();
        let before = read(v.path(), "entities/main");
        let result = merge_pages_with_store(v.path(), "entities/MAIN", "entities/main", &store());
        assert!(
            result.is_err() && read(v.path(), "entities/main") == before,
            "result: {result:?}"
        );
    }

    #[test]
    fn delete_refuses_a_differently_cased_id_of_a_parseable_page() {
        let v = standard_vault();
        let _ = delete_page_with_store(v.path(), "entities/ALICE", true, &store());
        assert!(wiki_dir(v.path()).join("entities/alice.md").is_file());
    }

    #[test]
    fn delete_refuses_a_differently_cased_id_of_an_unparseable_page() {
        let v = standard_vault();
        put(
            &wiki_dir(v.path()).join("entities/Junk.md"),
            "no frontmatter",
        );
        let _ = delete_page_with_store(v.path(), "entities/junk", false, &store());
        assert!(disk_entity_names(v.path()).contains(&"Junk.md".to_string()));
    }

    #[test]
    fn delete_removes_an_unparseable_page_named_exactly_like_the_id() {
        let v = standard_vault();
        put(
            &wiki_dir(v.path()).join("entities/Junk.md"),
            "no frontmatter",
        );
        delete_page_with_store(v.path(), "entities/Junk", false, &store()).unwrap();
        assert!(!disk_entity_names(v.path()).contains(&"Junk.md".to_string()));
    }

    fn cased_vault() -> TempDir {
        vault_with(&[(
            "entities/Old",
            page("entities/Old", "entity", "Old", "Body."),
        )])
    }

    #[test]
    fn case_only_rename_leaves_the_file_under_the_new_spelling() {
        let v = cased_vault();
        rename_page_with_store(v.path(), "entities/Old", "entities/old", &store()).unwrap();
        let names = disk_entity_names(v.path());
        assert!(
            names.contains(&"old.md".to_string()) && !names.contains(&"Old.md".to_string()),
            "on disk: {names:?}"
        );
    }

    #[test]
    fn case_only_rename_records_the_new_spelling_in_the_commit() {
        let v = cased_vault();
        rename_page_with_store(v.path(), "entities/Old", "entities/old", &store()).unwrap();
        let names = head_entity_names(v.path());
        assert!(
            names.contains(&"old.md".to_string()) && !names.contains(&"Old.md".to_string()),
            "in HEAD: {names:?}"
        );
    }

    // --- checkpoint, rollback, commit failure (S1 / S2) ---------------------

    #[test]
    fn uncommitted_edits_are_checkpointed_before_a_delete() {
        let v = standard_vault();
        put(
            &wiki_dir(v.path()).join("entities/old-2.md"),
            &page(
                "entities/old-2",
                "entity",
                "Old 2",
                "edited, never committed",
            ),
        );
        delete_page_with_store(v.path(), "entities/old-2", true, &store()).unwrap();
        assert_eq!(head_message(v.path(), 1), CHECKPOINT_MESSAGE);
    }

    #[test]
    fn a_failed_write_rolls_back_the_already_moved_page() {
        let v = standard_vault();
        let old_before = read(v.path(), "entities/old");
        // Ops: 0 move, 1 write moved page, 2 rewrite alice → fail at 2.
        let result = with_failure_at(2, || {
            rename_page_with_store(v.path(), "entities/old", "entities/new", &store())
        });
        let wiki = wiki_dir(v.path());
        assert!(
            result.is_err()
                && read(v.path(), "entities/old") == old_before
                && !wiki.join("entities/new.md").exists(),
            "result: {result:?}"
        );
    }

    #[test]
    fn a_failed_delete_restores_the_pages_it_had_already_rewritten() {
        let v = standard_vault();
        let alice_before = read(v.path(), "entities/alice");
        // Ops: 0 defuse alice, 1 remove old → fail at 1.
        let _ = with_failure_at(1, || {
            delete_page_with_store(v.path(), "entities/old", true, &store())
        });
        assert_eq!(read(v.path(), "entities/alice"), alice_before);
    }

    #[test]
    fn a_failed_commit_still_reports_success_with_a_note() {
        let v = standard_vault();
        let out = with_failure_at(COMMIT_FAILPOINT, || {
            rename_page_with_store(v.path(), "entities/old", "entities/new", &store())
        })
        .unwrap();
        assert!(
            out.commit.is_none() && out.note.is_some(),
            "outcome: {out:?}"
        );
    }

    // --- delete + merge on an encrypted vault (S6) --------------------------

    #[test]
    fn delete_on_an_encrypted_vault_removes_the_opaque_path_from_head() {
        let s = store();
        let v = encrypted_vault(&s);
        let rel = page_relpath_with_store(v.path(), "entities/old", &s).unwrap();
        delete_page_with_store(v.path(), "entities/old", true, &s).unwrap();
        assert!(!head_has(v.path(), &rel));
    }

    #[test]
    fn merge_on_an_encrypted_vault_removes_the_source_opaque_path_from_head() {
        let s = store();
        let v = encrypted_vault(&s);
        let rel = page_relpath_with_store(v.path(), "entities/old", &s).unwrap();
        merge_pages_with_store(v.path(), "entities/old", "entities/alice", &s).unwrap();
        assert!(!head_has(v.path(), &rel));
    }

    // --- rewrite edge cases (S6 / nits) -------------------------------------

    #[test]
    fn rewrite_leaves_external_urls_and_image_files_untouched() {
        let text = "[ext](https://example.com/entities/old) ![pic](entities/old.png)";
        let (out, _) = rewrite_links(text, "entities/old", &LinkAction::Retarget("entities/new"));
        assert_eq!(out, text);
    }

    #[test]
    fn defusing_an_image_link_to_the_page_leaves_no_stray_bang() {
        let (out, _) = rewrite_links(
            "see ![Old](entities/old) here",
            "entities/old",
            &LinkAction::Defuse("x"),
        );
        assert_eq!(out, "see Old here");
    }

    #[test]
    fn link_shaped_text_inside_frontmatter_is_not_rewritten() {
        let raw = "---\nid: entities/a\ntype: entity\nrelated: \"[[entities/old]]\"\n---\n\nbody\n";
        let (out, _) =
            rewrite_page_links(raw, "entities/old", &LinkAction::Retarget("entities/new"));
        assert_eq!(out, raw);
    }

    #[test]
    fn frontmatter_edit_on_a_crlf_file_keeps_crlf() {
        let raw = "---\r\nid: entities/old\r\ntype: entity\r\n---\r\n\r\nbody\r\n";
        let out = set_frontmatter_scalar(raw, "id", "entities/new").unwrap();
        assert_eq!(
            out,
            "---\r\nid: entities/new\r\ntype: entity\r\n---\r\n\r\nbody\r\n"
        );
    }

    #[test]
    fn appending_tags_to_a_crlf_file_uses_crlf() {
        let raw = "---\r\nid: entities/a\r\ntype: entity\r\n---\r\n\r\nbody\r\n";
        let out = set_frontmatter_tags(raw, &["x".into()]).unwrap();
        assert_eq!(
            out,
            "---\r\nid: entities/a\r\ntype: entity\r\ntags: [\"x\"]\r\n---\r\n\r\nbody\r\n"
        );
    }

    #[test]
    fn merging_into_a_crlf_page_writes_no_bare_line_feeds() {
        let v = vault_with(&[
            (
                "entities/dup",
                "---\nid: entities/dup\ntype: entity\ntags: [n]\n---\n\nline one\nline two\n"
                    .to_string(),
            ),
            (
                "entities/main",
                "---\r\nid: entities/main\r\ntype: entity\r\n---\r\n\r\nMain.\r\n".to_string(),
            ),
        ]);
        merge_pages_with_store(v.path(), "entities/dup", "entities/main", &store()).unwrap();
        let merged = read(v.path(), "entities/main");
        assert_eq!(merged.matches('\n').count(), merged.matches("\r\n").count());
    }

    // --- superseded_by / sources references (B1) --------------------------

    fn page_fm(id: &str, extra: &str, body: &str) -> String {
        format!("---\nid: {id}\ntype: entity\ntitle: T\n{extra}---\n\n{body}\n")
    }

    // --- keep (H2) ----------------------------------------------------------

    fn keep_of(vault: &Path, id: &str) -> Option<bool> {
        parse(&read(vault, id)).unwrap().frontmatter.keep
    }

    #[test]
    fn rename_carries_the_keep_mark() {
        let v = vault_with(&[(
            "entities/old",
            page_fm("entities/old", "keep: true\n", "Body."),
        )]);
        rename_page_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        assert_eq!(keep_of(v.path(), "entities/new"), Some(true));
    }

    #[test]
    fn merge_keeps_the_keep_mark_of_the_folded_in_page() {
        let v = vault_with(&[
            (
                "entities/dup",
                page_fm("entities/dup", "keep: true\n", "Dup."),
            ),
            ("entities/main", page_fm("entities/main", "", "Main.")),
        ]);
        merge_pages_with_store(v.path(), "entities/dup", "entities/main", &store()).unwrap();
        assert_eq!(keep_of(v.path(), "entities/main"), Some(true));
    }

    #[test]
    fn merge_writes_keep_as_a_bare_yaml_boolean() {
        let v = vault_with(&[
            (
                "entities/dup",
                page_fm(
                    "entities/dup",
                    "keep: true
",
                    "Dup.",
                ),
            ),
            ("entities/main", page_fm("entities/main", "", "Main.")),
        ]);
        merge_pages_with_store(v.path(), "entities/dup", "entities/main", &store()).unwrap();
        assert!(read(v.path(), "entities/main").contains(
            "
keep: true
"
        ));
    }

    #[test]
    fn merge_keeps_the_keep_mark_of_the_surviving_page() {
        let v = vault_with(&[
            ("entities/dup", page_fm("entities/dup", "", "Dup.")),
            (
                "entities/main",
                page_fm("entities/main", "keep: true\n", "Main."),
            ),
        ]);
        merge_pages_with_store(v.path(), "entities/dup", "entities/main", &store()).unwrap();
        assert_eq!(keep_of(v.path(), "entities/main"), Some(true));
    }

    #[test]
    fn merge_of_two_pages_without_keep_adds_none() {
        let v = vault_with(&[
            ("entities/dup", page_fm("entities/dup", "", "Dup.")),
            ("entities/main", page_fm("entities/main", "", "Main.")),
        ]);
        merge_pages_with_store(v.path(), "entities/dup", "entities/main", &store()).unwrap();
        assert!(!read(v.path(), "entities/main").contains("keep:"));
    }

    fn reference_vault() -> TempDir {
        vault_with(&[
            ("entities/old", page_fm("entities/old", "", "Old body.")),
            (
                "entities/replaced",
                page_fm(
                    "entities/replaced",
                    "superseded_by: entities/old\n",
                    "Earlier.",
                ),
            ),
            (
                "entities/cited",
                page_fm(
                    "entities/cited",
                    "sources: [entities/old, sources/keep]\n",
                    "Facts.",
                ),
            ),
            (
                "entities/block",
                page_fm(
                    "entities/block",
                    "sources:\n  - entities/old\n  - sources/keep\n",
                    "Facts.",
                ),
            ),
            ("sources/keep", page_fm("sources/keep", "", "Source.")),
        ])
    }

    #[test]
    fn rename_points_superseded_by_at_the_new_id() {
        let v = reference_vault();
        rename_page_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        assert_eq!(
            read(v.path(), "entities/replaced"),
            page_fm(
                "entities/replaced",
                "superseded_by: entities/new\n",
                "Earlier."
            )
        );
    }

    #[test]
    fn rename_points_a_flow_sources_entry_at_the_new_id() {
        let v = reference_vault();
        rename_page_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        assert_eq!(
            read(v.path(), "entities/cited"),
            page_fm(
                "entities/cited",
                "sources: [\"entities/new\",\"sources/keep\"]\n",
                "Facts."
            )
        );
    }

    #[test]
    fn rename_points_a_block_sources_entry_at_the_new_id() {
        let v = reference_vault();
        rename_page_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        let parsed = parse(&read(v.path(), "entities/block")).unwrap();
        assert_eq!(
            parsed.frontmatter.sources,
            vec!["entities/new", "sources/keep"]
        );
    }

    #[test]
    fn rename_reports_the_rewritten_references() {
        let v = reference_vault();
        let out =
            rename_page_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        assert_eq!(out.rewritten_references, 3);
    }

    #[test]
    fn rename_points_a_single_string_superseded_by_written_as_a_wiki_link_at_the_new_id() {
        let v = vault_with(&[
            ("entities/old", page_fm("entities/old", "", "Old.")),
            (
                "entities/a",
                page_fm("entities/a", "superseded_by: \"[[entities/old]]\"\n", "A."),
            ),
        ]);
        rename_page_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        let parsed = parse(&read(v.path(), "entities/a")).unwrap();
        assert_eq!(
            parsed.frontmatter.superseded_by.as_deref(),
            Some("entities/new")
        );
    }

    #[test]
    fn merge_points_superseded_by_at_the_surviving_page() {
        let v = reference_vault();
        merge_pages_with_store(v.path(), "entities/old", "entities/cited", &store()).unwrap();
        let parsed = parse(&read(v.path(), "entities/replaced")).unwrap();
        assert_eq!(
            parsed.frontmatter.superseded_by.as_deref(),
            Some("entities/cited")
        );
    }

    #[test]
    fn merge_drops_a_sources_entry_of_the_survivor_that_named_the_merged_page() {
        let v = reference_vault();
        merge_pages_with_store(v.path(), "entities/old", "entities/cited", &store()).unwrap();
        let parsed = parse(&read(v.path(), "entities/cited")).unwrap();
        assert_eq!(parsed.frontmatter.sources, vec!["sources/keep"]);
    }

    #[test]
    fn merge_keeps_the_merged_page_id_and_aliases_as_aliases_of_the_survivor() {
        let v = vault_with(&[
            (
                "entities/old",
                page_fm("entities/old", "aliases: [Old Corp]\n", "Old."),
            ),
            (
                "entities/new",
                page_fm("entities/new", "aliases: [New Corp]\n", "New."),
            ),
        ]);
        merge_pages_with_store(v.path(), "entities/old", "entities/new", &store()).unwrap();
        let parsed = parse(&read(v.path(), "entities/new")).unwrap();
        assert_eq!(
            parsed.frontmatter.aliases,
            vec!["New Corp", "Old Corp", "entities/old"]
        );
    }

    #[test]
    fn delete_refuses_while_a_page_is_superseded_by_it() {
        let v = vault_with(&[
            ("entities/old", page_fm("entities/old", "", "Old.")),
            (
                "entities/a",
                page_fm("entities/a", "superseded_by: entities/old\n", "A."),
            ),
        ]);
        let err = delete_page_with_store(v.path(), "entities/old", false, &store()).unwrap_err();
        assert!(matches!(
            err,
            RefactorError::StillLinked { ref referrers, .. } if referrers == &vec!["entities/a".to_string()]
        ));
    }

    #[test]
    fn delete_refuses_while_a_page_names_it_in_sources() {
        let v = vault_with(&[
            ("sources/s", page_fm("sources/s", "", "S.")),
            (
                "entities/a",
                page_fm("entities/a", "sources: [sources/s]\n", "A."),
            ),
        ]);
        let err = delete_page_with_store(v.path(), "sources/s", false, &store()).unwrap_err();
        assert!(matches!(err, RefactorError::StillLinked { .. }));
    }

    #[test]
    fn forced_delete_removes_the_superseded_by_line_and_nothing_else() {
        let v = vault_with(&[
            ("entities/old", page_fm("entities/old", "", "Old.")),
            (
                "entities/a",
                page_fm(
                    "entities/a",
                    "superseded_by: entities/old\nvalid_to: 2025-01-01\n",
                    "A.",
                ),
            ),
        ]);
        delete_page_with_store(v.path(), "entities/old", true, &store()).unwrap();
        assert_eq!(
            read(v.path(), "entities/a"),
            page_fm("entities/a", "valid_to: 2025-01-01\n", "A.")
        );
    }

    #[test]
    fn forced_delete_of_the_last_entry_leaves_no_blank_line_before_the_fence() {
        let v = vault_with(&[
            ("entities/old", page_fm("entities/old", "", "Old.")),
            (
                "entities/a",
                page_fm("entities/a", "superseded_by: entities/old\n", "A."),
            ),
        ]);
        delete_page_with_store(v.path(), "entities/old", true, &store()).unwrap();
        assert_eq!(
            read(v.path(), "entities/a"),
            page_fm("entities/a", "", "A.")
        );
    }

    #[test]
    fn forced_delete_removes_only_the_sources_entry_of_the_deleted_page() {
        let v = vault_with(&[
            ("sources/s", page_fm("sources/s", "", "S.")),
            ("sources/t", page_fm("sources/t", "", "T.")),
            (
                "entities/a",
                page_fm("entities/a", "sources: [sources/s, sources/t]\n", "A."),
            ),
        ]);
        delete_page_with_store(v.path(), "sources/s", true, &store()).unwrap();
        let parsed = parse(&read(v.path(), "entities/a")).unwrap();
        assert_eq!(parsed.frontmatter.sources, vec!["sources/t"]);
    }

    #[test]
    fn remove_top_level_entry_keeps_crlf_line_endings() {
        let raw = "---\r\nid: entities/a\r\nsuperseded_by: entities/old\r\n---\r\n\r\nA.\r\n";
        let out = set_frontmatter_entry(raw, "superseded_by", None).unwrap();
        assert_eq!(out, "---\r\nid: entities/a\r\n---\r\n\r\nA.\r\n");
    }
}
