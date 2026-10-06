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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// Build the best available embedder for `vault`. Prefers `bge-m3` when its
/// model files are present in `04_models/bge-m3/`; falls back to the
/// deterministic `HashedEmbedder` otherwise.
///
/// Falling back is intentional: search must work on a freshly-onboarded
/// vault before the user has downloaded ~2 GB of weights.
///
/// This is the UNCACHED constructor: every call with a present model
/// re-reads the ~2.2 GB weights from disk. Production code must use
/// [`cached_for_vault`]; this stays public for tests and one-off tools.
pub fn for_vault(vault: &Path) -> Arc<dyn Embedder> {
    try_load_bge_m3(&bge_m3_dir(vault)).unwrap_or_else(hashed_fallback)
}

/// Like [`for_vault`], but loads the bge-m3 weights at most once per
/// process and model directory; later calls return the same `Arc`.
///
/// The cache lock is held WHILE the model loads. That is deliberate: two
/// concurrent callers (e.g. the mount warm-up and the first search, or the
/// background re-index) must never both read the 2.2 GB file and keep two
/// ~2.2 GB copies in RAM (the model loads as F32, ~568M parameters). The
/// second caller simply waits for the first load and then gets the cached
/// instance. The cost is that a cache hit for a different vault also waits
/// during a load — acceptable, since only one vault is mounted at a time.
///
/// A missing model is never cached: a fresh `HashedEmbedder` is returned,
/// so a model downloaded later is picked up on the next call. A model
/// whose files are all present but fail to load IS remembered (negative
/// cache keyed by a size/mtime fingerprint of the files), so a corrupt
/// download doesn't re-read 2.2 GB on every search and re-index. Changing
/// a fingerprinted file, or [`invalidate_embedder_cache`], retries.
///
/// A loaded model that goes unused for [`EMBEDDER_IDLE_TTL`] is dropped by
/// the periodic [`evict_idle_embedders`] check; the next call then reloads
/// it lazily (a few seconds).
pub fn cached_for_vault(vault: &Path) -> Arc<dyn Embedder> {
    global_cache().get_or_load(&bge_m3_dir(vault), try_load_bge_m3)
}

/// Drop every cached embedder and every remembered load failure. Call
/// after the model files changed on disk (e.g. a completed download) or
/// on unmount to release the model's RAM. May block while a load is in
/// progress (the cache lock is held during loads), so call it off the UI
/// thread. In-flight users keep their own `Arc`, so clearing is safe.
pub fn invalidate_embedder_cache() {
    global_cache().clear();
}

/// How long a loaded bge-m3 model may sit unused before
/// [`evict_idle_embedders`] drops it.
///
/// Trade-off: the model holds ~2.2 GB of RAM (F32 weights) for as long as
/// it is cached, while reloading it after a pause costs a few seconds on
/// the next search or re-index. 15 minutes keeps it resident through an
/// active working session (searches, auto-commits that re-index) and
/// gives the memory back once the user has moved on. After an eviction
/// the next caller reloads lazily; nothing re-warms automatically.
pub const EMBEDDER_IDLE_TTL: Duration = Duration::from_secs(15 * 60);

/// Drop every cached real model that has not been handed out for longer
/// than `max_idle`; returns how many were evicted (and logs at info level
/// when that is non-zero). Remembered load failures are kept: they are
/// tiny and stop a corrupt model from being re-read.
///
/// Never queues behind an in-progress model load: if the cache lock is
/// held, this round is skipped and 0 is returned — the next tick retries.
///
/// Safe while a search runs: every user holds its own `Arc` to the
/// embedder, so eviction only drops the cache's reference and the memory
/// is freed when the last in-flight user finishes.
pub fn evict_idle_embedders(max_idle: Duration) -> usize {
    let evicted = global_cache().evict_idle(max_idle);
    if let Some(longest_idle) = evicted.iter().max() {
        tracing::info!(
            evicted = evicted.len(),
            longest_idle_secs = longest_idle.as_secs(),
            ttl_secs = max_idle.as_secs(),
            "evicted idle embedding model(s) from the cache"
        );
    }
    evicted.len()
}

fn global_cache() -> &'static EmbedderCache {
    static CACHE: OnceLock<EmbedderCache> = OnceLock::new();
    CACHE.get_or_init(EmbedderCache::default)
}

