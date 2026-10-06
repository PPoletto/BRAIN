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
    /// Alternative names of the page (A2). Matched by
    /// `brain_lookup` / the create-duplicate check via [`slug_key`].
    /// Lenient: a single string or a list of scalars is accepted.
    #[serde(default, deserialize_with = "de_string_list", skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// First day the page's facts hold (`YYYY-MM-DD`, Slice C).
    #[serde(default, deserialize_with = "de_opt_scalar", skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    /// Last day the page's facts hold (`YYYY-MM-DD`, Slice C).
    #[serde(default, deserialize_with = "de_opt_scalar", skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<String>,
    /// Id of the page that replaces this one (Slice C). `[[id]]` is
    /// accepted and stored as `id`.
    #[serde(default, deserialize_with = "de_opt_page_ref", skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Ids of the source pages the facts come from (Slice C). `[[id]]`
    /// entries are accepted and stored as `id`.
    #[serde(default, deserialize_with = "de_page_ref_list", skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// Ids of pages that share a name with this one but are a different
    /// thing (A2): silences the `alias-collision` lint for those pairs.
    #[serde(default, deserialize_with = "de_page_ref_list", skip_serializing_if = "Vec::is_empty")]
    pub distinct_from: Vec<String>,
    /// One or two sentences saying what the page is about (B2), written
    /// by the agent. Indexed as its own, higher-weighted FTS column and
    /// appended to every chunk's embedding context header. A list keeps
    /// its first entry; an empty value counts as absent.
    #[serde(default, deserialize_with = "de_opt_scalar", skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Every other frontmatter key (e.g. `status`), kept so JSON views
    /// of the frontmatter (`brain_get_pages`, `pages.frontmatter`) do not
    /// silently drop fields no named member covers. On disk the
    /// frontmatter text is never re-serialised, so nothing is lost there.
    /// Held as JSON values, converted at parse time: non-string keys
    /// (`2025: …`, also in nested maps) are kept under their text form and
    /// keys that are not scalars are dropped, so neither the parse nor the
    /// JSON serialisation can fail on unusual YAML.
    #[serde(flatten, deserialize_with = "de_extra")]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

fn de_extra<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<std::collections::BTreeMap<String, serde_json::Value>, D::Error> {
    let mapping = serde_yaml::Mapping::deserialize(d)?;
    Ok(mapping
        .into_iter()
        .filter_map(|(k, v)| Some((yaml_key(k)?, yaml_to_json(v))))
        .collect())
}

/// A YAML mapping key as text; `None` for keys that are not scalars.
fn yaml_key(k: YamlValue) -> Option<String> {
    match k {
        YamlValue::String(s) => Some(s),
        YamlValue::Number(n) => Some(n.to_string()),
        YamlValue::Bool(b) => Some(b.to_string()),
        YamlValue::Null => Some("null".to_string()),
        YamlValue::Tagged(t) => yaml_key(t.value),
        _ => None,
    }
}

/// YAML → JSON with every mapping key turned into text ([`yaml_key`]).
fn yaml_to_json(v: YamlValue) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        YamlValue::Null => J::Null,
        YamlValue::Bool(b) => J::Bool(b),
        YamlValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                J::from(i)
            } else if let Some(u) = n.as_u64() {
                J::from(u)
            } else {
                n.as_f64()
                    .and_then(serde_json::Number::from_f64)
                    .map(J::Number)
                    .unwrap_or_else(|| J::String(n.to_string()))
            }
        }
        YamlValue::String(s) => J::String(s),
        YamlValue::Sequence(items) => J::Array(items.into_iter().map(yaml_to_json).collect()),
        YamlValue::Mapping(map) => J::Object(
            map.into_iter()
                .filter_map(|(k, v)| Some((yaml_key(k)?, yaml_to_json(v))))
                .collect(),
        ),
        YamlValue::Tagged(t) => yaml_to_json(t.value),
    }
}

/// Scalars of a YAML value as strings: a scalar yields itself, a
/// sequence its scalar entries (nested sequences are flattened — an
/// unquoted `[[id]]` parses as a list inside a list), null nothing.
/// Mappings are ignored.
fn yaml_scalars(value: YamlValue, out: &mut Vec<String>) {
    match value {
        YamlValue::Null => {}
        YamlValue::Bool(b) => out.push(b.to_string()),
        YamlValue::Number(n) => out.push(n.to_string()),
        YamlValue::String(s) => out.push(s),
        YamlValue::Sequence(items) => {
            for item in items {
                yaml_scalars(item, out);
            }
        }
        YamlValue::Tagged(tagged) => yaml_scalars(tagged.value, out),
        YamlValue::Mapping(_) => {}
    }
}

