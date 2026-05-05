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
    fn generate_bearer_token_returns_64_hex_chars_for_256_bit() {
        let t = generate_bearer_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
