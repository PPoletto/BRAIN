//! Tier-3 graph data with type/tag/date filters.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::vault::layout::{wiki_dir, WIKI_SUBDIRS};
use crate::wiki::page::parse;

use super::ViewerResult;

#[derive(Debug, Clone, Serialize)]
pub struct GraphNode {
    pub id: String,
    pub r#type: String,
    pub title: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GraphFilters {
    pub types: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub updated_after: Option<String>,
}

pub fn build_graph(vault: &Path, filters: &GraphFilters) -> ViewerResult<GraphData> {
    let mut nodes: Vec<GraphNode> = Vec::new();
    // Dedup (src, dst) pairs in-memory: a single source page can reference
    // the same target multiple times in its body (e.g. once in the intro
    // and again in a "Verwandte Seiten" section). The DB layer dedups via
    // `INSERT OR IGNORE`, but the graph builder reads from the parser, so
    // we do the same thing with a HashSet here.
    let mut edge_set: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    // Walk recursively so pages nested under e.g. `entities/customers/` or
    // `concepts/regulations/` show up in the graph. The earlier flat
    // `read_dir` ignored anything below the top type-folder, which made
    // the graph look empty for users who organised pages in sub-folders.
    for sub in WIKI_SUBDIRS {
        let dir = wiki_dir(vault).join(sub);
        if !dir.exists() {
            continue;
        }
        visit_graph_dir(&dir, filters, &mut nodes, &mut edge_set)?;
    }

    let mut edges: Vec<GraphEdge> = edge_set
        .into_iter()
        .map(|(source, target)| GraphEdge { source, target })
        .collect();
    // Stable order so the output is deterministic across runs.
    edges.sort_by(|a, b| (a.source.as_str(), a.target.as_str())
        .cmp(&(b.source.as_str(), b.target.as_str())));

    let id_set: std::collections::HashSet<&String> = nodes.iter().map(|n| &n.id).collect();
    edges.retain(|e| id_set.contains(&e.target) && id_set.contains(&e.source));

    Ok(GraphData { nodes, edges })
}

fn visit_graph_dir(
    dir: &Path,
    filters: &GraphFilters,
    nodes: &mut Vec<GraphNode>,
    edge_set: &mut std::collections::HashSet<(String, String)>,
) -> ViewerResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            visit_graph_dir(&p, filters, nodes, edge_set)?;
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let raw = std::fs::read_to_string(&p)?;
        let parsed = match parse(&raw) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let fm = &parsed.frontmatter;
        if let Some(types) = &filters.types {
            if !types.is_empty() && !types.iter().any(|t| t == &fm.page_type) {
                continue;
            }
        }
        if let Some(tags) = &filters.tags {
            if !tags.is_empty() && !tags.iter().any(|t| fm.tags.contains(t)) {
                continue;
            }
        }
        if let (Some(after), Some(updated)) = (&filters.updated_after, &fm.updated) {
            if updated.as_str() < after.as_str() {
                continue;
            }
        }
        nodes.push(GraphNode {
            id: fm.id.clone(),
            r#type: fm.page_type.clone(),
            title: fm.title.clone().unwrap_or_else(|| fm.id.clone()),
            tags: fm.tags.clone(),
        });
        for link in &parsed.wiki_links {
            edge_set.insert((fm.id.clone(), link.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::ensure_skeleton;
    use tempfile::TempDir;

    fn write_page(vault: &Path, sub: &str, slug: &str, page_type: &str, body: &str, tags: &[&str], updated: &str) {
        let dir = wiki_dir(vault).join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        let tags_yaml = format!("[{}]", tags.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(","));
        std::fs::write(
            dir.join(format!("{slug}.md")),
            format!(
                "---\nid: {sub}/{slug}\ntype: {page_type}\ntitle: T\ntags: {tags_yaml}\ncreated: 2026-04-01\nupdated: {updated}\n---\n\n{body}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn build_graph_returns_all_nodes_and_resolved_edges_when_no_filter() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "entity", "see [[entities/bob]]", &[], "2026-04-29");
        write_page(tmp.path(), "entities", "bob", "entity", "hi", &[], "2026-04-29");
        let g = build_graph(tmp.path(), &GraphFilters::default()).unwrap();
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].source, "entities/alice");
        assert_eq!(g.edges[0].target, "entities/bob");
    }

    #[test]
    fn build_graph_dedups_edges_when_same_target_is_referenced_multiple_times() {
        // Regression for the connectivity test: pages that mention the same
        // wiki link in multiple sections (e.g. intro + "Verwandte Seiten")
        // were yielding duplicate edges in the graph output.
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(
            tmp.path(),
            "entities",
            "alice",
            "entity",
            "Alice met [[entities/bob]] yesterday. They worked on [[concepts/x]]. \
             See also [[entities/bob]] for context.",
            &[],
            "2026-04-29",
        );
        write_page(tmp.path(), "entities", "bob", "entity", "hi", &[], "2026-04-29");
        write_page(tmp.path(), "concepts", "x", "concept", "hi", &[], "2026-04-29");
        let g = build_graph(tmp.path(), &GraphFilters::default()).unwrap();
        let alice_to_bob: usize = g
            .edges
            .iter()
            .filter(|e| e.source == "entities/alice" && e.target == "entities/bob")
            .count();
        assert_eq!(
            alice_to_bob, 1,
            "duplicate alice→bob edges leaked through: {:#?}",
            g.edges
        );
    }

    #[test]
    fn build_graph_drops_edges_pointing_to_unknown_targets() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "entity", "see [[entities/missing]]", &[], "2026-04-29");
        let g = build_graph(tmp.path(), &GraphFilters::default()).unwrap();
        assert_eq!(g.nodes.len(), 1);
        assert!(g.edges.is_empty());
    }

    #[test]
    fn build_graph_filters_by_type() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "entity", "x", &[], "2026-04-29");
        write_page(tmp.path(), "concepts", "nlspec", "concept", "x", &[], "2026-04-29");
        let g = build_graph(
            tmp.path(),
            &GraphFilters {
                types: Some(vec!["entity".into()]),
                ..GraphFilters::default()
            },
        )
        .unwrap();
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].r#type, "entity");
    }

    #[test]
    fn build_graph_filters_by_tag_intersection() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice", "entity", "x", &["nis2"], "2026-04-29");
        write_page(tmp.path(), "entities", "bob", "entity", "x", &["other"], "2026-04-29");
        let g = build_graph(
            tmp.path(),
            &GraphFilters {
                tags: Some(vec!["nis2".into()]),
                ..GraphFilters::default()
            },
        )
        .unwrap();
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, "entities/alice");
    }

    #[test]
    fn build_graph_recurses_into_nested_subfolders() {
        // Regression: an earlier non-recursive walk silently dropped
        // pages stored under e.g. `entities/customers/dan-shapiro.md`,
        // leaving the graph view a "collection of disconnected dots"
        // because the only nodes that *did* get picked up had no
        // surviving link targets in the visible set.
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let nested = wiki_dir(tmp.path()).join("entities").join("customers");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("dan-shapiro.md"),
            "---\nid: entities/customers/dan-shapiro\ntype: entity\ntitle: Dan\n\
             tags: []\ncreated: 2026-04-01\nupdated: 2026-04-29\n---\n\n\
             see [[concepts/glowforge]]\n",
        )
        .unwrap();
        write_page(tmp.path(), "concepts", "glowforge", "concept", "x", &[], "2026-04-29");
        let g = build_graph(tmp.path(), &GraphFilters::default()).unwrap();
        assert_eq!(g.nodes.len(), 2, "nested page must be picked up: {:#?}", g.nodes);
        assert_eq!(g.edges.len(), 1, "edge must resolve to nested source");
    }

    #[test]
    fn build_graph_filters_by_updated_after_date() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "old", "entity", "x", &[], "2025-01-01");
        write_page(tmp.path(), "entities", "new", "entity", "x", &[], "2026-04-29");
        let g = build_graph(
            tmp.path(),
            &GraphFilters {
                updated_after: Some("2026-01-01".into()),
                ..GraphFilters::default()
            },
        )
        .unwrap();
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, "entities/new");
    }
}