fn clean_entries(raw: Vec<String>, page_ref: bool) -> Vec<String> {
    raw.into_iter()
        .map(|s| if page_ref { strip_page_ref(&s) } else { s.trim().to_string() })
        .filter(|s| !s.is_empty())
        .collect()
}

/// `[[entities/x]]`, `[[entities/x|Alias]]` or `entities/x.md` → `entities/x`.
fn strip_page_ref(raw: &str) -> String {
    let s = raw.trim();
    let s = s.strip_prefix("[[").unwrap_or(s);
    let s = s.strip_suffix("]]").unwrap_or(s);
    let s = s.split('|').next().unwrap_or(s).trim();
    s.strip_suffix(".md").unwrap_or(s).to_string()
}

fn de_list<'de, D: serde::Deserializer<'de>>(d: D, page_ref: bool) -> Result<Vec<String>, D::Error> {
    let value = Option::<YamlValue>::deserialize(d)?;
    let mut raw = Vec::new();
    if let Some(v) = value {
        yaml_scalars(v, &mut raw);
    }
    Ok(clean_entries(raw, page_ref))
}

fn de_string_list<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    de_list(d, false)
}

fn de_page_ref_list<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    de_list(d, true)
}

fn de_opt_scalar<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(de_list(d, false)?.into_iter().next())
}

fn de_opt_page_ref<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(de_list(d, true)?.into_iter().next())
}

/// Normalised form of a page slug, alias or title for duplicate
/// matching (A2): lowercase, `ä→ae ö→oe ü→ue ß→ss`, every character that
/// is not a letter or digit (`_`, space, `.`, `,`, `&`, `/` …) → `-`,
/// repeated `-` collapsed, leading/trailing `-` trimmed.
/// `"Müller_GmbH"`, `"Mueller GmbH."` and `"mueller-gmbh"` share the key
/// `mueller-gmbh`.
pub fn slug_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        match c {
            'ä' => out.push_str("ae"),
            'ö' => out.push_str("oe"),
            'ü' => out.push_str("ue"),
            'ß' => out.push_str("ss"),
            c if c.is_alphanumeric() => out.push(c),
            _ => out.push('-'),
        }
    }
    let mut collapsed = String::with_capacity(out.len());
    for c in out.chars() {
        if c == '-' && collapsed.ends_with('-') {
            continue;
        }
        collapsed.push(c);
    }
    collapsed.trim_matches('-').to_string()
}

/// [`slug_key`] of an alias. An alias written as a page id
/// (`entities/old-name`, e.g. the id a merge folded in) is keyed by its
/// slug, so it compares with page slugs.
pub fn alias_key(alias: &str) -> String {
    let trimmed = alias.trim();
    let slug = WIKI_TYPE_PREFIXES
        .iter()
        .find_map(|p| trimmed.strip_prefix(p))
        .unwrap_or(trimmed);
    slug_key(slug)
}

/// [`slug_key`] with the umlaut transliterations folded to the bare
/// vowel (`ae→a`, `oe→o`, `ue→u`), so the two common spellings of a
/// German name (`mueller` / `muller`) compare equal. Used next to
/// `slug_key` equality for the "normalised" duplicate class.
pub fn umlaut_folded_key(key: &str) -> String {
    key.replace("ae", "a").replace("oe", "o").replace("ue", "u")
}

