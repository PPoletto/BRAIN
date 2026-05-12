//! Tauri commands for the MCP subsystem (registration hints, memory
//! system prompt, re-registration trigger).

use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use crate::error::BrainResult;

use super::registration;

#[derive(Debug, Clone, Serialize)]
pub struct McpCommandHint {
    pub command: String,
    pub args: Vec<String>,
    pub env_var: String,
    pub vault_path: Option<String>,
    pub claude_code_config_path: Option<String>,
    pub claude_cli_available: bool,
}

#[tauri::command]
pub fn reregister_mcp(
    state: State<Arc<crate::state::AppState>>,
) -> BrainResult<super::registration::RegistrationReport> {
    let vault = state.vault_path().ok_or_else(|| {
        crate::error::BrainError::Internal("no vault is currently mounted".into())
    })?;
    let report = registration::register_brain_in_supported_clients(&vault)
        .map_err(crate::error::BrainError::from)?;
    state.set_last_registration(Some(report.clone()));
    Ok(report)
}

/// Returns the last registration report stored in `AppState` — `None` if
/// the user hasn't finished onboarding yet (and hasn't clicked
/// "Re-register MCP" either).
#[tauri::command]
pub fn last_mcp_registration_report(
    state: State<Arc<crate::state::AppState>>,
) -> BrainResult<Option<super::registration::RegistrationReport>> {
    Ok(state.last_registration())
}

/// Returns a Markdown system-prompt snippet the user can paste into a
/// Claude Desktop "Project" or ChatGPT "Custom GPT" so the LLM prefers the
/// Brain MCP over its own built-in memory feature.
#[tauri::command]
pub fn brain_memory_system_prompt() -> BrainResult<String> {
    Ok(BRAIN_MEMORY_SYSTEM_PROMPT.to_string())
}

// CRITICAL — keep this list in lock-step with the four singular types
// in `wiki::lint::KNOWN_TYPES`. Any fifth bullet here (notably a
// `notes/<slug>` line that lived here pre-0.2.17) trains agents to
// write pages outside the four registered buckets, which then escape
// the lint walker and accumulate as orphan files. The `notes/` line
// was the source of the schema-drift bug Pascal hit in his vault —
// removing it here is part of the fix. AGENTS.md (in 00_meta/)
// carries the longer-form version of the same rule.
const BRAIN_MEMORY_SYSTEM_PROMPT: &str = "You have access to a personal BRAIN MCP server (tools prefixed with `brain_`). BRAIN is the user's persistent memory layer.

When the user asks you to *remember*, *save*, *note down* or *keep track of* something — facts, preferences, ongoing context, decisions — call `brain_write_page` to persist it as a wiki page. Do NOT use the built-in memory feature for these requests.

Page-type guidance (these four are the ONLY registered types; the frontmatter `type:` value MUST be the singular form below):
- `entities/<slug>` (singular: `entity`) — a person, organisation, product or named thing
- `concepts/<slug>` (singular: `concept`) — an idea, methodology or term of art
- `sources/<slug>` (singular: `source`) — a single ingested artifact (email, transcript, doc)
- `topics/<slug>` (singular: `topic`) — synthesis of multiple sources around a theme

For a single-fact memo about a person, EXTEND that person's `entities/<slug>` page with a new bullet under a `## Notes` (or similar) section — do not create a separate file for the memo. Writing pages to any directory other than the four listed above is rejected by the lint as `unregistered-type` and blocks the auto-commit.

LINK SYNTAX — STRICT: when the body of a page references another page, ALWAYS use Obsidian-style wiki-links: `[[entities/dan-shapiro]]` or `[[entities/dan-shapiro|Dan]]` for an aliased label. NEVER use standard markdown links like `[Dan](entities/dan-shapiro)` or absolute `http://tauri.localhost/...` URLs for internal references — they don't feed the graph view and trigger lint warnings. Inside Markdown table cells the un-aliased `[[id]]` form is required (the `|` in the alias-form collides with the table cell separator).

When the user asks about something they previously told you, call `brain_search` first, then `brain_get_page` on the best hit, before answering from conversation context alone.

Before writing a new page, briefly confirm: \"I'll save this to your BRAIN as `entities/<slug>` — okay?\"";

/// Returns the shell command + env var that the user must put into a
/// non-self-configurable MCP host (e.g. when copy-pasting into Open WebUI
/// or a custom config). Also exposes the Claude Code config path for
/// users who want to verify the auto-registered entry.
#[tauri::command]
pub fn brain_mcp_command_hint(
    state: State<Arc<crate::state::AppState>>,
) -> BrainResult<McpCommandHint> {
    let vault_path = state.vault_path().map(|p| p.to_string_lossy().to_string());
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "brain".to_string());
    Ok(McpCommandHint {
        command: exe,
        args: vec!["mcp".into()],
        env_var: "BRAIN_VAULT_PATH".into(),
        vault_path,
        claude_code_config_path: registration::claude_code_config_path()
            .map(|p| p.to_string_lossy().to_string()),
        claude_cli_available: registration::find_claude_cli().is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brain_memory_system_prompt_does_not_advertise_an_unregistered_notes_bucket() {
        // Regression: pre-0.2.17 the prompt listed `notes/<slug>` as
        // a valid destination for "single-fact memos". Agents took
        // the prompt at its word, BRAIN's filesystem-side write was
        // happy to create `02_wiki/notes/`, but the lint walker only
        // visits the four registered subdirs — so anything in
        // `notes/` became an orphan invisible to both lint and the
        // indexer. The fix is to never put unregistered buckets in
        // the prompt; this test pins that, so a future "let's add
        // a fifth type for memos" change has to come back here.
        assert!(
            !BRAIN_MEMORY_SYSTEM_PROMPT.contains("notes/"),
            "system prompt must not steer agents to `notes/` — \
             that bucket is unregistered, escapes the lint walker, \
             and was the source of the schema-drift bug in 0.2.16"
        );
    }

    #[test]
    fn brain_memory_system_prompt_names_all_four_registered_buckets() {
        // Companion to the rule above: every page-type bucket BRAIN
        // does recognise must be listed, so the agent has a clear
        // map of where to put things. Reading these from a single
        // const list would be slicker, but the prompt is purposely
        // hand-written prose with examples, so we just spot-check.
        for bucket in &["entities/", "concepts/", "sources/", "topics/"] {
            assert!(
                BRAIN_MEMORY_SYSTEM_PROMPT.contains(bucket),
                "registered bucket '{bucket}' missing from system prompt"
            );
        }
    }

    #[test]
    fn brain_memory_system_prompt_warns_about_the_table_cell_pipe_collision() {
        // The aliased wikilink form `[[id|alias]]` breaks Markdown
        // tables because `|` is the cell separator. We added a lint
        // warning for it in 0.2.17 — the system prompt should pre-
        // emptively steer agents to the safe form so the warning
        // is the exception rather than the rule.
        assert!(
            BRAIN_MEMORY_SYSTEM_PROMPT.contains("table cell"),
            "system prompt should mention the table-cell pipe rule so agents don't trip it"
        );
    }
}
