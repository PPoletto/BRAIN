//! Vault layout, marker handling and idempotent skeleton creation.
//!
//! See `docs/architecture.md` §2.1 and §2.2.

pub mod layout;
pub mod marker;
pub mod settings;

pub use layout::{ensure_skeleton, is_vault, BRAIN_MARKER_FILENAME, MCP_CONFIG_FILENAME};
pub use marker::{read_marker, write_marker, VaultMarker, BRAIN_FORMAT_V1};

use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("not a vault: {path}")]
    NotAVault { path: PathBuf },

    #[error("unsupported vault format: expected {expected}, got {actual}")]
    UnsupportedFormat { expected: String, actual: String },

    #[error("invalid marker contents: {0}")]
    InvalidMarker(String),
}

pub type VaultResult<T> = Result<T, VaultError>;
