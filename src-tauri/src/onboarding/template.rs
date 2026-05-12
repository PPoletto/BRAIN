//! Template population for a freshly initialized vault.
//!
//! Writes the canonical `AGENTS.md`, `CLAUDE.md`, and `.mcp.json` files into
//! the vault's `00_meta/`. Idempotent: existing files are not overwritten.

use std::path::Path;

use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::vault::layout::{
    meta_dir, AGENTS_FILENAME, CLAUDE_FILENAME, MCP_CONFIG_FILENAME,
};
use crate::vault::VaultResult;

const AGENTS_MD: &str = include_str!("templates/AGENTS.md");
const CLAUDE_MD: &str = include_str!("templates/CLAUDE.md");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    pub version: u32,
    pub brain: BrainServer,
    pub external_servers: Vec<ExternalServer>,
    pub internal_routing: InternalRouting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainServer {
    pub transports: Vec<String>,
    pub http: BrainHttp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainHttp {
    pub host: String,
    pub port_strategy: String,
    pub bearer_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalServer {
    pub id: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: serde_json::Map<String, serde_json::Value>,
    pub enabled: bool,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InternalRouting {
    pub default_provider: String,
    pub rules: Vec<RoutingRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    pub path_prefix: String,
    pub provider: String,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            version: 1,
            brain: BrainServer {
                transports: vec!["stdio".into(), "http".into()],
                http: BrainHttp {
                    host: "127.0.0.1".into(),
                    port_strategy: "first-free-from-7137".into(),
                    bearer_token: None,
                },
            },
            external_servers: Vec::new(),
            internal_routing: InternalRouting {
                default_provider: "local".into(),
                rules: vec![
                    RoutingRule {
                        path_prefix: "01_raw/email/personal/".into(),
                        provider: "local".into(),
                    },
                    RoutingRule {
                        path_prefix: "01_raw/email/work/".into(),
                        provider: "anthropic".into(),
                    },
                ],
            },
        }
    }
}

/// Populates canonical files only when missing — idempotent.
pub fn populate(vault: &Path) -> VaultResult<()> {
    let meta = meta_dir(vault);
    std::fs::create_dir_all(&meta)?;

    write_if_missing(&meta.join(AGENTS_FILENAME), AGENTS_MD.as_bytes())?;
    write_if_missing(&meta.join(CLAUDE_FILENAME), CLAUDE_MD.as_bytes())?;

    let mcp_path = meta.join(MCP_CONFIG_FILENAME);
    if !mcp_path.exists() {
        let cfg = McpConfig::default();
        let raw = serde_json::to_string_pretty(&cfg)?;
        std::fs::write(mcp_path, raw)?;
    }
    Ok(())
}

fn write_if_missing(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

/// Single-file entry in the refresh report. The frontend uses it to
/// build a human-readable summary toast ("AGENTS.md: 2924 → 10066 B,
/// CLAUDE.md unchanged") so the user sees exactly what changed
/// without diffing themselves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TemplateUpdateEntry {
    pub path: String,
    /// `created`, `overwritten`, or `unchanged`.
    pub action: String,
    pub size_before: u64,
    pub size_after: u64,
}

/// Force-overwrites the bundled vault templates (AGENTS.md, CLAUDE.md)
/// in `00_meta/`. The mirror of `populate()` for an existing vault —
/// `populate()` is idempotent and skips existing files (correct on
/// first boot), but the user occasionally needs to pull a newer
/// template (new tools, fixed conventions, schema corrections) into
/// their existing vault. Wired to the "Update vault templates" button
/// in Settings → Danger. `.mcp.json` is deliberately NOT touched —
/// the user customises that with their bearer token / external MCP
/// servers / routing rules, so blowing it away would lose state.
///
/// Returns one entry per template file so the UI can show whether
/// each was overwritten or left unchanged (no-op when contents are
/// byte-identical, so re-clicking the button on a fresh vault is safe).
pub fn refresh_vault_templates(vault: &Path) -> VaultResult<Vec<TemplateUpdateEntry>> {
    let meta = meta_dir(vault);
    std::fs::create_dir_all(&meta)?;
    let mut out = Vec::with_capacity(2);
    for (filename, contents) in [
        (AGENTS_FILENAME, AGENTS_MD),
        (CLAUDE_FILENAME, CLAUDE_MD),
    ] {
        let path = meta.join(filename);
        let entry = write_or_replace(&path, contents.as_bytes())?;
        out.push(entry);
    }
    Ok(out)
}

/// Helper for `refresh_vault_templates`: writes the bundled contents,
/// reports what action it took. Reads the existing file's size before
/// truncating so the UI can show the delta — important when an agent
/// has been editing the file directly and the user wants to know
/// whether they're about to lose customisations.
fn write_or_replace(path: &Path, contents: &[u8]) -> std::io::Result<TemplateUpdateEntry> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path_str = path.to_string_lossy().to_string();
    let new_size = contents.len() as u64;
    let (action, size_before) = match std::fs::metadata(path) {
        Ok(meta) => {
            let prev = meta.len();
            // Skip the write entirely when the file is already the
            // bundled version (saves a disk write and keeps the
            // "unchanged" row honest in the report).
            let existing = std::fs::read(path).unwrap_or_default();
            if existing == contents {
                return Ok(TemplateUpdateEntry {
                    path: path_str,
                    action: "unchanged".into(),
                    size_before: prev,
                    size_after: prev,
                });
            }
            ("overwritten", prev)
        }
        Err(_) => ("created", 0),
    };
    std::fs::write(path, contents)?;
    Ok(TemplateUpdateEntry {
        path: path_str,
        action: action.into(),
        size_before,
        size_after: new_size,
    })
}

