//! MCP tool catalogue offered by the Brain server.
//!
//! The actual JSON-RPC dispatcher will live in a dedicated server module
//! once the protocol stack lands. This file documents the tool contracts
//! and provides the dispatch logic against the underlying viewer/wiki
//! modules so the same behavior is reachable from CLI tests.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDescriptor {
    pub name: &'static str,
    pub description: &'static str,
}

pub fn catalogue() -> &'static [ToolDescriptor] {
    &[
        ToolDescriptor {
            name: "brain.search",
            description: "Hybrid lexical + semantic search across wiki pages.",
        },
        ToolDescriptor {
            name: "brain.get_page",
            description: "Read a single wiki page by id.",
        },
        ToolDescriptor {
            name: "brain.get_context",
            description: "Page plus its 1-hop wiki-link neighborhood.",
        },
        ToolDescriptor {
            name: "brain.list_pages",
            description: "List page metadata, optionally filtered by type.",
        },
        ToolDescriptor {
            name: "brain.write_page",
            description: "Create or overwrite a wiki page (triggers lint + commit).",
        },
        ToolDescriptor {
            name: "brain.upsert_chunks",
            description: "Persist embedding chunks for a given page.",
        },
        ToolDescriptor {
            name: "brain.write_raw_file",
            description: "Place a raw artifact under 01_raw/ for later ingest.",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_is_non_empty_and_contains_search_get_page_write_page() {
        let names: Vec<_> = catalogue().iter().map(|t| t.name).collect();
        assert!(names.contains(&"brain.search"));
        assert!(names.contains(&"brain.get_page"));
        assert!(names.contains(&"brain.write_page"));
    }

    #[test]
    fn all_tools_have_a_non_empty_description() {
        for t in catalogue() {
            assert!(!t.description.is_empty(), "tool {} missing description", t.name);
        }
    }
}
