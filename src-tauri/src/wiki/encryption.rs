//! Enabling content encryption on an existing vault (S11 phase 5).
//!
//! git-crypt-style: the working tree stays plaintext, the committed
//! git blobs become ciphertext. This is inherently content-loss-safe —
//! the working-tree files are never deleted or rewritten in place; we
//! only (re)configure the filter, write the canary, and re-stage so the
//! commit stores ciphertext. Old plaintext commits remain in history
//! until the user chooses to rewrite it.
//!
//! **Why the commit is encrypted in Rust, not via the git filter.**
//! libgit2 (the `git2` crate) does not execute external process filters
//! configured in `filter.<name>.clean` — only the `git` CLI does. Since
//! BRAIN drives all its git operations through libgit2, relying on the
//! filter would silently commit *plaintext*. So the encrypting commit
//! stages ciphertext blobs itself: read each plaintext working-tree
//! file, [`filter_clean`] it, and stage the result with
//! `Index::add_frombuffer`. The `.gitattributes` + `brain-crypt` filter
//! config we still write is the interop safety net for anyone who runs
//! plain `git` against the vault — not BRAIN's own path.
//!
//! Filenames are NOT changed here — opaque HMAC filenames are the
//! separate phase 5b (they require rewiring every id→path call site to
//! the index/key, a larger and independently-riskable change).

use std::path::Path;

use anyhow::{Context, Result};

use crate::crypto::gitfilter;
use crate::crypto::keychain::{self, MasterKeyStore};
use crate::crypto::{filter_clean, DerivedKeys, MasterKey};
use crate::vault::layout::wiki_dir;

/// Whether a vault has content encryption enabled — detected by the
/// presence of the canary file in the wiki repo. (The canary travels
/// with the repo, so this is also true on a fresh clone before the key
/// is entered, which is exactly when we need to know "prompt for the
/// key".)
pub fn is_encrypted(vault: &Path) -> bool {
    wiki_dir(vault).join(gitfilter::CANARY_FILENAME).exists()
}

/// Set up content encryption on `vault`: ensure a master key exists
/// (generate + store if absent), configure the git clean/smudge filter,
/// and write the canary. Returns the master key so the caller can show
/// the user their recovery string.
///
/// The key is stored under the vault's stable `vault_id` (via
/// [`keychain::vault_account`]) so the app and the git filter agree on
/// which credential regardless of mount path.
///
/// Does NOT re-stage existing files — that is [`renormalize_and_commit`].
/// Splitting them keeps this crypto/keychain setup unit-testable in
/// isolation from the git tree work.
pub fn enable_encryption(
    vault: &Path,
    store: &impl MasterKeyStore,
    brain_exe: &Path,
) -> Result<MasterKey> {
    let account = keychain::vault_account(vault).context("resolving the vault keychain id")?;
    let key = match keychain::load_master_key(store, &account)
        .context("reading the vault master key from the keychain")?
    {
        Some(existing) => existing,
        None => {
            let fresh = MasterKey::generate();
            keychain::store_master_key(store, &account, &fresh)
                .context("storing the new vault master key in the keychain")?;
            fresh
        }
    };
    let wiki = wiki_dir(vault);
    gitfilter::configure_repo_filter(&wiki, brain_exe)
        .context("configuring the brain-crypt git filter")?;
    gitfilter::write_canary(&wiki, &key.derive()).context("writing the encryption canary")?;
    Ok(key)
}

/// Re-stage every tracked page as ciphertext and commit, so the vault's
/// committed blobs are encrypted at rest. Returns the new commit sha, or
/// `None` if the encrypted tree already matches HEAD (nothing to do).
///
/// Encrypts in Rust rather than via the git filter (see the module
/// docs): stages the working tree with libgit2 to capture the full path
/// set, then overwrites every `*.md` index entry — matching the
/// `.gitattributes` glob — with a [`filter_clean`] blob. Non-`.md`
/// entries stay as staged: `.gitattributes` is plaintext config and
/// `.brain-canary` is ciphertext we wrote directly. The working tree is
/// never touched, so this cannot lose content.
pub fn renormalize_and_commit(wiki: &Path, keys: &DerivedKeys) -> Result<Option<String>> {
    let repo = crate::wiki::git::init_repo(wiki).context("opening the wiki repo")?;
    let mut index = repo.index().context("opening the git index")?;

    // Stage the whole working tree so the index carries every path (git2
    // handles nested dirs + modes). These *.md entries point at PLAINTEXT
    // blobs — git2 will not run our clean filter — so we overwrite them.
    index
        .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
        .context("staging the working tree")?;

    let md_paths: Vec<Vec<u8>> = index
        .iter()
        .map(|entry| entry.path)
        .filter(|path| path.ends_with(b".md"))
        .collect();

    for path in md_paths {
        let rel = std::str::from_utf8(&path).context("non-UTF-8 path in the git index")?;
        let abs = wiki.join(rel);
        let plaintext = std::fs::read(&abs).with_context(|| format!("reading {}", abs.display()))?;
        // filter_clean's double-encrypt guard makes this idempotent even
        // if a working-tree file were already ciphertext.
        let ciphertext = filter_clean(keys, &plaintext);
        index
            .add_frombuffer(&encrypted_index_entry(path.clone()), &ciphertext)
            .with_context(|| format!("staging encrypted blob for {rel}"))?;
    }

    let sha = crate::wiki::git::commit_index(&repo, &mut index, "encrypt: enable content encryption")
        .context("committing the encrypted tree")?;
    Ok(sha)
}