/// Generates a random 256-bit bearer token, hex-encoded.
pub fn generate_bearer_token() -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::ensure_skeleton;
    use tempfile::TempDir;

    #[test]
    fn populate_creates_agents_claude_and_mcp_config_files() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        populate(tmp.path()).unwrap();
        let m = meta_dir(tmp.path());
        assert!(m.join(AGENTS_FILENAME).exists());
        assert!(m.join(CLAUDE_FILENAME).exists());
        assert!(m.join(MCP_CONFIG_FILENAME).exists());
    }

    #[test]
    fn populate_does_not_overwrite_existing_user_modified_files() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let agents = meta_dir(tmp.path()).join(AGENTS_FILENAME);
        std::fs::write(&agents, "user-modified").unwrap();
        populate(tmp.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&agents).unwrap(), "user-modified");
    }

    #[test]
    fn default_mcp_config_has_local_default_provider_and_two_routing_rules() {
        let cfg = McpConfig::default();
        assert_eq!(cfg.internal_routing.default_provider, "local");
        assert_eq!(cfg.internal_routing.rules.len(), 2);
        assert!(cfg.brain.http.bearer_token.is_none());
    }

    #[test]
    fn refresh_vault_templates_overwrites_an_outdated_agents_md_with_the_bundled_version() {
        // The user-flow this exercises: a vault was created back when
        // an older AGENTS.md template shipped with the BRAIN binary,
        // the user updates BRAIN, and now wants the new conventions
        // pulled into their existing vault without re-running
        // onboarding.
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let agents = meta_dir(tmp.path()).join(AGENTS_FILENAME);
        let stale = "# old AGENTS.md content that pre-dates the schema fix";
        std::fs::write(&agents, stale).unwrap();

        let report = refresh_vault_templates(tmp.path()).unwrap();
        let actions: Vec<&str> = report.iter().map(|e| e.action.as_str()).collect();
        assert!(
            actions.contains(&"overwritten"),
            "stale AGENTS.md must be overwritten, got actions: {actions:?}"
        );
        // After: file contents match the bundled template.
        let after = std::fs::read_to_string(&agents).unwrap();
        assert_eq!(after, AGENTS_MD, "bundled template content must be on disk verbatim");
        // size_before tracked the stale length, size_after the new.
        let agents_entry = report
            .iter()
            .find(|e| e.path.contains(AGENTS_FILENAME))
            .unwrap();
        assert_eq!(agents_entry.size_before as usize, stale.len());
        assert_eq!(agents_entry.size_after as usize, AGENTS_MD.len());
    }

    #[test]
    fn refresh_vault_templates_reports_unchanged_when_file_is_already_the_bundled_version() {
        // Re-clicking the button on an up-to-date vault must be a
        // no-op: file isn't rewritten (no mtime churn that would
        // confuse Git or the auto-commit watcher), and the report
        // says "unchanged" so the UI doesn't claim work it didn't do.
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let agents = meta_dir(tmp.path()).join(AGENTS_FILENAME);
        std::fs::write(&agents, AGENTS_MD).unwrap();
        let mtime_before = std::fs::metadata(&agents).unwrap().modified().unwrap();
        // Small sleep so a stray write would produce a different mtime.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let report = refresh_vault_templates(tmp.path()).unwrap();
        let agents_entry = report
            .iter()
            .find(|e| e.path.contains(AGENTS_FILENAME))
            .unwrap();
        assert_eq!(agents_entry.action, "unchanged");
        let mtime_after = std::fs::metadata(&agents).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "unchanged action must not rewrite the file (would churn the watcher)"
        );
    }

    #[test]
    fn refresh_vault_templates_creates_files_in_a_meta_dir_that_did_not_have_them_yet() {
        // Edge case: a vault initialized before AGENTS.md/CLAUDE.md
        // were part of populate() (or where they were deleted). The
        // refresh must *create* them, not bail because the file
        // wasn't there to read.
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        // Note: we did NOT call populate() — the meta dir is empty.
        let report = refresh_vault_templates(tmp.path()).unwrap();
        assert!(
            report.iter().all(|e| e.action == "created"),
            "expected every entry to be 'created' when meta started empty: {report:?}"
        );
        assert!(meta_dir(tmp.path()).join(AGENTS_FILENAME).exists());
        assert!(meta_dir(tmp.path()).join(CLAUDE_FILENAME).exists());
    }

    #[test]
    fn refresh_vault_templates_does_not_touch_mcp_config_so_user_settings_survive() {
        // Critical contract: .mcp.json carries the user's bearer
        // token, external MCP servers and routing rules — losing
        // those on a templates-refresh would be a user-data loss
        // event. Verify the file's contents survive byte-for-byte.
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        populate(tmp.path()).unwrap();
        let mcp = meta_dir(tmp.path()).join(MCP_CONFIG_FILENAME);
        let user_customised = r#"{ "version": 1, "user_secret": "DO NOT LOSE" }"#;
        std::fs::write(&mcp, user_customised).unwrap();

        let _ = refresh_vault_templates(tmp.path()).unwrap();

        let after = std::fs::read_to_string(&mcp).unwrap();
        assert_eq!(
            after, user_customised,
            "MCP config must survive a templates refresh untouched"
        );
    }

    #[test]
    fn generate_bearer_token_returns_64_hex_chars_for_256_bit() {
        let t = generate_bearer_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
