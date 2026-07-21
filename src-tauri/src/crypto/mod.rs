//! Content encryption for the S11 vault-sync feature.
//!
//! This module is the pure cryptographic core: key derivation,
//! deterministic authenticated encryption of page contents, opaque
//! filename tokens, and a canary for passphrase validation. It knows
//! nothing about git, the keychain, or the filesystem — those wire it
//! up in later phases. Everything here is a pure function of its
//! inputs, so it is exhaustively unit-testable in isolation.
//!
//! ## Why deterministic encryption
//!
//! The content encryption is consumed by a git clean/smudge filter:
//! the working tree stays plaintext, the committed blobs are
//! ciphertext. Git decides "did this file change?" by comparing the
//! *filter output* (the ciphertext). If encrypting the same plaintext
//! twice produced different ciphertext (random nonce), git would see
//! every file as modified on every checkout — endless phantom diffs.
//! So the nonce is derived deterministically from the plaintext via
//! HMAC: identical plaintext ⇒ identical nonce ⇒ identical ciphertext.
//!
//! ## Why XChaCha20-Poly1305
//!
//! A deterministic nonce means the nonce space must be large enough
//! that two *different* plaintexts practically never collide on the
//! same nonce under the same key. AES-GCM's 96-bit nonce makes that a
//! (remote, but catastrophic) risk. XChaCha20-Poly1305's 192-bit
//! nonce is designed for exactly this — random/derived nonces — and is
//! pure-Rust with no OpenSSL dependency.

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Magic + version prefix on every encrypted blob. Lets `decrypt`
/// reject input that was never encrypted (e.g. a plaintext file that
/// slipped past the filter) with a clear error instead of a confusing
/// AEAD failure, and gives us a version byte for future format changes.
const MAGIC: &[u8; 4] = b"BRNC";
const FORMAT_VERSION: u8 = 1;
const NONCE_LEN: usize = 24; // XChaCha20-Poly1305 nonce width.

/// A known plaintext encrypted into the vault (see [`make_canary`]) so
/// a freshly-cloned machine can validate the entered passphrase before
/// mounting: decrypt the stored canary and check it matches. Wrong key
/// ⇒ AEAD auth failure ⇒ clear "wrong passphrase", never a garbage
/// working tree.
const CANARY_PLAINTEXT: &[u8] = b"brain-vault-canary-v1";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CryptoError {
    #[error("input is not a BRAIN-encrypted blob (bad magic)")]
    NotEncrypted,
    #[error("unsupported encryption format version {0}")]
    UnsupportedVersion(u8),
    #[error("ciphertext too short")]
    Truncated,
    #[error("decryption failed (wrong key or tampered data)")]
    DecryptionFailed,
}

/// The user's single high-entropy master key (256-bit). Stored in the
/// OS keychain per machine and, as the master copy, in the user's
/// password manager. Everything else is HKDF-derived from it, so the
/// user manages exactly one secret.
#[derive(Clone)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    /// Generate a fresh random master key. Shown to the user once as a
    /// recovery string to save in their password manager.
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parse a 64-char hex recovery string (what the user pastes on a
    /// new machine).
    pub fn from_hex(s: &str) -> Option<Self> {
        let raw = hex::decode(s.trim()).ok()?;
        let arr: [u8; 32] = raw.try_into().ok()?;
        Some(Self(arr))
    }

    /// Hex recovery string to show/store. 64 lowercase hex chars.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Derive the per-purpose subkeys. Distinct `info` labels domain-
    /// separate the three keys so, e.g., the filename HMAC key can
    /// never coincide with the content key.
    pub fn derive(&self) -> DerivedKeys {
        let hk = Hkdf::<Sha256>::new(None, &self.0);
        let mut content = [0u8; 32];
        let mut filename = [0u8; 32];
        let mut nonce = [0u8; 32];
        hk.expand(b"brain-content-key-v1", &mut content)
            .expect("32 is a valid HKDF output length");
        hk.expand(b"brain-filename-hmac-v1", &mut filename)
            .expect("32 is a valid HKDF output length");
        hk.expand(b"brain-content-nonce-v1", &mut nonce)
            .expect("32 is a valid HKDF output length");
        DerivedKeys {
            content,
            filename,
            nonce,
        }
    }
}

/// The three purpose-specific keys derived from a [`MasterKey`].
pub struct DerivedKeys {
    content: [u8; 32],
    filename: [u8; 32],
    nonce: [u8; 32],
}

