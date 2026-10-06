//! Markdown body chunking for embedding.
//!
//! The body is first split into sections at ATX headings (`#`..`######`);
//! each section is then cut into overlapping word-sized windows. Every chunk
//! remembers the heading path of the section it came from (page root first,
//! most-specific heading last), so the indexer can prepend a short context
//! header before embedding ("contextual chunking"). Personal-scale wikis
//! rarely exceed a few thousand words per page, so a simple sliding window
//! per section is more than enough.

const MAX_WORDS: usize = 220;
const STRIDE: usize = 160;

/// Upper bound (in characters) for the context line produced by
/// [`contextual_text`]. ~160 characters stays well below ~40 bge-m3 tokens
/// for typical German/English titles and headings.
const MAX_HEADER_CHARS: usize = 160;

/// Separator between the page label and the heading path in the header.
const PATH_SEP: &str = " › ";

/// One embeddable window of a page body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Headings from the page root down to the chunk's section, most
    /// specific last. Empty for text before the first heading.
    pub heading_path: Vec<String>,
    /// The chunk's original text (whitespace-normalised words of the
    /// section, heading lines excluded). This is what gets stored.
    pub text: String,
}

/// Plain chunk texts — kept for callers that do not need heading context.
pub fn chunks(body: &str) -> Vec<String> {
    section_chunks(body).into_iter().map(|c| c.text).collect()
}

/// Heading-aware chunking: one or more chunks per non-empty section, each
/// tagged with the section's heading path.
///
/// Heading lines themselves are not part of a chunk's text — they travel
/// in `heading_path`. Lines inside fenced code blocks (```` ``` ```` / `~~~`)
/// are never treated as headings. If a non-empty body consists of headings
/// only, the whole body is chunked as one root section so the page still
/// gets at least one chunk (the indexer's skip-fast-path relies on that).
pub fn section_chunks(body: &str) -> Vec<Chunk> {
    let mut out = Vec::new();
    // (level, text) of the currently open headings, outermost first.
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut section_words: Vec<&str> = Vec::new();
    let mut fence: Option<(char, usize)> = None;

    for line in body.lines() {
        if let Some((ch, len)) = fence {
            if closes_fence(line, ch, len) {
                fence = None;
            }
            section_words.extend(line.split_whitespace());
            continue;
        }
        if let Some(open) = opens_fence(line) {
            fence = Some(open);
            section_words.extend(line.split_whitespace());
            continue;
        }
        if let Some((level, text)) = atx_heading(line) {
            flush(&mut out, &stack, &section_words);
            section_words.clear();
            while stack.last().is_some_and(|(l, _)| *l >= level) {
                stack.pop();
            }
            stack.push((level, text));
            continue;
        }
        section_words.extend(line.split_whitespace());
    }
    flush(&mut out, &stack, &section_words);

    if out.is_empty() {
        let words: Vec<&str> = body.split_whitespace().collect();
        out.extend(windows(&words).into_iter().map(|text| Chunk {
            heading_path: Vec::new(),
            text,
        }));
    }
    out
}

/// The text that is actually embedded for a chunk: one line of context
/// (`<title> (<type>) › <h1> › <h2>`), a blank line, then the chunk.
/// The header is capped at [`MAX_HEADER_CHARS`] characters. Empty parts
/// (no title, no type, empty heading path) are left out.
pub fn contextual_text(title: &str, page_type: &str, heading_path: &[String], chunk: &str) -> String {
    let title = collapse_ws(title);
    let page_type = collapse_ws(page_type);
    let mut header = match (title.is_empty(), page_type.is_empty()) {
        (false, false) => format!("{title} ({page_type})"),
        (false, true) => title,
        (true, false) => format!("({page_type})"),
        (true, true) => String::new(),
    };
    for h in heading_path {
        let h = collapse_ws(h);
        if h.is_empty() {
            continue;
        }
        if !header.is_empty() {
            header.push_str(PATH_SEP);
        }
        header.push_str(&h);
    }
    if header.chars().count() > MAX_HEADER_CHARS {
        header = header.chars().take(MAX_HEADER_CHARS - 1).collect();
        header.push('…');
    }
    if header.is_empty() {
        return chunk.to_string();
    }
    format!("{header}\n\n{chunk}")
}

