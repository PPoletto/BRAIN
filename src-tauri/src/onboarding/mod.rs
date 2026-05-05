//! S05 — Brain Initialization and Onboarding.
//!
//! Provides disk discovery, vault skeleton creation, template population and
//! embedding-model download for the onboarding wizard.

pub mod commands;
pub mod disks;
pub mod format;
pub mod init;
pub mod template;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OnboardingError {
    #[error("vault: {0}")]
    Vault(#[from] crate::vault::VaultError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("disk operation not supported on this platform: {0}")]
    UnsupportedOnPlatform(&'static str),

    #[error("disk not found: {0}")]
    DiskNotFound(String),

    #[error("download failed: {0}")]
    DownloadFailed(String),
}

pub type OnboardingResult<T> = Result<T, OnboardingError>;
