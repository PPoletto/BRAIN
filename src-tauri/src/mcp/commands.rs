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

const BRAIN_MEMORY_SYSTEM_PROMPT: &str = "You have access to a personal BRAIN MCP server (tools prefixed with `brain_`). BRAIN is the user's persistent memory layer.

When the user asks you to *remember*, *save*, *note down* or *keep track of* something — facts, preferences, ongoing context, decisions — call `brain_write_page` to persist it as a wiki page. Do NOT use the built-in memory feature for these requests.

Page-type guidance:
- `entities/<slug>` for a person, organisation or product
- `concepts/<slug>` for an idea, methodology or term of art
- `notes/<slug>` for a single-fact memo, preference or scratch note
- `topics/<slug>` for synthesis of multiple sources

LINK SYNTAX — STRICT: when the body of a page references another page, ALWAYS use Obsidian-style wiki-links: `[[entities/dan-shapiro]]` or `[[entities/dan-shapiro|Dan]]` for an aliased label. NEVER use standard markdown links like `[Dan](entities/dan-shapiro)` or absolute `http://tauri.localhost/...` URLs for internal references — they don't feed the graph view and trigger lint warnings.

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
