//! Local embedding pipeline.
//!
//! The MVP ships a deterministic, pure-Rust feature-hashing embedder that
//! produces 1024-dimensional vectors without external models. This unblocks
//! the hybrid search infrastructure (chunks table, vector storage, score
//! fusion) end-to-end. Real semantic quality requires the bge-m3 model;
//! once `candle::Bge` lands the trait can be swapped without touching the
//! callers.
//!
//! All vectors are L2-normalised so cosine similarity is just a dot product.

pub mod bge_m3;
pub mod chunk;
pub mod download;
pub mod hashed;

use std::path::Path;
use std::sync::Arc;

/// Build the best available embedder for `vault`. Prefers `bge-m3` when its
/// model files are present in `04_models/bge-m3/`; falls back to the
/// deterministic `HashedEmbedder` otherwise.
///
/// Falling back is intentional: search must work on a freshly-onboarded
/// vault before the user has downloaded ~2 GB of weights.
pub fn for_vault(vault: &Path) -> Arc<dyn Embedder> {
    let model_dir = crate::vault::layout::models_dir(vault).join("bge-m3");
    if has_full_model(&model_dir) {
        match bge_m3::BgeM3Embedder::try_new(&model_dir) {
            Ok(e) => return Arc::new(e),
            Err(err) => {
                tracing::warn!(?err, "bge-m3 init failed, falling back to hashed embedder");
            }
        }
    }
    Arc::new(hashed::HashedEmbedder::new())
}

fn has_full_model(dir: &Path) -> bool {
    // BAAI/bge-m3 ships `pytorch_model.bin`, not `model.safetensors` — see
    // `embedding::download::MODEL_FILES`.
    let required = [
        "pytorch_model.bin",
        "tokenizer.json",
        "config.json",
        "sentencepiece.bpe.model",
    ];
    required.iter().all(|f| dir.join(f).exists())
}

pub const EMBED_DIM: usize = 1024;

/// Trait every embedder must implement. Pure-CPU, blocking — callers wrap
/// it in `tokio::spawn_blocking` if needed.
pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Vec<f32>;
    fn name(&self) -> &'static str;
}

/// Cosine similarity over two L2-normalised vectors → just a dot product.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut sum = 0.0f32;
    for i in 0..n {
        sum += a[i] * b[i];
    }
    sum
}

/// Encode a vector as little-endian bytes for SQLite BLOB storage.
pub fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Decode an LE-byte vector back into floats. Returns an empty vec when the
/// blob length isn't a multiple of 4.
pub fn bytes_to_vec(bytes: &[u8]) -> Vec<f32> {
    if bytes.len() % 4 != 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_of_a_unit_vector_with_itself_is_one() {
        let v = vec![0.6f32, 0.8, 0.0, 0.0];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_of_orthogonal_vectors_is_zero() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        assert!(cosine(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn vec_to_bytes_round_trips_through_bytes_to_vec() {
        let v = vec![1.0f32, -0.5, 0.25, 1024.0];
        let b = vec_to_bytes(&v);
        let parsed = bytes_to_vec(&b);
        assert_eq!(parsed.len(), v.len());
        for i in 0..v.len() {
            assert!((parsed[i] - v[i]).abs() < 1e-6);
        }
    }

    #[test]
    fn bytes_to_vec_returns_empty_for_misaligned_input() {
        assert!(bytes_to_vec(&[1, 2, 3]).is_empty());
    }
}
