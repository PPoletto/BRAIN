//! S06 — MCP Integration and LLM-Client Registration (MVP scope, no Cron).

pub mod commands;
pub mod registration;
pub mod routing;
pub mod server;
pub mod tools;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("vault: {0}")]
    Vault(#[from] crate::vault::VaultError),

    #[error("server not running")]
    NotRunning,

    #[error("port unavailable in range")]
    PortUnavailable,

    #[error("client integration not supported on this platform")]
    UnsupportedPlatform,
}

pub type McpResult<T> = Result<T, McpError>;
