//! Viewer backend covering S08 Tier 1 (read), S09 Tier 2 (search +
//! backlinks), and S10 Tier 3 (graph data).

pub mod commands;
pub mod graph;
pub mod query;
pub mod search;
pub mod tree;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ViewerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("wiki: {0}")]
    Wiki(#[from] crate::wiki::WikiError),

    #[error("page not found: {0}")]
    PageNotFound(String),
}

pub type ViewerResult<T> = Result<T, ViewerError>;
