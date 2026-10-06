//! A2: "does a page for this probably exist already?"
//!
//! A requested page id is compared with the ids and `aliases` of the
//! pages in the SAME type directory (`entities/…` only against
//! `entities/…`) on their [`slug_key`] (aliases via [`alias_key`]):
//!
//!  - `alias` — the requested slug names one of a page's aliases;
//!  - `normalised` — the requested slug names the page's slug;
//!  - `similar` — the slugs are close but not the same name: one or two
//!    edits apart (see [`similar`]), or equal only once umlaut
//!    transliterations are folded on a short single-word key
//!    (`michael` / `michal`).
//!
//! "Names" ([`names_match`]) means equal keys (case, `ü`/`ue`,
//! separators and punctuation already normalised by `slug_key`), or keys
//! that are equal once `ae/oe/ue` are folded to `a/o/u` — but the folded
//! key must be at least [`FOLD_MIN_CHARS`] long or contain a `-`, so
//! `muller-gmbh` / `mueller-gmbh` are the same name while `joel` / `jol`
//! or `kohler` / `koehler` are not.
//!
//! `brain_page_exists` reports all three; `brain_write_page` /
//! `brain_write_batch` refuse to CREATE a page with an `alias` or
//! `normalised` match ([`MatchReason::blocks_create`]) unless the caller
//! passes `allow_duplicate: true`. `similar` is too fuzzy to block.
//!
//! The candidates come from the SQLite index ([`load_entries`]); the MCP
//! server drops matches whose page file no longer exists before it
//! refuses, so a stale index row never blocks. A page written seconds ago
//! — before the GUI watcher re-indexed — is not yet seen;
//! `brain_write_batch` also checks its entries against each other.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::db::DbResult;

use super::page::{alias_key, levenshtein, slug_key, umlaut_folded_key};

/// A folded key (see [`umlaut_folded_key`]) only makes two different
/// keys the same name when it has at least this many characters or
/// contains a `-`.
pub const FOLD_MIN_CHARS: usize = 8;

/// Keys shorter than this may differ by one edit to be `similar`; longer
/// ones by two.
pub const SIMILAR_LONG_KEY_CHARS: usize = 8;

/// Keys shorter than this are never `similar` (`ab` / `ac`).
pub const SIMILAR_MIN_KEY_CHARS: usize = 3;

/// The names of one page: its id, title and aliases, plus the ids it
/// declares itself distinct from (frontmatter `distinct_from`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NameEntry {
    pub id: String,
    pub title: Option<String>,
    pub aliases: Vec<String>,
    pub distinct_from: Vec<String>,
}

/// Why a page matched a requested id. Ordered strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchReason {
    Alias,
    Normalised,
    Similar,
}

impl MatchReason {
    /// `alias` and `normalised` matches block creating a new page;
    /// `similar` ones are only reported.
    pub fn blocks_create(self) -> bool {
        !matches!(self, MatchReason::Similar)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MatchReason::Alias => "alias",
            MatchReason::Normalised => "normalised",
            MatchReason::Similar => "similar",
        }
    }
}

/// One page that probably is the requested one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DuplicateMatch {
    pub id: String,
    pub title: Option<String>,
    pub reason: MatchReason,
}

/// `entities/foo/bar` → (`entities`, `foo/bar`); an id without `/` has an
/// empty type directory.
fn split_id(id: &str) -> (&str, &str) {
    id.split_once('/').unwrap_or(("", id))
}

/// The slug key of a page id (the part after the type directory).
pub fn id_key(id: &str) -> String {
    slug_key(split_id(id).1)
}

fn fold_qualifies(folded: &str) -> bool {
    folded.chars().count() >= FOLD_MIN_CHARS || folded.contains('-')
}

/// The key two names are compared under: the folded key when it
/// qualifies (see [`FOLD_MIN_CHARS`]), the key itself otherwise. Two keys
/// name the same thing exactly when their match keys are equal.
pub fn match_key(key: &str) -> String {
    let folded = umlaut_folded_key(key);
    if fold_qualifies(&folded) {
        folded
    } else {
        key.to_string()
    }
}

/// True when two slug keys name the same thing (module docs).
pub fn names_match(a: &str, b: &str) -> bool {
    !a.is_empty() && (a == b || match_key(a) == match_key(b))
}

/// True when two different keys are close: equal after folding on a
/// short key, or at most 1 edit apart (keys under
/// [`SIMILAR_LONG_KEY_CHARS`]) / 2 edits apart (longer keys).
pub fn similar(a: &str, b: &str) -> bool {
    let (la, lb) = (a.chars().count(), b.chars().count());
    let shorter = la.min(lb);
    if shorter < SIMILAR_MIN_KEY_CHARS {
        return false;
    }
    if umlaut_folded_key(a) == umlaut_folded_key(b) {
        return true;
    }
    let max = if shorter < SIMILAR_LONG_KEY_CHARS { 1 } else { 2 };
    la.abs_diff(lb) <= max && levenshtein(a, b) <= max
}

