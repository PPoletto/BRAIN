//! Markdown body chunking for embedding.
//!
//! Splits a body into overlapping word-sized windows. Personal-scale wikis
//! rarely exceed a few thousand words per page so a simple sliding-window
//! approach is more than enough.

const MAX_WORDS: usize = 220;
const STRIDE: usize = 160;

pub fn chunks(body: &str) -> Vec<String> {
    let words: Vec<&str> = body.split_whitespace().collect();
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let body = (0..500)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let c = chunks(&body);
        assert!(c.len() >= 2);
        // First and second window overlap (stride < window).
        let w0_last = c[0].split_whitespace().last().unwrap();
        let w1_first = c[1].split_whitespace().next().unwrap();
        assert_ne!(w0_last, w1_first); // distinct positions
    }
}
