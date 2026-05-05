//! S01 — Disk Detection and Mount Lifecycle.
//!
//! Provides volume-watcher integration (platform-specific watchers behind a
//! unified trait), vault detection via `00_meta/brain-marker.json`, and
//! mount/unmount transitions. Since the MVP does not encrypt the vault, the
//! exFAT volume itself serves as the mount path — there is no virtual
//! filesystem layer.

pub mod commands;
pub mod integrity;
pub mod lifecycle;
pub mod watcher;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MountError {
    #[error("vault: {0}")]
    Vault(#[from] crate::vault::VaultError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("source disappeared: {0}")]
    SourceLost(String),

    #[error("not currently mounted")]
    NotMounted,

    #[error("already mounted at {0}")]
    AlreadyMounted(String),
}

pub type MountResult<T> = Result<T, MountError>;

pub use lifecycle::{mount_source, unmount, UncleanFlag};
pub use watcher::{ChangeEvent, SourceWatcher};