/// Index entry for a hand-encrypted blob. Only `mode` and `path` matter
/// to `add_frombuffer` — it hashes the buffer and fills in the object id
/// and size. Stat fields stay zero: git2 owns all staging of encrypted
/// content, so working-tree stat is never consulted to decide "modified".
fn encrypted_index_entry(path: Vec<u8>) -> git2::IndexEntry {
    git2::IndexEntry {
        ctime: git2::IndexTime::new(0, 0),
        mtime: git2::IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode: 0o100644,
        uid: 0,
        gid: 0,
        file_size: 0,
        id: git2::Oid::zero(),
        flags: 0,
        flags_extended: 0,
        path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::keychain::KeychainError;
    use crate::crypto::{filter_smudge, looks_encrypted, MasterKey};
    use crate::vault::layout::ensure_skeleton;
    use crate::vault::marker::{write_marker, VaultMarker};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    // In-memory MasterKeyStore for tests (keyring's mock can't persist
    // across Entry instances), keyed by account string like the real one.
    #[derive(Default)]
    struct MemStore(Mutex<HashMap<String, String>>);
    impl MasterKeyStore for MemStore {
        fn set_hex(&self, account: &str, hex: &str) -> Result<(), KeychainError> {
            self.0.lock().unwrap().insert(account.into(), hex.into());
            Ok(())
        }
        fn get_hex(&self, account: &str) -> Result<Option<String>, KeychainError> {
            Ok(self.0.lock().unwrap().get(account).cloned())
        }
        fn delete(&self, account: &str) -> Result<(), KeychainError> {
            self.0.lock().unwrap().remove(account);
            Ok(())
        }
    }

    /// A vault skeleton with a marker (so `vault_account` resolves) and
    /// an initialised wiki repo — the state a real vault is in before
    /// convert.
    fn vault_with_repo() -> TempDirLike {
        let tmp = TempDirLike::new();
        ensure_skeleton(tmp.path()).unwrap();
        write_marker(tmp.path(), &VaultMarker::new("0.0.0-test")).unwrap();
        crate::wiki::git::init_repo(&wiki_dir(tmp.path())).unwrap();
        tmp
    }

    /// THE content-loss validation the user asked for: a spread of
    /// realistic page contents survives clean→smudge (what git stores
    /// then checks out) byte-for-byte. Covers frontmatter, unicode,
    /// a markdown table, a code fence, wiki-links, and an empty body.
    #[test]
    fn every_realistic_page_survives_encrypt_then_decrypt_unchanged() {
        let keys = MasterKey::from_bytes([3u8; 32]).derive();
        let pages: &[&[u8]] = &[
            b"---\nid: entities/pascal-poletto\ntype: entity\ntitle: Pascal Poletto\n---\n\n# Pascal\n\nTelefon +49 201 9 59 75 256, [[entities/dextradata-grc-technologies]].\n",
            b"---\nid: concepts/nl-spec\ntype: concept\n---\n\nUmlaute: \xc3\xa4\xc3\xb6\xc3\xbc\xc3\x9f, Emoji ok.\n\n| A | B |\n|---|---|\n| [[entities/x]] | yes |\n",
            b"---\nid: sources/doc\ntype: source\n---\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n",
            b"---\nid: topics/empty\ntype: topic\n---\n",
        ];
        for original in pages {
            let cleaned = filter_clean(&keys, original);
            assert!(looks_encrypted(&cleaned), "clean output must be ciphertext");
            assert_ne!(&cleaned, original, "ciphertext must differ from plaintext");
            let smudged = filter_smudge(&keys, &cleaned).expect("smudge must succeed");
            assert_eq!(&smudged, original, "content must survive the round-trip exactly");
        }
    }

    #[test]
    fn is_encrypted_reflects_canary_presence() {
        let tmp = TempDirLike::new();
        ensure_skeleton(tmp.path()).unwrap();
        assert!(!is_encrypted(tmp.path()), "fresh vault is not encrypted");
        gitfilter::write_canary(&wiki_dir(tmp.path()), &MasterKey::from_bytes([1u8; 32]).derive())
            .unwrap();
        assert!(is_encrypted(tmp.path()), "canary present => encrypted");
    }

    #[test]
    fn enable_encryption_stores_key_configures_filter_and_writes_canary() {
        let tmp = vault_with_repo();
        let store = MemStore::default();
        let exe = PathBuf::from("/opt/brain/brain");

        let key = enable_encryption(tmp.path(), &store, &exe).unwrap();

        // Key persisted under the vault_id account.
        let account = keychain::vault_account(tmp.path()).unwrap();
        let loaded = keychain::load_master_key(&store, &account).unwrap().unwrap();
        assert_eq!(loaded.to_hex(), key.to_hex());
        // Vault now reports encrypted, and the canary validates the key.
        assert!(is_encrypted(tmp.path()));
        assert!(gitfilter::canary_matches(&wiki_dir(tmp.path()), &key.derive()));
        // Filter configured.
        let repo = git2::Repository::open(wiki_dir(tmp.path())).unwrap();
        assert!(repo
            .config()
            .unwrap()
            .get_bool("filter.brain-crypt.required")
            .unwrap());
    }

    #[test]
    fn enable_encryption_reuses_an_existing_key() {
        let tmp = vault_with_repo();
        let store = MemStore::default();
        let exe = PathBuf::from("/opt/brain/brain");
        let first = enable_encryption(tmp.path(), &store, &exe).unwrap();
        let second = enable_encryption(tmp.path(), &store, &exe).unwrap();
        assert_eq!(
            first.to_hex(),
            second.to_hex(),
            "re-running must reuse the stored key, not mint a new one"
        );
    }

    /// The Bug-1 regression guard: because git2 does not run external
    /// filters, the encrypting commit must stage ciphertext ITSELF.
    /// Proven end-to-end through git2 — commit, then read the committed
    /// blob straight back out and check it is ciphertext that decrypts
    /// to the exact original.
    #[test]
    fn renormalize_commits_ciphertext_that_decrypts_to_the_original() {
        let tmp = vault_with_repo();
        let wiki = wiki_dir(tmp.path());

        let page = b"---\nid: entities/x\ntype: entity\n---\n\n# X\n\nPII +49 201 000, [[entities/y]].\n";
        std::fs::write(wiki.join("entities").join("x.md"), page).unwrap();
        crate::wiki::git::commit_all(&wiki, "plaintext baseline").unwrap();

        let store = MemStore::default();
        let key = enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();
        let keys = key.derive();

        let sha = renormalize_and_commit(&wiki, &keys).unwrap();
        assert!(sha.is_some(), "re-encryption must produce a commit");

        let repo = git2::Repository::open(&wiki).unwrap();
        let head_tree = repo.head().unwrap().peel_to_tree().unwrap();

        // The page blob is ciphertext and decrypts back exactly.
        let md = head_tree.get_path(Path::new("entities/x.md")).unwrap();
        let md_blob = repo.find_blob(md.id()).unwrap();
        assert!(looks_encrypted(md_blob.content()), "committed page must be ciphertext");
        assert_ne!(md_blob.content(), page, "committed page must not be plaintext");
        assert_eq!(
            &filter_smudge(&keys, md_blob.content()).unwrap(),
            page,
            "decrypt must restore the original page exactly"
        );

        // .gitattributes must stay plaintext (it is not a *.md file).
        let ga = head_tree.get_path(Path::new(".gitattributes")).unwrap();
        let ga_blob = repo.find_blob(ga.id()).unwrap();
        assert!(!looks_encrypted(ga_blob.content()), ".gitattributes stays plaintext");
        assert!(std::str::from_utf8(ga_blob.content())
            .unwrap()
            .contains("filter=brain-crypt"));
    }

    #[test]
    fn renormalize_is_noop_when_already_encrypted() {
        let tmp = vault_with_repo();
        let wiki = wiki_dir(tmp.path());
        std::fs::write(wiki.join("entities").join("x.md"), b"---\nid: entities/x\n---\nbody\n")
            .unwrap();
        crate::wiki::git::commit_all(&wiki, "baseline").unwrap();
        let store = MemStore::default();
        let key = enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();
        let keys = key.derive();

        assert!(renormalize_and_commit(&wiki, &keys).unwrap().is_some(), "first encrypts");
        assert!(
            renormalize_and_commit(&wiki, &keys).unwrap().is_none(),
            "re-run with identical content must not create an empty commit \
             (deterministic nonce => identical ciphertext => identical tree)"
        );
    }

    // Minimal tempdir helper (tempfile::TempDir wrapper) so tests read
    // cleanly.
    struct TempDirLike(tempfile::TempDir);
    impl TempDirLike {
        fn new() -> Self {
            Self(tempfile::TempDir::new().unwrap())
        }
        fn path(&self) -> &Path {
            self.0.path()
        }
    }
}
