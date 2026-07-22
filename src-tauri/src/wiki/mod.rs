//! S03 — Wiki Versioning and Recovery.
//!
//! Provides Git initialization with platform-tolerant config, an auto-commit
//! pipeline, lint, history reading, restore and hard-reset.

pub mod commands;
pub mod encryption;
pub mod git;
pub mod history;
pub mod lint;
pub mod meta_files;
pub mod page;
pub mod watcher;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WikiError {
    #[error("git2: {0}")]
    Git(#[from] git2::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("lint: {0}")]
    Lint(String),

    #[error("page not found: {0}")]
    PageNotFound(String),

    #[error("encryption: {0}")]
    Encryption(String),
}

pub type WikiResult<T> = Result<T, WikiError>;
