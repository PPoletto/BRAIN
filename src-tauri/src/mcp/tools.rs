//! MCP tool catalogue of the `brain mcp` server: the names it serves, the
//! action discriminators of the multi-action tools, and the names it no
//! longer serves.
//!
//! The descriptors (description, schemas, annotations) live next to the
//! dispatcher in `server.rs`; this module is the single list both sides
//! are checked against, plus the mapping that turns a call with a removed
//! tool name into an actionable error (Slice D: the tool surface was
//! consolidated without transition aliases — an old name is never
//! served, it only explains what replaced it).

/// Every tool `tools/list` advertises, in advertised order.
pub const TOOL_NAMES: &[&str] = &[
    "brain_ping",
    "brain_search",
    "brain_lookup",
    "brain_get_pages",
    "brain_query",
    "brain_graph",
    "brain_write_page",
    "brain_write_batch",
    "brain_patch_page",
    "brain_refactor",
    "brain_lint_report",
    "brain_history",
    "brain_write_raw_file",
    "brain_eval",
    "brain_dream",
];

/// A tool with an `action` discriminator: its actions and the action used
/// when the call names none (`None` = `action` is required).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionTool {
    pub tool: &'static str,
    pub actions: &'static [&'static str],
    pub default: Option<&'static str>,
}

/// The multi-action tools.
pub const ACTION_TOOLS: &[ActionTool] = &[
    ActionTool {
        tool: "brain_refactor",
        actions: &["rename", "merge", "delete"],
        default: None,
    },
    ActionTool {
        tool: "brain_history",
        actions: &["list", "restore"],
        default: Some("list"),
    },
    ActionTool {
        tool: "brain_eval",
        actions: &["run", "add"],
        default: Some("run"),
    },
    ActionTool {
        tool: "brain_dream",
        actions: &["queue", "log"],
        default: None,
    },
];

/// The action a call of `tool` with `action` (the raw `arguments.action`)
/// selects. `Ok(None)` for a tool without actions; `Err` names the valid
/// actions when the action is missing (and the tool has no default),
/// unknown or not a string.
pub fn resolve_action(
    tool: &str,
    action: Option<&serde_json::Value>,
) -> Result<Option<&'static str>, String> {
    let Some(spec) = ACTION_TOOLS.iter().find(|a| a.tool == tool) else {
        return Ok(None);
    };
    let valid = spec.actions.join(", ");
    match action {
        None | Some(serde_json::Value::Null) => spec
            .default
            .map(Some)
            .ok_or_else(|| format!("{tool}: missing 'action' (one of: {valid})")),
        Some(serde_json::Value::String(s)) => spec
            .actions
            .iter()
            .find(|a| **a == s.as_str())
            .map(|a| Some(*a))
            .ok_or_else(|| format!("{tool}: unknown action '{s}' (one of: {valid})")),
        Some(_) => Err(format!(
            "{tool}: 'action' must be a string (one of: {valid})"
        )),
    }
}

/// A tool name that was removed, what replaced it and how to call the
/// replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemovedTool {
    pub old: &'static str,
    pub new: &'static str,
    /// The replacement's arguments, as an example call.
    pub args: &'static str,
}