fn flush(out: &mut Vec<Chunk>, stack: &[(usize, String)], words: &[&str]) {
    let path: Vec<String> = stack
        .iter()
        .filter(|(_, t)| !t.is_empty())
        .map(|(_, t)| t.clone())
        .collect();
    for text in windows(words) {
        out.push(Chunk {
            heading_path: path.clone(),
            text,
        });
    }
}

/// Sliding window of MAX_WORDS words, advancing by STRIDE.
fn windows(words: &[&str]) -> Vec<String> {
    if words.is_empty() {
        return Vec::new();
    }
    if words.len() <= MAX_WORDS {
        return vec![words.join(" ")];
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let end = (i + MAX_WORDS).min(words.len());
        out.push(words[i..end].join(" "));
        if end == words.len() {
            break;
        }
        i += STRIDE;
    }
    out
}

/// Strips up to three spaces of indentation (CommonMark); returns `None`
/// for deeper indentation (indented code block).
fn strip_indent(line: &str) -> Option<&str> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    Some(&line[indent..])
}

/// Parses an ATX heading line into (level, text).
fn atx_heading(line: &str) -> Option<(usize, String)> {
    let rest = strip_indent(line)?;
    let level = rest.len() - rest.trim_start_matches('#').len();
    if !(1..=6).contains(&level) {
        return None;
    }
    let after = &rest[level..];
    if !after.is_empty() && !after.starts_with([' ', '\t']) {
        return None; // "#hashtag", not a heading
    }
    let mut text = after.trim();
    // Optional closing sequence: " ##" at the end.
    let without_closing = text.trim_end_matches('#');
    if without_closing.is_empty() {
        text = "";
    } else if without_closing.len() != text.len() && without_closing.ends_with([' ', '\t']) {
        text = without_closing.trim_end();
    }
    Some((level, collapse_ws(text)))
}

fn opens_fence(line: &str) -> Option<(char, usize)> {
    let rest = strip_indent(line)?;
    let ch = rest.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = rest.len() - rest.trim_start_matches(ch).len();
    (len >= 3).then_some((ch, len))
}

