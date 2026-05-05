//! bge-m3 dense embeddings via candle (XLM-RoBERTa architecture).
//!
//! BAAI/bge-m3 is a multilingual sentence-embedding model fine-tuned from
//! XLM-RoBERTa-large. The dense retrieval head is the **CLS token** of the
//! last hidden state, L2-normalised. The output is a 1024-d vector — the
//! `EMBED_DIM` constant the rest of Brain relies on.
//!
//! Required files in `<vault>/04_models/bge-m3/`:
//!
//! - `pytorch_model.bin`     — model weights (PyTorch pickle, ~2.3 GB).
//!   BAAI/bge-m3 doesn't publish a `model.safetensors`, so we use candle's
//!   pickle loader (`VarBuilder::from_pth`) for this one.
//! - `tokenizer.json`        — fast HuggingFace tokenizer
//! - `config.json`           — XLM-RoBERTa config (hidden_size, num_layers, …)
//! - `sentencepiece.bpe.model` — used by the slow tokenizer; the fast one in
//!   `tokenizer.json` is enough for us, but we keep this file in the
//!   "complete model" check (`embedding::has_full_model`) so callers know
//!   the download finished.
//!
//! Inference is CPU-only and synchronous. Callers (`pages_index::rebuild`,
//! `viewer::search::search_hybrid`) wrap it in a blocking task when needed.
//!
//! When any of the required files are missing or weight-loading fails, the
//! module returns a typed error and the caller (`embedding::for_vault`)
//! falls back to `HashedEmbedder`. Search still works on a freshly-onboarded
//! vault before the user has downloaded the ~2.3 GB of weights.

use std::path::Path;
use std::sync::Mutex;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::xlm_roberta::{Config, XLMRobertaModel};
use tokenizers::Tokenizer;

use super::{Embedder, EMBED_DIM};

/// Hard cap on tokens fed to the model. bge-m3 supports up to 8192, but
/// inference cost is quadratic in sequence length on CPU. 512 is the
/// sentence-transformers default and keeps a single embed under ~150 ms on
/// a modern laptop. Chunks are already short (`embedding::chunk`), so this
/// only kicks in for unusually long blocks.
const MAX_TOKENS: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum BgeM3Error {
    #[error("model file missing: {0}")]
    MissingFile(String),
    #[error("config.json could not be parsed: {0}")]
    BadConfig(String),
    #[error("tokenizer.json could not be loaded: {0}")]
    BadTokenizer(String),
    #[error("model weights could not be loaded: {0}")]
    BadWeights(String),
    #[error("model hidden size {actual} doesn't match expected {expected}")]
    HiddenSizeMismatch { actual: usize, expected: usize },
}

pub struct BgeM3Embedder {
    model: XLMRobertaModel,
    /// `Tokenizer::encode` takes `&self`, but mutating padding/truncation
    /// state via `with_padding` requires `&mut self`. We pre-configure the
    /// tokenizer in `try_new`, then guard reads with a `Mutex` only because
    /// downstream `tokenizers` versions internally mutate string buffers
    /// during encode in some configurations. A single contended mutex is
    /// fine — embedding latency is dominated by the forward pass anyway.
    tokenizer: Mutex<Tokenizer>,
    device: Device,
    pad_token_id: u32,
}

// SAFETY: candle's `XLMRobertaModel` only holds immutable parameter tensors
// and tracing spans, so it's `Send + Sync` even though some inner types
// don't implement them automatically. The `Tokenizer` is wrapped in a
// `Mutex` so the whole struct is trivially `Send + Sync`.
unsafe impl Send for BgeM3Embedder {}
unsafe impl Sync for BgeM3Embedder {}

impl std::fmt::Debug for BgeM3Embedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BgeM3Embedder")
            .field("device", &self.device)
            .field("pad_token_id", &self.pad_token_id)
            .finish_non_exhaustive()
    }
}