/// Every page in `entries` (other than `requested_id` itself) of the same
/// type directory that probably is the requested page, strongest reason
/// first, then by id.
pub fn find_matches(requested_id: &str, entries: &[NameEntry]) -> Vec<DuplicateMatch> {
    let (dir, slug) = split_id(requested_id);
    let key = slug_key(slug);
    if key.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<DuplicateMatch> = entries
        .iter()
        .filter(|e| e.id != requested_id && split_id(&e.id).0 == dir)
        .filter_map(|e| {
            let page_key = id_key(&e.id);
            let alias_keys: Vec<String> = e.aliases.iter().map(|a| alias_key(a)).collect();
            let reason = if alias_keys.iter().any(|k| names_match(k, &key)) {
                MatchReason::Alias
            } else if names_match(&page_key, &key) {
                MatchReason::Normalised
            } else if similar(&page_key, &key) || alias_keys.iter().any(|k| similar(k, &key)) {
                MatchReason::Similar
            } else {
                return None;
            };
            Some(DuplicateMatch {
                id: e.id.clone(),
                title: e.title.clone(),
                reason,
            })
        })
        .collect();
    out.sort_by(|a, b| a.reason.cmp(&b.reason).then_with(|| a.id.cmp(&b.id)));
    out
}

/// The error text of a refused create; names the existing page's id and
/// title.
pub fn refusal_message(m: &DuplicateMatch) -> String {
    let title = m
        .title
        .as_deref()
        .map(|t| format!(" \"{t}\""))
        .unwrap_or_default();
    format!(
        "a page for this probably exists already: {}{title} ({}) — use that id, or pass allow_duplicate:true",
        m.id,
        m.reason.as_str()
    )
}

/// Ids, titles and aliases of every indexed page.
pub fn load_entries(conn: &rusqlite::Connection) -> DbResult<Vec<NameEntry>> {
    let mut entries: BTreeMap<String, NameEntry> = BTreeMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, title FROM pages")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (id, title) = row?;
            entries.insert(
                id.clone(),
                NameEntry {
                    id,
                    title,
                    ..NameEntry::default()
                },
            );
        }
    }
    let mut stmt = conn.prepare("SELECT page_id, alias FROM page_aliases ORDER BY page_id, alias")?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    for row in rows {
        let (id, alias) = row?;
        if let Some(entry) = entries.get_mut(&id) {
            entry.aliases.push(alias);
        }
    }
    Ok(entries.into_values().collect())
}

/// One name of a page under a match key: (page index, via an alias?, the
/// name's own slug key).
type NameUse = (usize, bool, String);