/// Tool names removed in the Slice D consolidation (no aliases).
pub const REMOVED_TOOLS: &[RemovedTool] = &[
    RemovedTool {
        old: "brain_get_page",
        new: "brain_get_pages",
        args: r#"{"ids": ["<id>"]}"#,
    },
    RemovedTool {
        old: "brain_get_context",
        new: "brain_get_pages",
        args: r#"{"ids": ["<id>"], "include_context": true}"#,
    },
    RemovedTool {
        old: "brain_page_exists",
        new: "brain_lookup",
        args: r#"{"query_or_id": "<id>"}"#,
    },
    RemovedTool {
        old: "brain_list_pages",
        new: "brain_query",
        args: r#"{"query": "*"} or {"query": "type:entity", "prefix": "entities/acme", "limit": 50, "offset": 0}"#,
    },
    RemovedTool {
        old: "brain_list_tags",
        new: "brain_query",
        args: r#"{"facet": "tags"}"#,
    },
    RemovedTool {
        old: "brain_embedding_status",
        new: "brain_ping",
        args: r#"{"detail": true}"#,
    },
    RemovedTool {
        old: "brain_rename_page",
        new: "brain_refactor",
        args: r#"{"action": "rename", "id": "<id>", "new_id": "<new id>"}"#,
    },
    RemovedTool {
        old: "brain_merge_pages",
        new: "brain_refactor",
        args: r#"{"action": "merge", "from_id": "<duplicate>", "into_id": "<survivor>"}"#,
    },
    RemovedTool {
        old: "brain_delete_page",
        new: "brain_refactor",
        args: r#"{"action": "delete", "id": "<id>"}"#,
    },
    RemovedTool {
        old: "brain_get_page_history",
        new: "brain_history",
        args: r#"{"action": "list", "id": "<id>"}"#,
    },
    RemovedTool {
        old: "brain_restore_page",
        new: "brain_history",
        args: r#"{"action": "restore", "id": "<id>", "sha": "<sha>"}"#,
    },
    RemovedTool {
        old: "brain_eval_add",
        new: "brain_eval",
        args: r#"{"action": "add", "query": "...", "expected": ["<id>"]}"#,
    },
    RemovedTool {
        old: "brain_dream_queue",
        new: "brain_dream",
        args: r#"{"action": "queue"}"#,
    },
    RemovedTool {
        old: "brain_dream_log",
        new: "brain_dream",
        args: r#"{"action": "log", "entry": "..."}"#,
    },
];

/// Whether `name` is a tool the server serves.
pub fn is_known(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

/// The mapping entry for a removed tool name, if `name` is one.
pub fn removed(name: &str) -> Option<&'static RemovedTool> {
    REMOVED_TOOLS.iter().find(|r| r.old == name)
}

/// The error message a call with a removed tool name gets.
pub fn replaced_message(r: &RemovedTool) -> String {
    format!(
        "tool '{}' was replaced by '{}' (args: {})",
        r.old, r.new, r.args
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_catalogue_is_exactly_fifteen_tools() {
        assert_eq!(TOOL_NAMES.len(), 15);
    }

    #[test]
    fn the_catalogue_names_each_tool_once() {
        let unique: std::collections::BTreeSet<&str> = TOOL_NAMES.iter().copied().collect();
        assert_eq!(unique.len(), TOOL_NAMES.len());
    }

    #[test]
    fn every_replacement_names_a_tool_that_is_served() {
        let dangling: Vec<&str> = REMOVED_TOOLS
            .iter()
            .filter(|r| !is_known(r.new))
            .map(|r| r.new)
            .collect();
        assert!(dangling.is_empty(), "{dangling:?}");
    }

    #[test]
    fn no_removed_name_is_still_served() {
        let served: Vec<&str> = REMOVED_TOOLS
            .iter()
            .filter(|r| is_known(r.old))
            .map(|r| r.old)
            .collect();
        assert!(served.is_empty(), "{served:?}");
    }

    #[test]
    fn every_action_tool_is_served() {
        let unknown: Vec<&str> = ACTION_TOOLS
            .iter()
            .filter(|a| !is_known(a.tool))
            .map(|a| a.tool)
            .collect();
        assert!(unknown.is_empty(), "{unknown:?}");
    }

    #[test]
    fn the_replaced_message_names_the_old_and_the_new_tool() {
        let msg = replaced_message(removed("brain_get_page").unwrap());
        assert_eq!(
            msg,
            r#"tool 'brain_get_page' was replaced by 'brain_get_pages' (args: {"ids": ["<id>"]})"#
        );
    }

    #[test]
    fn a_missing_required_action_lists_the_valid_actions() {
        let err = resolve_action("brain_refactor", None).unwrap_err();
        assert_eq!(
            err,
            "brain_refactor: missing 'action' (one of: rename, merge, delete)"
        );
    }

    #[test]
    fn a_missing_action_falls_back_to_the_default() {
        assert_eq!(resolve_action("brain_history", None), Ok(Some("list")));
    }

    #[test]
    fn an_unknown_action_is_refused() {
        let err = resolve_action("brain_dream", Some(&json!("sleep"))).unwrap_err();
        assert!(err.contains("unknown action 'sleep'"), "{err}");
    }

    #[test]
    fn a_tool_without_actions_resolves_to_none() {
        assert_eq!(resolve_action("brain_search", Some(&json!("x"))), Ok(None));
    }
}