impl BgeM3Embedder {
    pub fn try_new(model_dir: &Path) -> Result<Self, BgeM3Error> {
        let config_path = model_dir.join("config.json");
        // BAAI/bge-m3 only publishes pytorch_model.bin. We accept a
        // safetensors variant too in case a downstream mirror provides one
        // (and to make swapping in a fine-tune easier).
        let pth_path = model_dir.join("pytorch_model.bin");
        let safetensors_path = model_dir.join("model.safetensors");
        let tokenizer_path = model_dir.join("tokenizer.json");

        if !config_path.exists() {
            return Err(BgeM3Error::MissingFile(config_path.display().to_string()));
        }
        if !tokenizer_path.exists() {
            return Err(BgeM3Error::MissingFile(tokenizer_path.display().to_string()));
        }
        if !pth_path.exists() && !safetensors_path.exists() {
            return Err(BgeM3Error::MissingFile(format!(
                "{} or {}",
                pth_path.display(),
                safetensors_path.display()
            )));
        }

        let config_raw = std::fs::read_to_string(&config_path)
            .map_err(|e| BgeM3Error::BadConfig(e.to_string()))?;
        let config: Config = serde_json::from_str(&config_raw)
            .map_err(|e| BgeM3Error::BadConfig(e.to_string()))?;
        if config.hidden_size != EMBED_DIM {
            return Err(BgeM3Error::HiddenSizeMismatch {
                actual: config.hidden_size,
                expected: EMBED_DIM,
            });
        }

        let device = Device::Cpu;

        let vb = if safetensors_path.exists() {
            // SAFETY: `from_mmaped_safetensors` mmap's the file read-only;
            // the storage is reference-counted, so the file outlives `vb`.
            unsafe {
                VarBuilder::from_mmaped_safetensors(
                    &[&safetensors_path],
                    DType::F32,
                    &device,
                )
                .map_err(|e| BgeM3Error::BadWeights(e.to_string()))?
            }
        } else {
            VarBuilder::from_pth(&pth_path, DType::F32, &device)
                .map_err(|e| BgeM3Error::BadWeights(e.to_string()))?
        };

        // BAAI/bge-m3 saves weights with no `roberta.` prefix (it's saved as
        // a bare `XLMRobertaModel`, not `…ForMaskedLM`). Try the bare layout
        // first; fall back to the prefixed layout for community variants.
        let model = match XLMRobertaModel::new(&config, vb.clone()) {
            Ok(m) => m,
            Err(_) => XLMRobertaModel::new(&config, vb.pp("roberta"))
                .map_err(|e| BgeM3Error::BadWeights(e.to_string()))?,
        };

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| BgeM3Error::BadTokenizer(e.to_string()))?;
        // We never batch-encode here, so padding is unnecessary; truncation
        // we do ourselves below to stay under MAX_TOKENS.
        let _ = tokenizer.with_padding(None);
        let _ = tokenizer.with_truncation(None);

        Ok(Self {
            model,
            tokenizer: Mutex::new(tokenizer),
            device,
            pad_token_id: config.pad_token_id,
        })
    }

    /// Inner forward — separated from the trait method so we can use `?` on
    /// candle and tokenizer errors. Returns the L2-normalised CLS embedding.
    fn embed_inner(&self, text: &str) -> Result<Vec<f32>, BgeM3Error> {
        let encoding = {
            let tok = self
                .tokenizer
                .lock()
                .map_err(|e| BgeM3Error::BadTokenizer(format!("mutex poisoned: {e}")))?;
            tok.encode(text, true)
                .map_err(|e| BgeM3Error::BadTokenizer(e.to_string()))?
        };

        let ids: Vec<u32> = encoding.get_ids().to_vec();
        let attn: Vec<u32> = encoding.get_attention_mask().to_vec();

        // Truncate to MAX_TOKENS while keeping the leading [CLS] token.
        let len = ids.len().clamp(1, MAX_TOKENS);
        let ids = &ids[..len];
        let attn = &attn[..len];

        let ids_t = Tensor::new(ids, &self.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(|e| BgeM3Error::BadWeights(e.to_string()))?;
        let attn_t = Tensor::new(attn, &self.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(|e| BgeM3Error::BadWeights(e.to_string()))?;
        // XLM-RoBERTa uses a single-segment input; token-type-ids are zero.
        let tt_t = Tensor::zeros((1, len), DType::U32, &self.device)
            .map_err(|e| BgeM3Error::BadWeights(e.to_string()))?;

        let last_hidden = self
            .model
            .forward(&ids_t, &attn_t, &tt_t, None, None, None)
            .map_err(|e| BgeM3Error::BadWeights(e.to_string()))?;

        // CLS pooling: take the first token of the last hidden state.
        // Shape: [1, seq, hidden] -> [1, hidden] -> [hidden].
        let cls = last_hidden
            .i((.., 0, ..))
            .map_err(|e| BgeM3Error::BadWeights(e.to_string()))?
            .squeeze(0)
            .map_err(|e| BgeM3Error::BadWeights(e.to_string()))?;

        let v: Vec<f32> = cls
            .to_vec1::<f32>()
            .map_err(|e| BgeM3Error::BadWeights(e.to_string()))?;

        Ok(l2_normalise(v))
    }
}

impl Embedder for BgeM3Embedder {
    fn dim(&self) -> usize {
        EMBED_DIM
    }
    fn name(&self) -> &'static str {
        "bge-m3"
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        if text.trim().is_empty() {
            return vec![0.0; EMBED_DIM];
        }
        match self.embed_inner(text) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(?err, "bge-m3 embed failed; returning zero vector");
                vec![0.0; EMBED_DIM]
            }
        }
    }
}

// `Tensor::i((.., 0, ..))` resolves through this trait.
use candle_core::IndexOp;

fn l2_normalise(mut v: Vec<f32>) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn try_new_returns_missing_file_when_model_dir_is_empty() {
        let err = BgeM3Embedder::try_new(Path::new("/nope")).unwrap_err();
        assert!(matches!(err, BgeM3Error::MissingFile(_)));
    }

    #[test]
    fn l2_normalise_makes_unit_length_vector() {
        let v = l2_normalise(vec![3.0, 4.0]);
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-6);
    }

    #[test]
    fn l2_normalise_keeps_zero_vector_zero() {
        let v = l2_normalise(vec![0.0, 0.0, 0.0]);
        assert!(v.iter().all(|x| *x == 0.0));
    }
}
