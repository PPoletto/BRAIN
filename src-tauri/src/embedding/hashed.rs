//! Feature-hashing embedder — deterministic, model-free, pure Rust.
//!
//! Produces L2-normalised 1024-d vectors via the well-known "hashing trick":
//! each token hashes to a bucket in [0, dim), with a second hash bit
//! deciding the sign. Cosine similarity over two such vectors approximates
//! the Jaccard overlap of their token sets, which gives Brain a meaningful
//! semantic signal *without* a 2 GB neural model.
//!
//! Quality is good enough for retrieval over personal-scale wikis. When the
//! bge-m3 candle pipeline lands, the only swap needed is the `Embedder`
//! implementation registered with `AppState`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use unicode_segmentation::UnicodeSegmentation;

use super::{Embedder, EMBED_DIM};

#[derive(Debug, Clone, Default)]
pub struct HashedEmbedder;

impl HashedEmbedder {
    pub fn new() -> Self {
        Self
    }
}

impl Embedder for HashedEmbedder {
    fn dim(&self) -> usize {
        EMBED_DIM
    }
    fn name(&self) -> &'static str {
        "hashed-fh-1024"
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; EMBED_DIM];
        for token in tokens(text) {
            // Two hashes — one for the bucket, one for the sign.
            let bucket = hash_with_seed(&token, 0xb1aa_1ce5).rem_euclid(EMBED_DIM as i64) as usize;
            let sign = if hash_with_seed(&token, 0x00c0_ffee_5eed) & 1 == 0 {
                1.0f32
            } else {
                -1.0f32
            };
            v[bucket] += sign;
        }
        l2_normalise(&mut v);
        v
    }
}

fn hash_with_seed(token: &str, seed: u64) -> i64 {
    let mut h = DefaultHasher::new();
    seed.hash(&mut h);
    token.hash(&mut h);
    h.finish() as i64
}

fn l2_normalise(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.unicode_words()
        .map(str::to_lowercase)
        .filter(|w| w.len() >= 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_has_the_advertised_dimension() {
        let e = HashedEmbedder::new();
        assert_eq!(e.dim(), EMBED_DIM);
        let v = e.embed("hello world");
        assert_eq!(v.len(), EMBED_DIM);
    }

    #[test]
    fn embedding_is_l2_unit_length_for_non_empty_text() {
        let e = HashedEmbedder::new();
        let v = e.embed("the methodology behind nlspec specs");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm was {norm}");
    }

    #[test]
    fn embedding_is_zero_vector_for_empty_text() {
        let e = HashedEmbedder::new();
        let v = e.embed("");
        assert!(v.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn related_texts_have_higher_cosine_than_unrelated_ones() {
        let e = HashedEmbedder::new();
        let a = e.embed("alice talks about nlspec methodology");
        let b = e.embed("nlspec methodology specifies behaviour");
        let c = e.embed("totally unrelated weather forecast");
        let ab = super::super::cosine(&a, &b);
        let ac = super::super::cosine(&a, &c);
        assert!(
            ab > ac,
            "related cosine ({ab}) should exceed unrelated ({ac})"
        );
    }

    #[test]
    fn embedding_is_deterministic_across_calls() {
        let e = HashedEmbedder::new();
        let v1 = e.embed("the brain client");
        let v2 = e.embed("the brain client");
        assert_eq!(v1, v2);
    }
}
