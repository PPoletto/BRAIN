//! Wiki page parsing — frontmatter + body + wiki link extraction.

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_yaml::Value as YamlValue;

use super::{WikiError, WikiResult};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageFrontmatter {
    pub id: String,
    #[serde(rename = "type")]
    pub page_type: String,
    pub title: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created: Option<String>,
    pub updated: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ParsedPage {
    pub frontmatter: PageFrontmatter,
    pub frontmatter_raw: YamlValue,
    pub body: String,
    pub wiki_links: Vec<String>,
}

const FRONTMATTER_DELIM: &str = "---";

/// Parses a Markdown file with required YAML frontmatter.
pub fn parse(raw: &str) -> WikiResult<ParsedPage> {
    let trimmed = raw.trim_start_matches('\u{feff}');
    if !trimmed.starts_with(FRONTMATTER_DELIM) {
        return Err(WikiError::Lint("missing frontmatter delimiter".into()));
    }
    let after_first = &trimmed[FRONTMATTER_DELIM.len()..];
    let after_first = after_first.trim_start_matches('\n');
    let end = after_first
        .find(&format!("\n{}\n", FRONTMATTER_DELIM))
        .or_else(|| after_first.find(&format!("\n{}", FRONTMATTER_DELIM)))
        .ok_or_else(|| WikiError::Lint("frontmatter not closed".into()))?;
    let yaml = &after_first[..end];
    let body = &after_first[end..];
    let body = body
        .trim_start_matches('\n')
        .trim_start_matches(FRONTMATTER_DELIM)
        .trim_start_matches('\n');

    let frontmatter_raw: YamlValue = serde_yaml::from_str(yaml)?;
    let frontmatter: PageFrontmatter = serde_yaml::from_str(yaml)?;
    let wiki_links = extract_wiki_links(body);
    Ok(ParsedPage {
        frontmatter,
        frontmatter_raw,
        body: body.to_string(),
        wiki_links,
    })
}

/// Type prefixes a valid page-id can start with. Mirrors
/// `vault::layout::WIKI_SUBDIRS`; duplicated here so the parser doesn't
/// need to depend on the layout module.
const WIKI_TYPE_PREFIXES: &[&str] = &["entities/", "concepts/", "sources/", "topics/"];

/// Extracts page references from a body. Two syntaxes are recognised:
///
///  1. `[[entities/dan-shapiro]]` — the canonical wiki-link form.
///  2. `[Display Text](entities/dan-shapiro)` — standard Markdown link
///     whose destination *looks like* a wiki page id (starts with a
///     known type prefix, no URL scheme, no leading slash). LLMs writing
///     pages via MCP often emit this form by default — accepting it
///     means the graph and backlinks see those edges, and the viewer
///     can route the click in-app instead of letting the webview
///     navigate to a 404.
///
/// External links (`https://…`, `mailto:…`), in-page anchors (`#…`) and
/// absolute paths (`/foo`) are filtered out. The string returned is the
/// raw destination, with any trailing `.md` stripped so it matches the
/// id stored in `pages.id`.
pub fn extract_wiki_links(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // Wiki-link syntax.
    let wiki = Regex::new(r"\[\[([^\[\]\|]+?)(?:\|[^\]]*)?\]\]").expect("regex");
    for cap in wiki.captures_iter(body) {
        out.push(cap[1].trim().to_string());
    }

    // Markdown-link syntax with a wiki-shaped destination.
    let md = Regex::new(
        r#"\[(?:[^\]]*)\]\(\s*([^)\s]+)(?:\s+"[^"]*")?\s*\)"#,
    )
    .expect("regex");
    for cap in md.captures_iter(body) {
        let raw = cap[1].trim();
        if let Some(id) = page_id_from_markdown_target(raw) {
            out.push(id);
        }
    }

    out
}

/// True when `raw` is a markdown link destination shaped like a wiki
/// page id. Thin wrapper over `page_id_from_markdown_target` exposed
/// for the lint pass.
pub fn looks_like_wiki_page_target(raw: &str) -> bool {
    page_id_from_markdown_target(raw).is_some()
}

/// Maps a markdown-link destination to a wiki page-id, or `None` if it's
/// clearly not one (external URL, anchor, absolute path, etc.).
///
/// Tolerates the absolute form `http://tauri.localhost/<id>` — that's
/// what the Tauri webview expands a relative href to at click time, and
/// older pages can sneak that into their bodies if a user copy-pasted
/// from the address bar.
fn page_id_from_markdown_target(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.starts_with('#') || raw.starts_with('/') {
        return None;
    }
    if raw.starts_with("mailto:") || raw.starts_with("file:") || raw.starts_with("javascript:") {
        return None;
    }

    // Defensive: strip the Tauri webview's resolved-absolute prefix.
    let stripped = raw
        .strip_prefix("http://tauri.localhost/")
        .or_else(|| raw.strip_prefix("https://tauri.localhost/"))
        .unwrap_or(raw);

    if stripped.starts_with("http://") || stripped.starts_with("https://") {
        return None;
    }

    // Drop query/fragment.
    let stripped = stripped
        .split(['?', '#'])
        .next()
        .unwrap_or(stripped);
    // Drop optional `.md` suffix so `entities/alice.md` and
    // `entities/alice` collapse to the same id.
    let stripped = stripped.strip_suffix(".md").unwrap_or(stripped);

    // Sanity: must start with a known wiki type prefix to count as a
    // page-id. Keeps `[Github](https://github.com/...)` and other
    // ambiguous cases out — they get filtered earlier by the scheme
    // check anyway, but this is the belt to that suspenders.
    if !WIKI_TYPE_PREFIXES.iter().any(|p| stripped.starts_with(p)) {
        return None;
    }

    Some(stripped.to_string())
}

/// Rewrites every standard markdown link whose target is a wiki page-id
/// into the canonical `[[type/slug]]` (or `[[type/slug|alias]]` when the
/// link text differs from the slug's last segment) form.
///
/// External links, anchors, absolute paths and links to non-wiki targets
/// stay as-is. Code spans (`` `…` ``) and fenced code blocks (```` ```…```` )
/// are left alone — link-like sequences inside example code shouldn't
/// be silently rewritten.
///
/// Idempotent: running it twice produces the same output as running it
/// once. New input that already uses `[[wiki-links]]` is unchanged.
pub fn normalize_internal_links(body: &str) -> String {
    let md_link = Regex::new(
        r#"(?P<text>\[(?:[^\]]*)\])\(\s*(?P<target>[^)\s]+)(?:\s+"[^"]*")?\s*\)"#,
    )
    .expect("regex");

    // Walk the body once and replace links only when we're outside any
    // code fence or backtick span. A small state machine is enough — we
    // don't need a full markdown parser here, just a "are we currently
    // inside code" flag.
    let mut out = String::with_capacity(body.len());
    let mut chars = body.char_indices().peekable();
    let mut buf_start = 0usize;

    while let Some(&(i, c)) = chars.peek() {
        // Detect fence boundaries (``` at start of line, optionally
        // preceded by whitespace).
        if c == '`'
            && body[i..].starts_with("```")
            && (i == 0 || body[..i].ends_with('\n'))
        {
            // Flush buffered non-code chunk through the link regex.
            out.push_str(&rewrite_md_links_in(&body[buf_start..i], &md_link));
            // Find the matching end-of-fence.
            let after = i + 3;
            let close_rel = body[after..].find("\n```").map(|n| after + n + 4);
            match close_rel {
                Some(end) => {
                    out.push_str(&body[i..end]);
                    buf_start = end;
                    // Advance the iterator past the fence.
                    while let Some(&(j, _)) = chars.peek() {
                        if j >= end {
                            break;
                        }
                        chars.next();
                    }
                    continue;
                }
                None => {
                    // Unclosed fence — bail and keep the rest verbatim.
                    out.push_str(&body[i..]);
                    return out;
                }
            }
        }

        // Inline code: skip until the matching backtick on the same line.
        // Doesn't handle multi-backtick runs (`` ` ``) — fine for our
        // use case; worst-case the regex below also doesn't match those
        // because their content rarely looks like a link.
        if c == '`' {
            // Flush.
            out.push_str(&rewrite_md_links_in(&body[buf_start..i], &md_link));
            let after = i + 1;
            let close_rel = body[after..]
                .find('`')
                .map(|n| after + n + 1);
            match close_rel {
                Some(end) => {
                    out.push_str(&body[i..end]);
                    buf_start = end;
                    while let Some(&(j, _)) = chars.peek() {
                        if j >= end {
                            break;
                        }
                        chars.next();
                    }
                    continue;
                }
                None => {
                    // No closing backtick — bail.
                    out.push_str(&body[i..]);
                    return out;
                }
            }
        }
        chars.next();
    }
    // Flush remainder.
    out.push_str(&rewrite_md_links_in(&body[buf_start..], &md_link));
    out
}

fn rewrite_md_links_in(chunk: &str, re: &Regex) -> String {
    // Walk line-by-line so we can defensively skip markdown table rows.
    // Reason: pipe-form wiki links `[[id|alias]]` collide with the
    // table column separator `|`. If we'd rewrite a `[Foo](entities/foo)`
    // inside a `| … |` row, the resulting `[[entities/foo|Foo]]`
    // contains a literal `|` that the GFM table parser interprets as
    // a new column boundary — silently corrupts the rendered table.
    // Leaving the markdown-style link verbatim inside table cells is
    // safe (renderer handles it, extract_wiki_links still picks it up
    // as a graph edge) and avoids the breakage.
    let mut out = String::with_capacity(chunk.len());
    for line in chunk.split_inclusive('\n') {
        if is_markdown_table_row(line) {
            out.push_str(line);
            continue;
        }
        let rewritten = re.replace_all(line, |caps: &regex::Captures<'_>| {
            let text = &caps["text"]; // includes the brackets, e.g. "[Dan]"
            let target = caps["target"].trim();
            let Some(page_id) = page_id_from_markdown_target(target) else {
                // Not a wiki page — keep the original markdown link verbatim.
                return caps[0].to_string();
            };
            // text is `[<label>]`; pull the inside.
            let label = text.trim_start_matches('[').trim_end_matches(']');
            // Use the slug's last segment for the "natural" comparison so
            // [Dan](entities/dan-shapiro) keeps the alias `Dan` rather than
            // collapsing to `[[entities/dan-shapiro]]` (which would render
            // as "entities/dan-shapiro").
            let slug_tail = page_id.rsplit('/').next().unwrap_or(&page_id);
            if label == slug_tail || label == page_id {
                format!("[[{page_id}]]")
            } else {
                format!("[[{page_id}|{label}]]")
            }
        });
        out.push_str(&rewritten);
    }
    out
}

