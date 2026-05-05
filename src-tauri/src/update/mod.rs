//! S04 — Auto-Update via Release Repository.
//!
//! The actual download/install pipeline is delegated to `tauri-plugin-updater`.
//! This module owns the policy: channel filter, skip-list, signature
//! verification, and the user-facing prompt. Vault data is never touched.

pub mod commands;
pub mod skip_list;
pub mod verify;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("network: {0}")]
    Network(String),

    #[error("signature verification failed")]
    SignatureMismatch,

    #[error("no update available")]
    NoUpdate,
}

pub type UpdateResult<T> = Result<T, UpdateError>;