/// Cheap identity of the model files on disk, used to decide whether a
/// remembered load failure is still valid. Covers the weights (length +
/// mtime: the 2.2 GB file we must not re-read needlessly) plus the
/// tokenizer and config, which `BgeM3Embedder::try_new` parses before the
/// weights, so a fix to either must also trigger a retry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelFingerprint {
    bin_len: u64,
    bin_mtime: SystemTime,
    tok_mtime: SystemTime,
    config_mtime: SystemTime,
}

impl ModelFingerprint {
    /// `None` when any file's metadata is unreadable. The failure is then
    /// not remembered and the next call simply retries.
    fn of(model_dir: &Path) -> Option<Self> {
        let bin = std::fs::metadata(model_dir.join("pytorch_model.bin")).ok()?;
        let tok = std::fs::metadata(model_dir.join("tokenizer.json")).ok()?;
        let config = std::fs::metadata(model_dir.join("config.json")).ok()?;
        Some(Self {
            bin_len: bin.len(),
            bin_mtime: bin.modified().ok()?,
            tok_mtime: tok.modified().ok()?,
            config_mtime: config.modified().ok()?,
        })
    }
}

enum Slot {
    /// A successfully loaded real model and when it was last handed out
    /// (on load and on every cache hit), for idle eviction.
    Loaded {
        embedder: Arc<dyn Embedder>,
        last_used: Instant,
    },
    /// All files were present but loading failed. Not retried while the
    /// files still match this fingerprint.
    Failed(ModelFingerprint),
}

/// Map from model directory to its [`Slot`]. One process-wide instance
/// backs [`cached_for_vault`]; tests build their own instances so they
/// never race with other tests (e.g. unmount) that clear the global one.
#[derive(Default)]
struct EmbedderCache {
    slots: Mutex<HashMap<PathBuf, Slot>>,
}

impl EmbedderCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Slot>> {
        // A poisoned lock only means a previous load panicked; the map is
        // still consistent (slots are written only after a load returns).
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn get_or_load(
        &self,
        model_dir: &Path,
        load: impl Fn(&Path) -> Option<Arc<dyn Embedder>>,
    ) -> Arc<dyn Embedder> {
        let mut slots = self.lock();
        let fingerprint = if has_full_model(model_dir) {
            ModelFingerprint::of(model_dir)
        } else {
            None
        };
        match slots.get_mut(model_dir) {
            Some(Slot::Loaded {
                embedder,
                last_used,
            }) => {
                *last_used = Instant::now();
                return embedder.clone();
            }
            Some(Slot::Failed(failed)) if fingerprint.as_ref() == Some(failed) => {
                return hashed_fallback();
            }
            _ => {}
        }
        match load(model_dir) {
            Some(embedder) => {
                slots.insert(
                    model_dir.to_path_buf(),
                    Slot::Loaded {
                        embedder: embedder.clone(),
                        last_used: Instant::now(),
                    },
                );
                embedder
            }
            None => {
                // Remember only a failure with every file present (and a
                // readable fingerprint). A missing model stays uncached so
                // a later download is picked up.
                match fingerprint {
                    Some(fp) => {
                        slots.insert(model_dir.to_path_buf(), Slot::Failed(fp));
                    }
                    None => {
                        slots.remove(model_dir);
                    }
                }
                hashed_fallback()
            }
        }
    }

    fn clear(&self) {
        self.lock().clear();
    }

    /// Removes `Loaded` slots idle for longer than `max_idle` and returns
    /// each evicted slot's idle time. Uses `try_lock`: a load in progress
    /// holds the lock for seconds, and eviction must not queue behind it —
    /// the round is skipped instead.
    fn evict_idle(&self, max_idle: Duration) -> Vec<Duration> {
        let mut slots = match self.slots.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Vec::new(),
        };
        let now = Instant::now();
        let mut evicted = Vec::new();
        slots.retain(|_, slot| match slot {
            Slot::Loaded { last_used, .. } => {
                let idle = now.saturating_duration_since(*last_used);
                if idle > max_idle {
                    evicted.push(idle);
                    false
                } else {
                    true
                }
            }
            Slot::Failed(_) => true,
        });
        evicted
    }

    /// Test-only: pretend the slot for `model_dir` was last used `ago`
    /// earlier, so idle eviction can be tested without sleeping.
    #[cfg(test)]
    fn backdate(&self, model_dir: &Path, ago: Duration) {
        if let Some(Slot::Loaded { last_used, .. }) = self.lock().get_mut(model_dir) {
            *last_used = Instant::now()
                .checked_sub(ago)
                .expect("backdate fits in the monotonic clock");
        }
    }

    /// Test-only peek: `Some("loaded")`, `Some("failed")`, or `None` when
    /// the directory has no slot.
    #[cfg(test)]
    fn slot_kind(&self, model_dir: &Path) -> Option<&'static str> {
        self.lock().get(model_dir).map(|slot| match slot {
            Slot::Loaded { .. } => "loaded",
            Slot::Failed(_) => "failed",
        })
    }
}