/// Heuristic: a markdown table row starts AND ends with `|` (after
/// trimming whitespace and the trailing newline). Both anchors are
/// required so legitimate non-table lines that happen to start with
/// `|` (e.g. text wrapping in a quote block) don't accidentally
/// trigger the skip. The trailing-newline trim handles
/// `split_inclusive` keeping the `\n` glued to the line.
fn is_markdown_table_row(line: &str) -> bool {
    let trimmed = line.trim_end_matches('\n').trim();
    !trimmed.is_empty() && trimmed.starts_with('|') && trimmed.ends_with('|')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nid: entities/dan-shapiro\ntype: entity\ntitle: Dan Shapiro\ntags: [strongdm]\ncreated: 2026-04-29\nupdated: 2026-04-29\n---\n\nDan is a [[concepts/nlspec]] practitioner. See [[entities/serina]].\n";

    #[test]
    fn parses_frontmatter_and_body_when_well_formed() {
        let p = parse(SAMPLE).unwrap();
        assert_eq!(p.frontmatter.id, "entities/dan-shapiro");
        assert_eq!(p.frontmatter.page_type, "entity");
        assert_eq!(p.frontmatter.tags, vec!["strongdm"]);
        assert!(p.body.contains("Dan is a"));
    }

    #[test]
    fn extracts_all_wiki_link_targets_in_order() {
        let p = parse(SAMPLE).unwrap();
        assert_eq!(p.wiki_links, vec!["concepts/nlspec", "entities/serina"]);
    }

    #[test]
    fn parse_rejects_input_without_frontmatter_delimiter() {
        let err = parse("# no frontmatter here").unwrap_err();
        assert!(matches!(err, WikiError::Lint(_)));
    }

    #[test]
    fn parse_rejects_unclosed_frontmatter_block() {
        let err = parse("---\nid: x\ntype: entity\n").unwrap_err();
        assert!(matches!(err, WikiError::Lint(_)));
    }

    #[test]
    fn extract_wiki_links_handles_pipe_aliases() {
        let links = extract_wiki_links("see [[entities/dan-shapiro|Dan]]");
        assert_eq!(links, vec!["entities/dan-shapiro"]);
    }

    #[test]
    fn extract_wiki_links_picks_up_markdown_style_links_to_wiki_pages() {
        // LLMs writing via MCP often emit standard Markdown link syntax
        // instead of [[wiki-links]]. We must recognise those too,
        // otherwise the graph view ends up edge-less even though the
        // text clearly references other pages.
        let body = "Dan is a [methodology practitioner](concepts/nlspec). \
                    See [Serina](entities/serina.md).";
        let links = extract_wiki_links(body);
        assert!(
            links.contains(&"concepts/nlspec".to_string()),
            "bare markdown link missing: {links:?}"
        );
        assert!(
            links.contains(&"entities/serina".to_string()),
            ".md-suffixed markdown link missing: {links:?}"
        );
    }

    #[test]
    fn extract_wiki_links_ignores_external_and_anchor_links() {
        let body = "external [GitHub](https://github.com/x), \
                    anchor [top](#top), \
                    abs [foo](/foo), \
                    mailto [me](mailto:a@b.c)";
        let links = extract_wiki_links(body);
        assert!(
            links.is_empty(),
            "external/anchor/abs/mailto links leaked through: {links:?}"
        );
    }

    #[test]
    fn extract_wiki_links_strips_tauri_localhost_prefix() {
        // The webview resolves a relative href to an absolute
        // `http://tauri.localhost/...` URL at click time. If that ever
        // round-trips into a page body (e.g. paste from the address
        // bar), we should still recognise it as a page id.
        let body = "see [Dark Factory](http://tauri.localhost/concepts/dark-factory)";
        let links = extract_wiki_links(body);
        assert_eq!(links, vec!["concepts/dark-factory"]);
    }

    #[test]
    fn normalize_rewrites_markdown_link_with_alias_into_pipe_form() {
        let out = normalize_internal_links("Hi [Dan](entities/dan-shapiro)!");
        assert_eq!(out, "Hi [[entities/dan-shapiro|Dan]]!");
    }

    #[test]
    fn normalize_collapses_to_short_form_when_label_equals_slug_tail() {
        let out = normalize_internal_links("see [dan-shapiro](entities/dan-shapiro)");
        assert_eq!(out, "see [[entities/dan-shapiro]]");
    }

    #[test]
    fn normalize_strips_trailing_md_extension_in_target() {
        let out = normalize_internal_links("see [Dan](entities/dan-shapiro.md)");
        assert_eq!(out, "see [[entities/dan-shapiro|Dan]]");
    }

    #[test]
    fn normalize_strips_resolved_tauri_localhost_prefix() {
        let out = normalize_internal_links(
            "see [Dark Factory](http://tauri.localhost/concepts/dark-factory)",
        );
        assert_eq!(out, "see [[concepts/dark-factory|Dark Factory]]");
    }

    #[test]
    fn normalize_leaves_external_links_untouched() {
        let body = "see [GitHub](https://github.com/foo/bar) and [tel](tel:1234)";
        let out = normalize_internal_links(body);
        assert_eq!(out, body);
    }

    #[test]
    fn normalize_leaves_anchors_and_absolute_paths_untouched() {
        let body = "[top](#top), [foo](/foo/bar.md), [img](images/x.png)";
        let out = normalize_internal_links(body);
        assert_eq!(out, body);
    }

    #[test]
    fn normalize_does_not_touch_existing_wiki_link_syntax() {
        let body = "already [[entities/dan-shapiro|Dan]] in canonical form";
        let out = normalize_internal_links(body);
        assert_eq!(out, body);
    }

    #[test]
    fn normalize_is_idempotent() {
        let body = "Hi [Dan](entities/dan-shapiro), see [Bob](entities/bob).";
        let once = normalize_internal_links(body);
        let twice = normalize_internal_links(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn normalize_skips_links_inside_inline_code() {
        // Code-formatted link example must NOT be rewritten — it's
        // documentation, not an actual page reference.
        let body = "Use `[Dan](entities/dan-shapiro)` style only outside code.";
        let out = normalize_internal_links(body);
        assert_eq!(out, body);
    }

    #[test]
    fn normalize_skips_links_inside_fenced_code_block() {
        let body =
            "Example:\n```markdown\n[Dan](entities/dan-shapiro)\n```\nLive: [Dan](entities/dan-shapiro)";
        let out = normalize_internal_links(body);
        assert!(out.contains("```markdown\n[Dan](entities/dan-shapiro)\n```"));
        assert!(out.contains("Live: [[entities/dan-shapiro|Dan]]"));
    }

    #[test]
    fn normalize_handles_multiple_links_on_one_line() {
        let body = "[A](entities/a) met [B](entities/b)";
        let out = normalize_internal_links(body);
        assert_eq!(out, "[[entities/a|A]] met [[entities/b|B]]");
    }

    #[test]
    fn normalize_does_not_rewrite_links_inside_markdown_table_rows() {
        // Regression for the silent table-corruption bug. Pre-fix the
        // normalizer would rewrite [Dan](entities/dan-shapiro) to
        // [[entities/dan-shapiro|Dan]] EVEN inside a table cell — and
        // the resulting `|Dan]]` collides with the table's column
        // separator, breaking the row's rendering in the viewer (and in
        // every standard GFM renderer). Inside a `| … | … |` row we now
        // leave the line verbatim so the markdown link stays as the
        // safe alias-free form. The graph view still picks up the edge
        // because extract_wiki_links recognises markdown-style links.
        let body = "\
| Person | Role |
|--------|------|
| [Dan](entities/dan-shapiro) | CEO |
";
        let out = normalize_internal_links(body);
        assert!(
            !out.contains("[[entities/dan-shapiro|Dan]]"),
            "table-cell link must NOT be rewritten to pipe-form, got:\n{out}"
        );
        // The original markdown form is preserved inside the table cell.
        assert!(
            out.contains("[Dan](entities/dan-shapiro)"),
            "markdown-style link inside table must be left verbatim, got:\n{out}"
        );
    }

    #[test]
    fn normalize_still_rewrites_links_in_prose_after_a_table() {
        // Defensive: the table-row skip must be local to the table — a
        // paragraph that comes after the table must continue to be
        // normalized so the rest of the page benefits from the
        // canonical `[[…]]` form everywhere it's safe.
        let body = "\
| Header |
|--------|
| cell |

Then [Dan](entities/dan-shapiro) appears in prose.
";
        let out = normalize_internal_links(body);
        assert!(
            out.contains("[[entities/dan-shapiro|Dan]]"),
            "post-table prose must still be normalized, got:\n{out}"
        );
    }

    #[test]
    fn normalize_still_rewrites_links_in_prose_before_a_table() {
        // Mirror of the above: prose BEFORE a table must also continue
        // to be normalized. Tables shouldn't poison adjacent content
        // in either direction.
        let body = "\
Intro mentions [Dan](entities/dan-shapiro).

| Header |
|--------|
| cell |
";
        let out = normalize_internal_links(body);
        assert!(
            out.contains("[[entities/dan-shapiro|Dan]]"),
            "pre-table prose must still be normalized, got:\n{out}"
        );
    }

    #[test]
    fn normalize_leaves_existing_pipe_form_inside_table_cells_unchanged() {
        // If the user / a prior write already used [[id|alias]] inside
        // a table cell (manually, perhaps relying on a renderer that
        // tolerates it, or via a cell that escapes pipes upstream),
        // the normalizer must not touch it on a re-write — same
        // idempotency contract as for normal text.
        let body = "\
| Person | Role |
|--------|------|
| [[entities/dan-shapiro|Dan]] | CEO |
";
        let out = normalize_internal_links(body);
        assert_eq!(out, body);
    }
}