/// Levenshtein edit distance over chars.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
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
pub(crate) fn page_id_from_markdown_target(raw: &str) -> Option<String> {
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

    // ---- A2 / C: name keys and optional frontmatter fields --------------

    fn frontmatter_with(extra: &str) -> PageFrontmatter {
        parse(&format!("---\nid: entities/x\ntype: entity\n{extra}---\n\nbody\n"))
            .unwrap()
            .frontmatter
    }

    #[test]
    fn slug_key_lowercases_and_transliterates_umlauts_and_sharp_s() {
        assert_eq!(slug_key("Müller Öl Übergröße"), "mueller-oel-uebergroesse");
    }

    #[test]
    fn slug_key_turns_underscores_and_spaces_into_single_hyphens() {
        assert_eq!(slug_key("__Foo  _ Bar--Baz_"), "foo-bar-baz");
    }

    #[test]
    fn slug_key_of_an_umlaut_slug_equals_its_transliterated_slug() {
        assert_eq!(slug_key("Müller_GmbH"), slug_key("mueller-gmbh"));
    }

    #[test]
    fn the_umlaut_folded_key_equates_ue_and_u() {
        assert_eq!(umlaut_folded_key("mueller-gmbh"), umlaut_folded_key("muller-gmbh"));
    }

    #[test]
    fn levenshtein_counts_one_dropped_letter_as_one_edit() {
        assert_eq!(levenshtein("dan-shapiro", "dan-shapio"), 1);
    }

    #[test]
    fn aliases_accept_a_single_string() {
        assert_eq!(frontmatter_with("aliases: Acme Corp\n").aliases, vec!["Acme Corp"]);
    }

    #[test]
    fn aliases_accept_a_list_with_numbers() {
        assert_eq!(frontmatter_with("aliases: [Acme, 2024]\n").aliases, vec!["Acme", "2024"]);
    }

    #[test]
    fn an_unquoted_wiki_link_in_superseded_by_is_read_as_the_page_id() {
        assert_eq!(
            frontmatter_with("superseded_by: [[entities/b]]\n").superseded_by.as_deref(),
            Some("entities/b")
        );
    }

    #[test]
    fn sources_drop_wiki_link_brackets_and_md_suffixes() {
        assert_eq!(
            frontmatter_with("sources: [\"[[sources/a|A]]\", sources/b.md]\n").sources,
            vec!["sources/a", "sources/b"]
        );
    }

    #[test]
    fn validity_dates_are_read_as_strings() {
        let fm = frontmatter_with("valid_from: 2024-01-01\nvalid_to: 2025-12-31\n");
        assert_eq!(
            (fm.valid_from.as_deref(), fm.valid_to.as_deref()),
            (Some("2024-01-01"), Some("2025-12-31"))
        );
    }

    #[test]
    fn the_json_view_of_the_frontmatter_keeps_the_summary() {
        let json = serde_json::to_value(frontmatter_with("summary: Short.\n")).unwrap();
        assert_eq!(json["summary"], serde_json::json!("Short."));
    }

    #[test]
    fn the_summary_is_read_into_its_own_field() {
        assert_eq!(
            frontmatter_with("summary: Kunde A buys GRASP.\n").summary.as_deref(),
            Some("Kunde A buys GRASP.")
        );
    }

    #[test]
    fn the_summary_is_not_kept_a_second_time_among_the_extra_keys() {
        assert!(!frontmatter_with("summary: Short.\n").extra.contains_key("summary"));
    }

    #[test]
    fn an_empty_summary_counts_as_absent() {
        assert_eq!(frontmatter_with("summary: \"  \"\n").summary, None);
    }

    #[test]
    fn the_json_view_of_the_frontmatter_keeps_unknown_keys_like_status() {
        let json = serde_json::to_value(frontmatter_with("status: draft\n")).unwrap();
        assert_eq!(json["status"], serde_json::json!("draft"));
    }

    #[test]
    fn a_numeric_frontmatter_key_does_not_fail_the_parse() {
        assert_eq!(frontmatter_with("2025: revenue\n").extra["2025"], serde_json::json!("revenue"));
    }

    #[test]
    fn a_nested_map_with_numeric_keys_serialises_to_json() {
        let fm = frontmatter_with("figures:\n  2024: 10\n  2025: 12\n");
        assert_eq!(
            serde_json::to_value(&fm).unwrap()["figures"],
            serde_json::json!({ "2024": 10, "2025": 12 })
        );
    }

    #[test]
    fn slug_key_turns_punctuation_into_hyphens() {
        assert_eq!(slug_key("Acme, Inc. & Co"), "acme-inc-co");
    }

    #[test]
    fn alias_key_drops_a_type_directory_prefix() {
        assert_eq!(alias_key("entities/Old_Name"), "old-name");
    }

    #[test]
    fn distinct_from_is_read_as_page_ids() {
        assert_eq!(
            frontmatter_with("distinct_from: [\"[[entities/y]]\"]\n").distinct_from,
            vec!["entities/y"]
        );
    }

    #[test]
    fn the_json_view_of_the_frontmatter_omits_unset_optional_fields() {
        let json = serde_json::to_value(frontmatter_with("")).unwrap();
        let mut keys: Vec<&String> = json.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["created", "id", "tags", "title", "type", "updated"]);
    }
}