/// Pairs `(a, b)` (a < b, same type directory) of pages that share a name
/// — the `alias-collision` lint. A pair counts when an alias of one names
/// the other's id or one of its aliases ([`names_match`]), or when the two
/// id keys are exactly equal. Two ids that only match after umlaut
/// folding are left to `similar` (`kohler` / `koehler`). A pair is
/// skipped when either page lists the other in `distinct_from`.
pub fn name_collisions(pages: &[NameEntry]) -> Vec<(String, String)> {
    let mut by_name: BTreeMap<(String, String), Vec<NameUse>> = BTreeMap::new();
    for (i, page) in pages.iter().enumerate() {
        let dir = split_id(&page.id).0.to_string();
        let mut names: Vec<(String, bool)> = vec![(id_key(&page.id), false)];
        names.extend(page.aliases.iter().map(|a| (alias_key(a), true)));
        for (key, via_alias) in names.into_iter().filter(|(k, _)| !k.is_empty()) {
            by_name
                .entry((dir.clone(), match_key(&key)))
                .or_default()
                .push((i, via_alias, key));
        }
    }
    let distinct = |a: &NameEntry, b: &NameEntry| {
        a.distinct_from.contains(&b.id) || b.distinct_from.contains(&a.id)
    };
    let mut pairs: BTreeSet<(String, String)> = BTreeSet::new();
    for members in by_name.values() {
        for (x, (i, alias_i, key_i)) in members.iter().enumerate() {
            for (j, alias_j, key_j) in &members[x + 1..] {
                let (a, b) = (&pages[*i], &pages[*j]);
                if a.id == b.id || distinct(a, b) {
                    continue;
                }
                if *alias_i || *alias_j || key_i == key_j {
                    let pair = if a.id < b.id {
                        (a.id.clone(), b.id.clone())
                    } else {
                        (b.id.clone(), a.id.clone())
                    };
                    pairs.insert(pair);
                }
            }
        }
    }
    pairs.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, aliases: &[&str]) -> NameEntry {
        NameEntry {
            id: id.into(),
            title: Some(format!("Title of {id}")),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            distinct_from: Vec::new(),
        }
    }

    fn reasons(requested: &str, entries: &[NameEntry]) -> Vec<(String, MatchReason)> {
        find_matches(requested, entries)
            .into_iter()
            .map(|m| (m.id, m.reason))
            .collect()
    }

    fn blocks(requested: &str, entries: &[NameEntry]) -> bool {
        find_matches(requested, entries)
            .iter()
            .any(|m| m.reason.blocks_create())
    }

    #[test]
    fn a_slug_that_equals_a_page_after_normalisation_is_a_normalised_match() {
        let entries = [entry("entities/mueller-gmbh", &[])];
        assert_eq!(
            reasons("entities/Müller_GmbH", &entries),
            vec![("entities/mueller-gmbh".to_string(), MatchReason::Normalised)]
        );
    }

    #[test]
    fn an_umlaut_spelled_without_e_in_a_hyphenated_slug_blocks_creation() {
        let entries = [entry("entities/mueller-gmbh", &[])];
        assert!(blocks("entities/muller-gmbh", &entries));
    }

    #[test]
    fn michael_and_michal_do_not_block_creation() {
        let entries = [entry("entities/michael", &[])];
        assert!(!blocks("entities/michal", &entries));
    }

    #[test]
    fn michael_and_michal_are_reported_as_similar() {
        let entries = [entry("entities/michael", &[])];
        assert_eq!(
            reasons("entities/michal", &entries),
            vec![("entities/michael".to_string(), MatchReason::Similar)]
        );
    }

    #[test]
    fn kohler_and_koehler_do_not_block_creation() {
        let entries = [entry("entities/koehler", &[])];
        assert!(!blocks("entities/kohler", &entries));
    }

    #[test]
    fn a_slug_that_equals_an_alias_is_an_alias_match() {
        let entries = [entry("entities/mueller-gmbh", &["Müller Holding"])];
        assert_eq!(
            reasons("entities/mueller-holding", &entries),
            vec![("entities/mueller-gmbh".to_string(), MatchReason::Alias)]
        );
    }

    #[test]
    fn an_alias_written_as_a_page_id_matches_its_slug() {
        let entries = [entry("entities/acme", &["entities/acme-corp"])];
        assert_eq!(
            reasons("entities/acme-corp", &entries),
            vec![("entities/acme".to_string(), MatchReason::Alias)]
        );
    }

    #[test]
    fn a_long_slug_two_edits_away_is_a_similar_match() {
        let entries = [entry("entities/dan-shapiro", &[])];
        assert_eq!(
            reasons("entities/dan-shapo", &entries),
            vec![("entities/dan-shapiro".to_string(), MatchReason::Similar)]
        );
    }

    #[test]
    fn a_short_slug_two_edits_away_is_not_similar() {
        let entries = [entry("entities/bobby", &[])];
        assert!(find_matches("entities/bob", &entries).is_empty());
    }

    #[test]
    fn pages_of_another_type_directory_never_match() {
        let entries = [entry("concepts/mueller-gmbh", &[])];
        assert!(find_matches("entities/mueller-gmbh", &entries).is_empty());
    }

    #[test]
    fn the_requested_page_itself_is_not_a_match() {
        let entries = [entry("entities/alice", &[])];
        assert!(find_matches("entities/alice", &entries).is_empty());
    }

    #[test]
    fn a_one_letter_typo_does_not_block_creation() {
        let entries = [entry("entities/dan-shapiro", &[])];
        assert!(!blocks("entities/dan-shapio", &entries));
    }

    #[test]
    fn the_refusal_names_the_existing_id_its_title_and_the_override() {
        let m = DuplicateMatch {
            id: "entities/mueller-gmbh".into(),
            title: Some("Müller GmbH".into()),
            reason: MatchReason::Normalised,
        };
        assert_eq!(
            refusal_message(&m),
            "a page for this probably exists already: entities/mueller-gmbh \"Müller GmbH\" (normalised) — use that id, or pass allow_duplicate:true"
        );
    }

    #[test]
    fn two_pages_sharing_a_name_through_an_alias_collide() {
        let pages = [
            entry("entities/acme", &["Acme Corp"]),
            entry("entities/acme-corp", &[]),
            entry("entities/other", &[]),
        ];
        assert_eq!(
            name_collisions(&pages),
            vec![("entities/acme".to_string(), "entities/acme-corp".to_string())]
        );
    }

    #[test]
    fn two_ids_equal_only_after_umlaut_folding_do_not_collide() {
        let pages = [entry("entities/mueller-gmbh", &[]), entry("entities/muller-gmbh", &[])];
        assert!(name_collisions(&pages).is_empty());
    }

    #[test]
    fn distinct_from_silences_a_collision() {
        let mut acme = entry("entities/acme", &["Acme Corp"]);
        acme.distinct_from = vec!["entities/acme-corp".into()];
        let pages = [acme, entry("entities/acme-corp", &[])];
        assert!(name_collisions(&pages).is_empty());
    }
}