fn closes_fence(line: &str, ch: char, open_len: usize) -> bool {
    let Some(rest) = strip_indent(line) else {
        return false;
    };
    let len = rest.len() - rest.trim_start_matches(ch).len();
    len >= open_len && rest[len..].trim().is_empty()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(n: usize) -> String {
        (0..n).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn chunks_returns_a_single_window_for_a_short_body() {
        let body = "alice meets bob in berlin";
        let c = chunks(body);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn chunks_returns_empty_for_an_empty_body() {
        assert!(chunks("").is_empty());
        assert!(chunks("   ").is_empty());
    }

    #[test]
    fn chunks_yields_overlapping_windows_for_a_long_body() {
        let body = words(500);
        let c = chunks(&body);
        assert!(c.len() >= 2);
        // First and second window overlap (stride < window).
        let w0_last = c[0].split_whitespace().last().unwrap();
        let w1_first = c[1].split_whitespace().next().unwrap();
        assert_ne!(w0_last, w1_first); // distinct positions
    }

    #[test]
    fn heading_aware_chunking_yields_one_chunk_per_short_section_with_its_heading_path() {
        let body = "# Vertrag\nKunde A.\n## Laufzeit\n12 Monate.\n## Kündigung\n3 Monate.\n";
        let paths: Vec<Vec<String>> = section_chunks(body)
            .into_iter()
            .map(|c| c.heading_path)
            .collect();
        assert_eq!(
            paths,
            vec![
                vec!["Vertrag".to_string()],
                vec!["Vertrag".to_string(), "Laufzeit".to_string()],
                vec!["Vertrag".to_string(), "Kündigung".to_string()],
            ]
        );
    }

    #[test]
    fn heading_aware_chunking_keeps_heading_lines_out_of_the_chunk_text() {
        let body = "# Vertrag\n## Laufzeit\nthe contract renews for 12 months\n";
        let texts: Vec<String> = section_chunks(body).into_iter().map(|c| c.text).collect();
        assert_eq!(texts, vec!["the contract renews for 12 months".to_string()]);
    }

    #[test]
    fn a_long_section_is_split_into_overlapping_windows_that_share_the_section_heading_path() {
        let body = format!("# Vertrag\n## Laufzeit\n{}\n", words(500));
        let c = section_chunks(&body);
        let expected = vec!["Vertrag".to_string(), "Laufzeit".to_string()];
        assert!(c.len() >= 2 && c.iter().all(|ch| ch.heading_path == expected));
    }

    #[test]
    fn windows_of_a_long_section_overlap() {
        let body = format!("## Laufzeit\n{}\n", words(500));
        let c = section_chunks(&body);
        let second_starts_inside_first = c[0]
            .text
            .split_whitespace()
            .any(|w| Some(w) == c[1].text.split_whitespace().next());
        assert!(second_starts_inside_first);
    }

    #[test]
    fn text_before_the_first_heading_gets_an_empty_heading_path() {
        let body = "Intro text.\n# Vertrag\nKunde A.\n";
        let first = section_chunks(body).into_iter().next().unwrap();
        assert!(first.heading_path.is_empty());
    }

    #[test]
    fn a_deeper_heading_after_a_shallower_one_replaces_the_sibling_branch() {
        let body = "# A\n## B\n### C\nx\n## D\ny\n";
        let last = section_chunks(body).into_iter().last().unwrap();
        assert_eq!(last.heading_path, vec!["A".to_string(), "D".to_string()]);
    }

    #[test]
    fn hash_lines_inside_a_fenced_code_block_are_not_headings() {
        let body = "# Setup\n```bash\n# install deps\npnpm i\n```\n";
        let c = section_chunks(body);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn a_hashtag_without_a_space_is_not_a_heading() {
        let body = "#nis2 is relevant\n";
        let first = section_chunks(body).into_iter().next().unwrap();
        assert!(first.heading_path.is_empty());
    }

    #[test]
    fn closing_hashes_are_stripped_from_the_heading_text() {
        let body = "## Laufzeit ##\nx\n";
        let first = section_chunks(body).into_iter().next().unwrap();
        assert_eq!(first.heading_path, vec!["Laufzeit".to_string()]);
    }

    #[test]
    fn a_body_of_headings_only_still_yields_one_chunk() {
        let c = section_chunks("# Vertrag\n## Laufzeit\n");
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn contextual_text_renders_title_type_and_heading_path_on_one_line_then_a_blank_line_then_the_chunk() {
        let path = vec!["Vertrag".to_string(), "Laufzeit".to_string()];
        let t = contextual_text("Kunde A", "entity", &path, "renews for 12 months");
        assert_eq!(t, "Kunde A (entity) › Vertrag › Laufzeit\n\nrenews for 12 months");
    }

    #[test]
    fn contextual_text_with_an_empty_heading_path_renders_only_title_and_type() {
        let t = contextual_text("Kunde A", "entity", &[], "body");
        assert_eq!(t, "Kunde A (entity)\n\nbody");
    }

    #[test]
    fn contextual_text_caps_an_overlong_header_line() {
        let long = "x".repeat(500);
        let t = contextual_text(&long, "entity", &[], "body");
        let header = t.split("\n\n").next().unwrap();
        assert_eq!(header.chars().count(), MAX_HEADER_CHARS);
    }
}
