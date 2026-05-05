//! Tier-1 tree-listing and page-read.

use std::path::Path;

use serde::Serialize;

use crate::vault::layout::{wiki_dir, WIKI_SUBDIRS};
use crate::wiki::page::{id_from_path, parse};

use super::{ViewerError, ViewerResult};

#[derive(Debug, Clone, Serialize)]
pub struct WikiTree {
    pub entities: Vec<String>,
    pub concepts: Vec<String>,
    pub sources: Vec<String>,
    pub topics: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageView {
    pub id: String,
    pub title: String,
    pub frontmatter: String,
    pub body: String,
}

pub fn list_tree(vault: &Path) -> ViewerResult<WikiTree> {
    let mut tree = WikiTree {
        entities: Vec::new(),
        concepts: Vec::new(),
        sources: Vec::new(),
        topics: Vec::new(),
    };
    for sub in WIKI_SUBDIRS {
        let dir = wiki_dir(vault).join(sub);
        if !dir.exists() {
            continue;
        }
        let mut ids = Vec::new();
        collect_ids(&dir, sub, &mut ids)?;
        ids.sort();
        match *sub {
            "entities" => tree.entities = ids,
            "concepts" => tree.concepts = ids,
            "sources" => tree.sources = ids,
            "topics" => tree.topics = ids,
            _ => {}
        }
    }
    Ok(tree)
}

fn collect_ids(dir: &Path, sub: &str, out: &mut Vec<String>) -> ViewerResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            collect_ids(&p, sub, out)?;
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Some(rel) = p.file_name().and_then(|n| n.to_str()) {
            let id = format!("{sub}/{}", id_from_path(Path::new(rel)));
            out.push(id);
        }
    }
    Ok(())
}

pub fn read_page(vault: &Path, id: &str) -> ViewerResult<PageView> {
    let path = wiki_dir(vault).join(format!("{id}.md"));
    if !path.exists() {
        return Err(ViewerError::PageNotFound(id.to_string()));
    }
    let raw = std::fs::read_to_string(&path)?;
    let parsed = parse(&raw)?;
    let title = parsed
        .frontmatter
        .title
        .clone()
        .unwrap_or_else(|| id.to_string());
    let frontmatter_json = serde_json::to_string_pretty(&parsed.frontmatter)
        .unwrap_or_else(|_| String::new());
    Ok(PageView {
        id: id.to_string(),
        title,
        frontmatter: frontmatter_json,
        body: parsed.body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::ensure_skeleton;
    use tempfile::TempDir;

    fn write_page(vault: &Path, sub: &str, slug: &str) {
        let dir = wiki_dir(vault).join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{slug}.md")),
            format!(
                "---\nid: {sub}/{slug}\ntype: entity\ntitle: T\ncreated: 2026-04-29\nupdated: 2026-04-29\n---\n\nbody"
            ),
        )
        .unwrap();
    }

    #[test]
    fn list_tree_returns_ids_grouped_by_subdir_and_sorted() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "bob");
        write_page(tmp.path(), "entities", "alice");
        write_page(tmp.path(), "concepts", "nlspec");
        let tree = list_tree(tmp.path()).unwrap();
        assert_eq!(tree.entities, vec!["entities/alice", "entities/bob"]);
        assert_eq!(tree.concepts, vec!["concepts/nlspec"]);
        assert!(tree.sources.is_empty());
    }

    #[test]
    fn read_page_returns_title_and_body_when_page_exists() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_page(tmp.path(), "entities", "alice");
        let view = read_page(tmp.path(), "entities/alice").unwrap();
        assert_eq!(view.id, "entities/alice");
        assert_eq!(view.title, "T");
        assert!(view.body.contains("body"));
    }

    #[test]
    fn read_page_returns_not_found_for_missing_id() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let err = read_page(tmp.path(), "entities/missing").unwrap_err();
        assert!(matches!(err, ViewerError::PageNotFound(_)));
    }
}