impl DerivedKeys {
    /// Deterministically encrypt `plaintext`. Output layout:
    /// `MAGIC(4) || version(1) || nonce(24) || ciphertext+tag`.
    /// The nonce is `HMAC(nonce_key, plaintext)[..24]`, so identical
    /// plaintext yields byte-identical output (no phantom git diffs).
    pub fn encrypt(&self, plaintext: &[u8]) -> Vec<u8> {
        let nonce_bytes = self.deterministic_nonce(plaintext);
        let cipher = XChaCha20Poly1305::new((&self.content).into());
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce_bytes), plaintext)
            .expect("XChaCha20-Poly1305 encryption is infallible for valid inputs");
        let mut out = Vec::with_capacity(4 + 1 + NONCE_LEN + ciphertext.len());
        out.extend_from_slice(MAGIC);
        out.push(FORMAT_VERSION);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        out
    }

    /// Decrypt a blob produced by [`encrypt`]. Rejects non-encrypted
    /// input (bad magic), unknown versions, truncated blobs, and wrong
    /// key / tampering (AEAD auth failure) — each with a distinct error.
    pub fn decrypt(&self, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
        // Magic first: anything that doesn't start with our 4-byte
        // magic was never encrypted by us (a stray plaintext file, or
        // empty input).
        if blob.len() < 4 || blob[..4] != *MAGIC {
            return Err(CryptoError::NotEncrypted);
        }
        // Magic present but the header (magic+version+nonce) is
        // incomplete → a corrupt/truncated blob, distinct from
        // never-encrypted.
        if blob.len() < 4 + 1 + NONCE_LEN {
            return Err(CryptoError::Truncated);
        }
        let version = blob[4];
        if version != FORMAT_VERSION {
            return Err(CryptoError::UnsupportedVersion(version));
        }
        let nonce = &blob[5..5 + NONCE_LEN];
        let ciphertext = &blob[5 + NONCE_LEN..];
        let cipher = XChaCha20Poly1305::new((&self.content).into());
        cipher
            .decrypt(XNonce::from_slice(nonce), ciphertext)
            .map_err(|_| CryptoError::DecryptionFailed)
    }

    /// Opaque filename token for a page id: `HMAC(filename_key, id)` as
    /// 64 lowercase hex chars. Deterministic (same id → same token on
    /// every machine, so a page created independently on two machines
    /// collides on one path and surfaces as a git conflict rather than
    /// a silent duplicate) and not reversible without the key.
    pub fn filename_token(&self, id: &str) -> String {
        // Fully-qualified `Mac::new_from_slice` — `KeyInit` (from the
        // AEAD) is also in scope and also defines `new_from_slice`.
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.filename)
            .expect("HMAC accepts any key length");
        mac.update(id.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn deterministic_nonce(&self, plaintext: &[u8]) -> [u8; NONCE_LEN] {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.nonce)
            .expect("HMAC accepts any key length");
        mac.update(plaintext);
        let full = mac.finalize().into_bytes(); // 32 bytes
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&full[..NONCE_LEN]);
        nonce
    }
}

/// Encrypt the canary constant — store the result in the vault so a
/// new machine can validate a candidate key against it.
pub fn make_canary(keys: &DerivedKeys) -> Vec<u8> {
    keys.encrypt(CANARY_PLAINTEXT)
}