fn bge_m3_dir(vault: &Path) -> PathBuf {
    crate::vault::layout::models_dir(vault).join("bge-m3")
}

/// Load the real bge-m3 model from `model_dir`, or `None` when the files
/// are incomplete or loading fails (logged).
fn try_load_bge_m3(model_dir: &Path) -> Option<Arc<dyn Embedder>> {
    if !has_full_model(model_dir) {
        return None;
    }
    match bge_m3::BgeM3Embedder::try_new(model_dir) {
        Ok(e) => Some(Arc::new(e)),
        Err(err) => {
            tracing::warn!(?err, "bge-m3 init failed, falling back to hashed embedder");
            None
        }
    }
}

fn hashed_fallback() -> Arc<dyn Embedder> {
    Arc::new(hashed::HashedEmbedder::new())
}

/// True when the complete bge-m3 model is on disk for `vault`, i.e. the
/// index holds (or will hold after the next re-index) real semantic
/// vectors rather than the hashed fallback's. Checks file presence only —
/// never loads the 2.2 GB weights.
pub fn model_available(vault: &Path) -> bool {
    has_full_model(&bge_m3_dir(vault))
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

/// Running sum of little-endian f32 vector blobs whose result is the
/// L2-normalised mean — the "page vector" (mean of a page's chunk
/// vectors) used by the duplicate lint and stored in `page_vectors`.
/// The dimension is set by the first usable blob; blobs that are empty,
/// not a whole number of f32s, or of another dimension are skipped.
#[derive(Debug, Default, Clone)]
pub struct MeanVector {
    sum: Option<Vec<f32>>,
}

impl MeanVector {
    pub fn add_blob(&mut self, blob: &[u8]) {
        if blob.is_empty() || blob.len() % 4 != 0 {
            return;
        }
        let dim = blob.len() / 4;
        let sum = self.sum.get_or_insert_with(|| vec![0.0; dim]);
        if sum.len() != dim {
            return;
        }
        for (s, bytes) in sum.iter_mut().zip(blob.chunks_exact(4)) {
            *s += f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
    }

    /// The normalised mean, or `None` when nothing usable was added or
    /// the sum is the zero vector. (Normalising the sum equals
    /// normalising the mean.)
    pub fn finish(self) -> Option<Vec<f32>> {
        let mut v = self.sum?;
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm <= f32::EPSILON {
            return None;
        }
        for x in &mut v {
            *x /= norm;
        }
        Some(v)
    }
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

    /// Minimal embedder so cache tests never need the real model.
    struct FakeEmbedder;
    impl Embedder for FakeEmbedder {
        fn dim(&self) -> usize {
            EMBED_DIM
        }
        fn embed(&self, _text: &str) -> Vec<f32> {
            vec![0.0; EMBED_DIM]
        }
        fn name(&self) -> &'static str {
            "fake"
        }
    }

    fn fake_loader(_dir: &Path) -> Option<Arc<dyn Embedder>> {
        Some(Arc::new(FakeEmbedder))
    }

    /// Writes the four model files as tiny stubs with an invalid
    /// `config.json`, so `BgeM3Embedder::try_new` fails while parsing the
    /// config, before it would ever read weights.
    fn write_broken_model(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("pytorch_model.bin"), b"stub").unwrap();
        std::fs::write(dir.join("tokenizer.json"), b"{}").unwrap();
        std::fs::write(dir.join("config.json"), b"not json").unwrap();
        std::fs::write(dir.join("sentencepiece.bpe.model"), b"stub").unwrap();
    }

    #[test]
    fn cached_for_vault_returns_a_full_dimension_embedder_when_no_model_is_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(cached_for_vault(tmp.path()).dim(), EMBED_DIM);
    }

    #[test]
    fn the_cache_does_not_remember_the_fallback_when_no_model_is_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = EmbedderCache::default();
        let _ = cache.get_or_load(tmp.path(), try_load_bge_m3);
        assert_eq!(cache.slot_kind(tmp.path()), None);
    }

    #[test]
    fn the_cache_returns_the_same_instance_for_a_loaded_model_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = EmbedderCache::default();
        let first = cache.get_or_load(tmp.path(), fake_loader);
        let second = cache.get_or_load(tmp.path(), fake_loader);
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn clearing_the_cache_removes_a_loaded_embedder() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = EmbedderCache::default();
        let _ = cache.get_or_load(tmp.path(), fake_loader);
        cache.clear();
        assert_eq!(cache.slot_kind(tmp.path()), None);
    }

    #[test]
    fn invalidate_embedder_cache_empties_the_process_wide_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Unique tempdir key; a concurrent clear from another test (e.g.
        // unmount) can only make the assertion "more true", never flaky.
        let _ = global_cache().get_or_load(tmp.path(), fake_loader);
        invalidate_embedder_cache();
        assert_eq!(global_cache().slot_kind(tmp.path()), None);
    }

    #[test]
    fn a_failed_real_load_falls_back_to_the_hashed_embedder() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_broken_model(tmp.path());
        let cache = EmbedderCache::default();
        let _ = cache.get_or_load(tmp.path(), try_load_bge_m3);
        let second = cache.get_or_load(tmp.path(), try_load_bge_m3);
        assert_eq!(second.name(), hashed::HashedEmbedder::new().name());
    }

    #[test]
    fn a_failed_real_load_is_not_cached_as_loaded() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_broken_model(tmp.path());
        let cache = EmbedderCache::default();
        let _ = cache.get_or_load(tmp.path(), try_load_bge_m3);
        assert_eq!(cache.slot_kind(tmp.path()), Some("failed"));
    }

    #[test]
    fn a_failed_real_model_load_is_remembered_and_not_retried_while_the_files_are_unchanged() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_broken_model(tmp.path());
        let cache = EmbedderCache::default();
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let counting = |dir: &Path| {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            try_load_bge_m3(dir)
        };
        let _ = cache.get_or_load(tmp.path(), counting);
        let _ = cache.get_or_load(tmp.path(), counting);
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_real_model_load_is_retried_once_the_weights_file_changes() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_broken_model(tmp.path());
        let cache = EmbedderCache::default();
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let counting = |dir: &Path| {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            try_load_bge_m3(dir)
        };
        let _ = cache.get_or_load(tmp.path(), counting);
        // A different length changes the fingerprint regardless of the
        // filesystem's mtime resolution.
        std::fs::write(tmp.path().join("pytorch_model.bin"), b"longer stub").unwrap();
        let _ = cache.get_or_load(tmp.path(), counting);
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn an_embedder_idle_longer_than_the_ttl_is_evicted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = EmbedderCache::default();
        let _ = cache.get_or_load(tmp.path(), fake_loader);
        cache.backdate(tmp.path(), Duration::from_secs(120));
        let _ = cache.evict_idle(Duration::from_secs(60));
        assert_eq!(cache.slot_kind(tmp.path()), None);
    }

    #[test]
    fn an_embedder_used_within_the_ttl_is_kept() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = EmbedderCache::default();
        let _ = cache.get_or_load(tmp.path(), fake_loader);
        cache.backdate(tmp.path(), Duration::from_secs(30));
        let _ = cache.evict_idle(Duration::from_secs(60));
        assert_eq!(cache.slot_kind(tmp.path()), Some("loaded"));
    }

    #[test]
    fn eviction_leaves_failed_load_slots_in_place() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_broken_model(tmp.path());
        let cache = EmbedderCache::default();
        let _ = cache.get_or_load(tmp.path(), try_load_bge_m3);
        let _ = cache.evict_idle(Duration::ZERO);
        assert_eq!(cache.slot_kind(tmp.path()), Some("failed"));
    }

    #[test]
    fn the_mean_vector_of_two_orthogonal_unit_blobs_is_the_normalised_diagonal() {
        let mut mean = MeanVector::default();
        mean.add_blob(&vec_to_bytes(&[1.0, 0.0]));
        mean.add_blob(&vec_to_bytes(&[0.0, 1.0]));
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert_eq!(mean.finish(), Some(vec![h, h]));
    }

    #[test]
    fn the_mean_vector_skips_a_blob_of_another_dimension() {
        let mut mean = MeanVector::default();
        mean.add_blob(&vec_to_bytes(&[2.0, 0.0]));
        mean.add_blob(&vec_to_bytes(&[0.0, 1.0, 0.0]));
        assert_eq!(mean.finish(), Some(vec![1.0, 0.0]));
    }

    #[test]
    fn the_mean_vector_of_nothing_is_none() {
        assert_eq!(MeanVector::default().finish(), None);
    }
}
