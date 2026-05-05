//! Top-level error type aggregating module-specific errors.

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BrainError {
    #[error("vault error: {0}")]
    Vault(#[from] crate::vault::VaultError),

    #[error("onboarding error: {0}")]
    Onboarding(#[from] crate::onboarding::OnboardingError),

    #[error("mount error: {0}")]
    Mount(#[from] crate::mount::MountError),

    #[error("wiki error: {0}")]
    Wiki(#[from] crate::wiki::WikiError),

    #[error("mcp error: {0}")]
    Mcp(#[from] crate::mcp::McpError),

    #[error("viewer error: {0}")]
    Viewer(#[from] crate::viewer::ViewerError),

    #[error("update error: {0}")]
    Update(#[from] crate::update::UpdateError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal: {0}")]
    Internal(String),
}

impl Serialize for BrainError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type BrainResult<T> = Result<T, BrainError>;