/// True iff `keys` decrypts `stored_canary` back to the known canary
/// plaintext — i.e. the passphrase/key is correct. Any failure
/// (wrong key, tampered, not-encrypted) is `false`, never a panic.
pub fn check_canary(keys: &DerivedKeys, stored_canary: &[u8]) -> bool {
    matches!(keys.decrypt(stored_canary), Ok(pt) if pt == CANARY_PLAINTEXT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> DerivedKeys {
        MasterKey::from_bytes([7u8; 32]).derive()
    }

    #[test]
    fn encrypt_then_decrypt_round_trips() {
        let k = keys();
        let pt = b"# Alice\n\nA wiki page with [[entities/bob]].\n";
        let blob = k.encrypt(pt);
        assert_eq!(k.decrypt(&blob).unwrap(), pt);
    }

    #[test]
    fn encryption_is_deterministic_for_stable_git_diffs() {
        // The whole reason for the deterministic-nonce design: the same
        // plaintext must produce byte-identical ciphertext, or git sees
        // phantom modifications on every checkout.
        let k = keys();
        let pt = b"stable content";
        assert_eq!(k.encrypt(pt), k.encrypt(pt));
    }

    #[test]
    fn different_plaintext_yields_different_ciphertext() {
        let k = keys();
        assert_ne!(k.encrypt(b"one"), k.encrypt(b"two"));
    }

    #[test]
    fn decrypt_with_wrong_key_fails_cleanly() {
        let a = MasterKey::from_bytes([1u8; 32]).derive();
        let b = MasterKey::from_bytes([2u8; 32]).derive();
        let blob = a.encrypt(b"secret");
        assert_eq!(b.decrypt(&blob), Err(CryptoError::DecryptionFailed));
    }

    #[test]
    fn decrypt_rejects_non_encrypted_input() {
        let k = keys();
        assert_eq!(
            k.decrypt(b"plain markdown, never encrypted"),
            Err(CryptoError::NotEncrypted)
        );
        assert_eq!(k.decrypt(b""), Err(CryptoError::NotEncrypted));
    }

    #[test]
    fn decrypt_reports_truncated_when_magic_present_but_header_incomplete() {
        let k = keys();
        // Correct magic + version but the nonce is cut short.
        let mut blob = Vec::new();
        blob.extend_from_slice(MAGIC);
        blob.push(FORMAT_VERSION);
        blob.extend_from_slice(&[0u8; 5]); // < NONCE_LEN
        assert_eq!(k.decrypt(&blob), Err(CryptoError::Truncated));
    }

    #[test]
    fn decrypt_rejects_an_unknown_format_version() {
        let k = keys();
        let mut blob = Vec::new();
        blob.extend_from_slice(MAGIC);
        blob.push(FORMAT_VERSION + 1);
        blob.extend_from_slice(&[0u8; NONCE_LEN]);
        blob.extend_from_slice(&[0u8; 16]);
        assert_eq!(
            k.decrypt(&blob),
            Err(CryptoError::UnsupportedVersion(FORMAT_VERSION + 1))
        );
    }

    #[test]
    fn decrypt_rejects_a_tampered_blob() {
        let k = keys();
        let mut blob = k.encrypt(b"authentic");
        let last = blob.len() - 1;
        blob[last] ^= 0xff; // flip a ciphertext/tag bit
        assert_eq!(k.decrypt(&blob), Err(CryptoError::DecryptionFailed));
    }

    #[test]
    fn canary_validates_the_correct_key_and_rejects_others() {
        let right = MasterKey::from_bytes([9u8; 32]).derive();
        let wrong = MasterKey::from_bytes([8u8; 32]).derive();
        let canary = make_canary(&right);
        assert!(check_canary(&right, &canary));
        assert!(!check_canary(&wrong, &canary));
        assert!(!check_canary(&right, b"garbage"));
    }

    #[test]
    fn filename_token_is_deterministic_keyed_and_not_the_id() {
        let k = keys();
        let t = k.filename_token("entities/michael-simon");
        // Deterministic.
        assert_eq!(t, k.filename_token("entities/michael-simon"));
        // 64 hex chars, and the person's name does not appear.
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!t.contains("michael"));
        // Different ids → different tokens.
        assert_ne!(t, k.filename_token("entities/michael-schmidt"));
    }

    #[test]
    fn filename_token_differs_under_a_different_key() {
        // A plain hash of a low-entropy name would be brute-forceable;
        // the keyed HMAC means the token is unpredictable without the
        // key — two vaults with different keys produce different tokens
        // for the same id.
        let a = MasterKey::from_bytes([1u8; 32]).derive();
        let b = MasterKey::from_bytes([2u8; 32]).derive();
        assert_ne!(
            a.filename_token("entities/alice"),
            b.filename_token("entities/alice")
        );
    }

    #[test]
    fn master_key_hex_round_trips() {
        let k = MasterKey::generate();
        let hex = k.to_hex();
        assert_eq!(hex.len(), 64);
        let parsed = MasterKey::from_hex(&hex).unwrap();
        // Derive from both and confirm identical content key (proves the
        // bytes round-tripped).
        assert_eq!(k.derive().content, parsed.derive().content);
    }

    #[test]
    fn from_hex_rejects_malformed_input() {
        assert!(MasterKey::from_hex("not hex").is_none());
        assert!(MasterKey::from_hex("abcd").is_none()); // too short
    }
}
