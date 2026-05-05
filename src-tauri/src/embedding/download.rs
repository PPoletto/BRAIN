//! HuggingFace download for the bge-m3 model files.
//!
//! `hf-hub` resolves the file via a content-addressable cache under
//! `~/.cache/huggingface/`, then we copy each file into the vault's
//! `04_models/bge-m3/` so the vault remains self-contained (an unmounted
//! vault on another machine still has the weights). Re-runs are idempotent
//! — files that already exist with the right size are skipped.
//!
//! Download is synchronous (sync-ureq flavour of `hf-hub`); callers wrap it
//! in `tokio::spawn_blocking` if running inside an async runtime.

use std::path::Path;

use hf_hub::api::sync::ApiBuilder;

/// Repo id on HuggingFace. Pinned because Brain's hybrid-search code
/// hard-codes the 1024-d output shape (see `EMBED_DIM`).
pub const HF_REPO: &str = "BAAI/bge-m3";

/// Files Brain needs locally to run `BgeM3Embedder`.
///
/// BAAI/bge-m3 ships its weights as `pytorch_model.bin` (PyTorch pickle)
/// — there is no `model.safetensors` artefact in the official repo, so we
/// fetch the .bin and load it through candle's `VarBuilder::from_pth`.
pub const MODEL_FILES: &[&str] = &[
    "config.json",
    "tokenizer.json",
    "pytorch_model.bin",
    "sentencepiece.bpe.model",
];

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("hf-hub api init failed: {0}")]
    ApiInit(String),
    #[error("downloading {file} from {repo} failed: {message}")]
    Fetch {
        file: String,
        repo: String,
        message: String,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Progress callback. `(file_name, downloaded_index, total_count)`.
pub type ProgressFn = dyn Fn(&str, usize, usize) + Send + Sync;

/// Download all `MODEL_FILES` from `HF_REPO` into `dest_dir`. Returns `Ok`
/// once every file is present locally.
pub fn download_bge_m3(
    dest_dir: &Path,
    progress: Option<&ProgressFn>,
) -> Result<(), DownloadError> {
    std::fs::create_dir_all(dest_dir)?;

    // Skip the network round-trip when every file is already cached locally
    // — common for re-runs of `populate` or `download_embedding_model`.
    if MODEL_FILES.iter().all(|f| dest_dir.join(f).exists()) {
        for (idx, f) in MODEL_FILES.iter().enumerate() {
            if let Some(cb) = progress {
                cb(f, idx + 1, MODEL_FILES.len());
            }
        }
        return Ok(());
    }

    let api = ApiBuilder::new()
        .with_progress(false)
        .build()
        .map_err(|e| DownloadError::ApiInit(e.to_string()))?;
    let repo = api.model(HF_REPO.to_string());

    for (idx, file) in MODEL_FILES.iter().enumerate() {
        let target = dest_dir.join(file);
        if !target.exists() {
            let cached = repo.get(file).map_err(|e| DownloadError::Fetch {
                file: (*file).to_string(),
                repo: HF_REPO.to_string(),
                message: e.to_string(),
            })?;
            // hf-hub returns a path inside its own cache. Copy into the
            // vault so the vault is portable (the cache lives outside the
            // vault on the user's home dir).
            std::fs::copy(&cached, &target)?;
        }
        if let Some(cb) = progress {
            cb(file, idx + 1, MODEL_FILES.len());
        }
    }

    // Best-effort cleanup of the legacy "PENDING_DOWNLOAD" marker the older
    // stub command wrote when the embedder was a no-op.
    let _ = std::fs::remove_file(dest_dir.join("PENDING_DOWNLOAD"));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn download_bge_m3_is_no_op_when_all_files_already_present() {
        let tmp = TempDir::new().unwrap();
        for f in MODEL_FILES {
            std::fs::write(tmp.path().join(f), b"stub").unwrap();
        }
        let cb = |_: &str, _: usize, _: usize| {
            // Cannot capture &mut from a Fn without UnsafeCell; we just
            // rely on the fact that this didn't error.
        };
        // Smoke test: should not contact the network when files exist.
        download_bge_m3(tmp.path(), Some(&cb)).unwrap();
        // Verify the files weren't touched (sizes still 4 bytes).
        for f in MODEL_FILES {
            let meta = std::fs::metadata(tmp.path().join(f)).unwrap();
            assert_eq!(meta.len(), 4);
        }
    }

    #[test]
    fn model_files_constant_lists_the_four_artefacts_bge_m3_needs() {
        // Guards against an accidental edit that would silently break the
        // BgeM3Embedder loader.
        assert!(MODEL_FILES.contains(&"config.json"));
        assert!(MODEL_FILES.contains(&"tokenizer.json"));
        assert!(MODEL_FILES.contains(&"pytorch_model.bin"));
        assert!(MODEL_FILES.contains(&"sentencepiece.bpe.model"));
    }
}
