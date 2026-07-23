//! Content encryption for the wiki repo (S11 phase 5).
//!
//! git-crypt-style: the working tree stays plaintext, the committed git
//! blobs are ciphertext. This is inherently content-loss-safe — working
//! files are never deleted or rewritten in place; commits store an
//! encrypted copy and restores decrypt back to plaintext.
//!
//! **Why encryption happens in Rust, not via the git filter.** libgit2
//! (the `git2` crate) does not execute external process filters
//! configured in `filter.<name>.clean/smudge` — only the `git` CLI does.
//! BRAIN drives all its git operations through libgit2, so relying on the
//! filter would silently commit *plaintext*. Instead [`commit_wiki`]
//! stages ciphertext blobs itself (`filter_clean` + `add_frombuffer`) and
//! [`blob_to_worktree`] decrypts on the way out. The `.gitattributes` +
//! `brain-crypt` filter config we still write is the interop safety net
//! for anyone running plain `git` — not BRAIN's own path.
//!
//! [`commit_wiki`] is the ONE commit entry point every write path uses
//! (watcher auto-commit, restore, convert). It refuses to commit into an
//! encrypted vault whose key is unavailable, so no path can leak
//! plaintext.
//!
//! Filenames are NOT changed here — opaque HMAC filenames are the
//! separate phase 5b.

use std::path::{Path, PathBuf};

use crate::crypto::gitfilter;
use crate::crypto::keychain::{self, KeyringStore, MasterKeyStore};
use crate::crypto::{filter_clean, filter_smudge, DerivedKeys, MasterKey};
use crate::vault::layout::{opaque_relpath_for_id, page_relpath_for_id, wiki_dir};

use super::{WikiError, WikiResult};

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
/// Does NOT re-stage existing files — the caller runs [`commit_wiki`]
/// afterwards to write the encrypted blobs. Splitting them keeps this
/// crypto/keychain setup unit-testable in isolation.
pub fn enable_encryption(
    vault: &Path,
    store: &impl MasterKeyStore,
    brain_exe: &Path,
) -> anyhow::Result<MasterKey> {
    use anyhow::Context;
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

/// The commit entry point for every wiki write path. Stages the working
/// tree and commits, returning the new commit sha (or `None` if nothing
/// changed). If the vault is encrypted, every `*.md` blob is stored as
/// ciphertext; otherwise it behaves exactly like a plaintext commit.
///
/// Uses the real OS keychain. Deterministic encryption means unchanged
/// pages produce identical ciphertext, so re-encrypting the whole tree
/// each commit never churns git history — only genuinely-changed pages
/// yield new blobs. (It does re-encrypt every page per commit; a
/// changed-only fast path is a later optimisation.)
pub fn commit_wiki(wiki: &Path, message: &str) -> WikiResult<Option<String>> {
    commit_wiki_with_store(wiki, message, &KeyringStore)
}

/// [`commit_wiki`] with an injectable key store (unit tests use an
/// in-memory store; production uses the OS keychain).
pub(crate) fn commit_wiki_with_store(
    wiki: &Path,
    message: &str,
    store: &impl MasterKeyStore,
) -> WikiResult<Option<String>> {
    let repo = crate::wiki::git::init_repo(wiki)?;
    let keys = resolve_keys(wiki, store)?;
    // In an encrypted vault, ensure every page sits at its opaque path
    // BEFORE staging, so a page created outside the app's write path (an
    // external editor, a dropped file) can't leak its human name through
    // the committed filename. Idempotent: pages already at their opaque
    // path (e.g. written via the MCP resolver) are skipped.
    if let Some(keys) = &keys {
        rename_pages_to_opaque(wiki, keys)?;
    }
    let mut index = repo.index()?;
    index.add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)?;
    if let Some(keys) = &keys {
        encrypt_md_index_entries(wiki, &mut index, keys)?;
    }
    crate::wiki::git::commit_index(&repo, &mut index, message)
}

/// Turn a committed blob into the bytes that belong in the working tree.
/// In an encrypted vault this decrypts the blob (legacy plaintext blobs
/// pass through unchanged); in a plaintext vault it is the identity. Used
/// by restore, which must never write ciphertext into the working tree.
pub fn blob_to_worktree(wiki: &Path, blob: &[u8]) -> WikiResult<Vec<u8>> {
    blob_to_worktree_with_store(wiki, blob, &KeyringStore)
}

pub(crate) fn blob_to_worktree_with_store(
    wiki: &Path,
    blob: &[u8],
    store: &impl MasterKeyStore,
) -> WikiResult<Vec<u8>> {
    match resolve_keys(wiki, store)? {
        Some(keys) => {
            filter_smudge(&keys, blob).map_err(|e| WikiError::Encryption(e.to_string()))
        }
        None => Ok(blob.to_vec()),
    }
}

/// Inverse of [`blob_to_worktree`]: turn working-tree bytes into the form
/// that belongs in a committed blob. Encrypted vault → ciphertext
/// (`filter_clean`); plaintext vault → identity. Used by the merge to
/// re-stage a conflict-resolved file as the correct blob.
pub(crate) fn worktree_to_blob_with_store(
    wiki: &Path,
    bytes: &[u8],
    store: &impl MasterKeyStore,
) -> WikiResult<Vec<u8>> {
    match resolve_keys(wiki, store)? {
        Some(keys) => Ok(filter_clean(&keys, bytes)),
        None => Ok(bytes.to_vec()),
    }
}

/// Resolve the vault's derived keys: `None` if the vault is plaintext,
/// `Some` if encrypted and the key is available. `Err` if the vault is
/// encrypted but the key is missing/unreadable — callers MUST propagate
/// this and abort rather than fall back to plaintext.
fn resolve_keys_for_vault(
    vault: &Path,
    store: &impl MasterKeyStore,
) -> WikiResult<Option<DerivedKeys>> {
    if !is_encrypted(vault) {
        return Ok(None);
    }
    let account = keychain::vault_account(vault).map_err(|e| WikiError::Encryption(e.to_string()))?;
    let key = keychain::load_master_key(store, &account)
        .map_err(|e| WikiError::Encryption(e.to_string()))?
        .ok_or_else(|| {
            WikiError::Encryption(
                "vault is encrypted but no master key is in the keychain — unlock it first"
                    .to_string(),
            )
        })?;
    Ok(Some(key.derive()))
}

/// [`resolve_keys_for_vault`] given a wiki dir (`<vault>/02_wiki`) rather
/// than the vault root. Git operations know the wiki dir; the vault is
/// its parent.
fn resolve_keys(wiki: &Path, store: &impl MasterKeyStore) -> WikiResult<Option<DerivedKeys>> {
    resolve_keys_for_vault(wiki.parent().unwrap_or(wiki), store)
}

/// Resolve a page id to its repo-relative path, honouring the vault's
/// encryption mode. Plaintext vault: `<id>.md`. Encrypted vault:
/// `<type>/<HMAC-token>.md` — the opaque layout that hides the human
/// slug (the person/customer name) from anyone who obtains a copy of the
/// repo. Deterministic and index-independent: the same id always maps to
/// the same path, so reads, writes and history all agree without a
/// lookup table. THE single id→path entry point for encrypted-aware
/// callers (plain [`page_relpath_for_id`] is only correct for a known
/// plaintext vault).
pub fn page_relpath(vault: &Path, id: &str) -> WikiResult<String> {
    page_relpath_with_store(vault, id, &KeyringStore)
}

pub(crate) fn page_relpath_with_store(
    vault: &Path,
    id: &str,
    store: &impl MasterKeyStore,
) -> WikiResult<String> {
    match resolve_keys_for_vault(vault, store)? {
        Some(keys) => Ok(opaque_relpath_for_id(id, &keys.filename_token(id))),
        None => Ok(page_relpath_for_id(id)),
    }
}

/// Absolute on-disk path of a page id in `vault` — `wiki_dir(vault)`
/// joined with [`page_relpath`].
pub fn page_path(vault: &Path, id: &str) -> WikiResult<std::path::PathBuf> {
    Ok(wiki_dir(vault).join(page_relpath(vault, id)?))
}

/// Overwrite every `*.md` index entry (matching the `.gitattributes`
/// glob) with a [`filter_clean`] blob read from the working tree. Other
/// entries stay untouched: `.gitattributes` is plaintext config,
/// `.brain-canary` is ciphertext we wrote directly. `filter_clean`'s
/// double-encrypt guard keeps this idempotent.
fn encrypt_md_index_entries(
    wiki: &Path,
    index: &mut git2::Index,
    keys: &DerivedKeys,
) -> WikiResult<()> {
    let md_paths: Vec<Vec<u8>> = index
        .iter()
        .map(|entry| entry.path)
        .filter(|path| path.ends_with(b".md"))
        .collect();

    for path in md_paths {
        let rel = std::str::from_utf8(&path)
            .map_err(|_| WikiError::Encryption("non-UTF-8 path in the git index".to_string()))?;
        let abs = wiki.join(rel);
        let plaintext = std::fs::read(&abs)?;
        let ciphertext = filter_clean(keys, &plaintext);
        index.add_frombuffer(&encrypted_index_entry(path.clone()), &ciphertext)?;
    }
    Ok(())
}

/// Re-materialise the working tree from HEAD as plaintext. After a git2
/// operation that checks out committed blobs verbatim (e.g. a hard
/// reset, or a fresh clone), an encrypted vault's working tree holds
/// ciphertext; this decrypts every `*.md` back to plaintext in place.
/// No-op for a plaintext vault. Also the basis for clone materialisation
/// (phase 6b).
pub fn smudge_worktree_from_head(wiki: &Path) -> WikiResult<()> {
    smudge_worktree_from_head_with_store(wiki, &KeyringStore)
}

pub(crate) fn smudge_worktree_from_head_with_store(
    wiki: &Path,
    store: &impl MasterKeyStore,
) -> WikiResult<()> {
    let Some(keys) = resolve_keys(wiki, store)? else {
        return Ok(());
    };
    let repo = crate::wiki::git::init_repo(wiki)?;
    let tree = match repo.head() {
        Ok(head) => head.peel_to_tree()?,
        Err(_) => return Ok(()), // empty repo — nothing to materialise
    };
    // Collect targets first so the walk closure doesn't borrow the repo
    // while we read blobs and write files.
    let mut targets: Vec<(String, git2::Oid)> = Vec::new();
    tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        if entry.kind() == Some(git2::ObjectType::Blob) {
            if let Some(name) = entry.name() {
                if name.ends_with(".md") {
                    targets.push((format!("{root}{name}"), entry.id()));
                }
            }
        }
        git2::TreeWalkResult::Ok
    })?;
    for (rel, oid) in targets {
        let blob = repo.find_blob(oid)?;
        let plaintext =
            filter_smudge(&keys, blob.content()).map_err(|e| WikiError::Encryption(e.to_string()))?;
        let abs = wiki.join(&rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&abs, &plaintext)?;
    }
    Ok(())
}

/// Rename every page file to its opaque path `<type>/<token>.md`, based
/// on the id in the file's frontmatter. This is the convert-time step
/// that stops person/customer names leaking through *file paths* (the
/// content is already hidden by encryption). Idempotent — files already
/// at their opaque path are skipped — and content-loss-safe: it is a
/// plain filesystem rename, and the logical id lives in the frontmatter
/// so the mapping is fully reversible. Files whose frontmatter can't be
/// parsed (a stray non-page `.md`) are left untouched. Returns the
/// number of files renamed. The caller's subsequent [`commit_wiki`]
/// records the renames and encrypts the content.
pub fn rename_pages_to_opaque(wiki: &Path, keys: &DerivedKeys) -> WikiResult<usize> {
    // Collect (old_abs → new_abs) first so the walk never trips over a
    // file it just renamed.
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    collect_page_renames(wiki, wiki, keys, &mut moves)?;
    let mut renamed = 0;
    for (old_abs, new_abs) in moves {
        if old_abs == new_abs {
            continue;
        }
        if let Some(parent) = new_abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&old_abs, &new_abs)?;
        renamed += 1;
    }
    Ok(renamed)
}

fn collect_page_renames(
    wiki: &Path,
    dir: &Path,
    keys: &DerivedKeys,
    out: &mut Vec<(PathBuf, PathBuf)>,
) -> WikiResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            collect_page_renames(wiki, &path, keys, out)?;
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        // Read the plaintext working-tree file; skip anything that isn't a
        // parseable page (no frontmatter id → not ours to rename).
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = crate::wiki::page::parse(&raw) else {
            continue;
        };
        let id = parsed.frontmatter.id;
        let new_rel = opaque_relpath_for_id(&id, &keys.filename_token(&id));
        out.push((path, wiki.join(new_rel)));
    }
    Ok(())
}

/// Disable content encryption on `vault` — the inverse of convert. Renames
/// pages back to their plaintext `<id>.md` names, drops the canary and the
/// brain-crypt filter config, and commits the now-plaintext tree. The
/// master key is left in the keychain (dormant) so re-enabling reuses it
/// and old encrypted history stays readable. Refuses while a network
/// remote is attached — a plaintext push would leak content. No-op if the
/// vault isn't encrypted.
pub fn disable_encryption(vault: &Path) -> WikiResult<()> {
    disable_encryption_with_store(vault, &KeyringStore)
}

pub(crate) fn disable_encryption_with_store(
    vault: &Path,
    store: &impl MasterKeyStore,
) -> WikiResult<()> {
    if !is_encrypted(vault) {
        return Ok(());
    }
    let wiki = wiki_dir(vault);
    if crate::wiki::sync::has_network_remote(&wiki) {
        return Err(WikiError::Encryption(
            "remove the network remote before disabling encryption — otherwise the next \
             push would send plaintext"
                .to_string(),
        ));
    }
    // 1. Opaque → plaintext filenames (from the frontmatter id).
    rename_pages_to_plaintext(&wiki)?;
    // 2. Drop the canary + filter config, so is_encrypted() becomes false
    //    and the commit below stages plaintext blobs.
    let _ = std::fs::remove_file(wiki.join(gitfilter::CANARY_FILENAME));
    gitfilter::remove_repo_filter(&wiki).map_err(|e| WikiError::Encryption(e.to_string()))?;
    // 3. Commit the plaintext tree (is_encrypted now false → plaintext
    //    add_all + commit; stages the renames, the canary deletion and the
    //    .gitattributes change).
    commit_wiki_with_store(&wiki, "decrypt: disable content encryption", store)?;
    Ok(())
}

/// Rename every page from its opaque path back to the plaintext `<id>.md`,
/// based on the frontmatter id. Inverse of [`rename_pages_to_opaque`];
/// content-loss-safe (a filesystem rename) and idempotent. Returns the
/// number of files renamed.
pub fn rename_pages_to_plaintext(wiki: &Path) -> WikiResult<usize> {
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    collect_plaintext_renames(wiki, wiki, &mut moves)?;
    let mut renamed = 0;
    for (old_abs, new_abs) in moves {
        if old_abs == new_abs {
            continue;
        }
        if let Some(parent) = new_abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&old_abs, &new_abs)?;
        renamed += 1;
    }
    Ok(renamed)
}

fn collect_plaintext_renames(
    wiki: &Path,
    dir: &Path,
    out: &mut Vec<(PathBuf, PathBuf)>,
) -> WikiResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            collect_plaintext_renames(wiki, &path, out)?;
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = crate::wiki::page::parse(&raw) else {
            continue;
        };
        out.push((path, wiki.join(page_relpath_for_id(&parsed.frontmatter.id))));
    }
    Ok(())
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
    use crate::crypto::{looks_encrypted, MasterKey};
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

    fn head_blob(wiki: &Path, rel: &str) -> Vec<u8> {
        let repo = git2::Repository::open(wiki).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let entry = tree.get_path(Path::new(rel)).unwrap();
        repo.find_blob(entry.id()).unwrap().content().to_vec()
    }

    /// THE content-loss validation: a spread of realistic page contents
    /// survives clean→smudge (what git stores then checks out)
    /// byte-for-byte. Covers frontmatter, unicode, a markdown table, a
    /// code fence, wiki-links, and an empty body.
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

        let account = keychain::vault_account(tmp.path()).unwrap();
        let loaded = keychain::load_master_key(&store, &account).unwrap().unwrap();
        assert_eq!(loaded.to_hex(), key.to_hex());
        assert!(is_encrypted(tmp.path()));
        assert!(gitfilter::canary_matches(&wiki_dir(tmp.path()), &key.derive()));
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
        assert_eq!(first.to_hex(), second.to_hex(), "must reuse the stored key");
    }

    /// The Bug-1 regression guard: git2 does not run external filters, so
    /// the encrypting commit stages ciphertext ITSELF. Proven end-to-end
    /// through git2 — commit, read the committed blob back, check it is
    /// ciphertext that decrypts to the exact original.
    #[test]
    fn commit_wiki_stores_ciphertext_that_decrypts_to_the_original() {
        let tmp = vault_with_repo();
        let wiki = wiki_dir(tmp.path());
        let page = b"---\nid: entities/x\ntype: entity\n---\n\n# X\n\nPII +49 201 000, [[entities/y]].\n";
        std::fs::write(wiki.join("entities").join("x.md"), page).unwrap();
        crate::wiki::git::commit_all(&wiki, "plaintext baseline").unwrap();

        let store = MemStore::default();
        enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();
        let keys = keychain::load_master_key(&store, &keychain::vault_account(tmp.path()).unwrap())
            .unwrap()
            .unwrap()
            .derive();

        let sha = commit_wiki_with_store(&wiki, "encrypt", &store).unwrap();
        assert!(sha.is_some(), "encryption must produce a commit");

        // The page is now at its opaque path (commit_wiki renames it).
        let blob = head_blob(&wiki, &format!("entities/{}.md", keys.filename_token("entities/x")));
        assert!(looks_encrypted(&blob), "committed page must be ciphertext");
        assert_ne!(blob.as_slice(), page, "committed page must not be plaintext");
        assert_eq!(&filter_smudge(&keys, &blob).unwrap(), page, "decrypt restores original");

        // .gitattributes stays plaintext (not a *.md file).
        let ga = head_blob(&wiki, ".gitattributes");
        assert!(!looks_encrypted(&ga), ".gitattributes stays plaintext");
    }

    #[test]
    fn commit_wiki_gives_a_directly_created_page_an_opaque_name() {
        // A page dropped straight into the working tree with a human name
        // (bypassing the MCP resolver) must still be committed at its
        // opaque path — the name must never reach the committed tree.
        let tmp = vault_with_repo();
        let wiki = wiki_dir(tmp.path());
        let store = MemStore::default();
        let key = enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();
        let keys = key.derive();
        std::fs::write(
            wiki.join("entities").join("michael-simon.md"),
            "---\nid: entities/michael-simon\ntype: entity\ntitle: M\n---\nbody\n",
        )
        .unwrap();

        commit_wiki_with_store(&wiki, "add page", &store).unwrap();

        let repo = git2::Repository::open(&wiki).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        assert!(
            tree.get_path(Path::new("entities/michael-simon.md")).is_err(),
            "the human-named path must not appear in the committed tree"
        );
        let opaque = format!("entities/{}.md", keys.filename_token("entities/michael-simon"));
        let entry = tree.get_path(Path::new(&opaque)).expect("opaque path committed");
        let blob = repo.find_blob(entry.id()).unwrap();
        assert!(looks_encrypted(blob.content()), "committed at the opaque path as ciphertext");
    }

    #[test]
    fn commit_wiki_is_noop_when_nothing_changed_in_encrypted_vault() {
        let tmp = vault_with_repo();
        let wiki = wiki_dir(tmp.path());
        std::fs::write(wiki.join("entities").join("x.md"), b"---\nid: entities/x\n---\nbody\n")
            .unwrap();
        crate::wiki::git::commit_all(&wiki, "baseline").unwrap();
        let store = MemStore::default();
        enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();

        assert!(commit_wiki_with_store(&wiki, "encrypt", &store).unwrap().is_some(), "first encrypts");
        assert!(
            commit_wiki_with_store(&wiki, "again", &store).unwrap().is_none(),
            "re-run with identical content must not create an empty commit \
             (deterministic nonce => identical ciphertext => identical tree)"
        );
    }

    /// Safety invariant: a commit into an encrypted vault whose key is
    /// unavailable FAILS rather than storing plaintext.
    #[test]
    fn commit_wiki_errors_when_encrypted_but_key_missing() {
        let tmp = vault_with_repo();
        let wiki = wiki_dir(tmp.path());
        std::fs::write(wiki.join("entities").join("x.md"), b"---\nid: entities/x\n---\nsecret\n")
            .unwrap();
        crate::wiki::git::commit_all(&wiki, "baseline").unwrap();
        // Set up encryption with one store, then commit with an EMPTY one.
        enable_encryption(tmp.path(), &MemStore::default(), &PathBuf::from("/opt/brain/brain"))
            .unwrap();
        std::fs::write(wiki.join("entities").join("x.md"), b"---\nid: entities/x\n---\nchanged\n")
            .unwrap();
        let empty = MemStore::default();
        assert!(
            matches!(commit_wiki_with_store(&wiki, "should fail", &empty), Err(WikiError::Encryption(_))),
            "must refuse to commit plaintext into an encrypted vault without the key"
        );
    }

    #[test]
    fn commit_wiki_writes_plaintext_when_vault_not_encrypted() {
        let tmp = vault_with_repo();
        let wiki = wiki_dir(tmp.path());
        std::fs::write(wiki.join("entities").join("x.md"), b"---\nid: entities/x\n---\nplain\n")
            .unwrap();
        let store = MemStore::default();
        commit_wiki_with_store(&wiki, "plain commit", &store).unwrap();
        let blob = head_blob(&wiki, "entities/x.md");
        assert!(!looks_encrypted(&blob), "plaintext vault stores plaintext blobs");
    }

    #[test]
    fn page_relpath_is_plaintext_id_when_vault_not_encrypted() {
        let tmp = vault_with_repo();
        let store = MemStore::default();
        assert_eq!(
            page_relpath_with_store(tmp.path(), "entities/alice", &store).unwrap(),
            "entities/alice.md"
        );
    }

    #[test]
    fn page_relpath_is_opaque_token_when_vault_encrypted() {
        let tmp = vault_with_repo();
        let store = MemStore::default();
        let key = enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();
        let token = key.derive().filename_token("entities/michael-simon");
        let rel = page_relpath_with_store(tmp.path(), "entities/michael-simon", &store).unwrap();
        assert_eq!(rel, format!("entities/{token}.md"), "opaque layout is <type>/<token>.md");
        assert!(!rel.contains("michael"), "person name must not leak into the path");
    }

    #[test]
    fn rename_pages_to_opaque_moves_files_and_hides_names_reversibly() {
        let tmp = vault_with_repo();
        let wiki = wiki_dir(tmp.path());
        let page = "---\nid: entities/michael-simon\ntype: entity\ntitle: Michael\n---\n\nbody\n";
        std::fs::write(wiki.join("entities").join("michael-simon.md"), page).unwrap();
        let store = MemStore::default();
        let key = enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();
        let keys = key.derive();

        assert_eq!(rename_pages_to_opaque(&wiki, &keys).unwrap(), 1);
        assert!(
            !wiki.join("entities").join("michael-simon.md").exists(),
            "the human-named file must be gone"
        );
        let token = keys.filename_token("entities/michael-simon");
        let opaque = wiki.join("entities").join(format!("{token}.md"));
        assert!(opaque.exists(), "file must now live at the opaque path");
        // Content untouched, id still recoverable from frontmatter (reversible).
        let raw = std::fs::read_to_string(&opaque).unwrap();
        assert!(raw.contains("id: entities/michael-simon"), "id stays in frontmatter");
        // Idempotent.
        assert_eq!(rename_pages_to_opaque(&wiki, &keys).unwrap(), 0, "second run is a no-op");
    }

    #[test]
    fn disable_encryption_restores_plaintext_names_and_blobs() {
        let tmp = vault_with_repo();
        let wiki = wiki_dir(tmp.path());
        std::fs::write(
            wiki.join("entities").join("alice.md"),
            "---\nid: entities/alice\ntype: entity\ntitle: A\n---\nbody\n",
        )
        .unwrap();
        crate::wiki::git::commit_all(&wiki, "baseline").unwrap();
        let store = MemStore::default();
        enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();
        commit_wiki_with_store(&wiki, "encrypt", &store).unwrap();
        assert!(is_encrypted(tmp.path()));

        disable_encryption_with_store(tmp.path(), &store).unwrap();

        assert!(!is_encrypted(tmp.path()), "canary gone → vault reports plaintext");
        assert!(
            wiki.join("entities").join("alice.md").exists(),
            "plaintext filename restored in the working tree"
        );
        let repo = git2::Repository::open(&wiki).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let entry = tree.get_path(Path::new("entities/alice.md")).unwrap();
        let blob = repo.find_blob(entry.id()).unwrap();
        assert!(!looks_encrypted(blob.content()), "committed blob is plaintext again");
        assert!(std::str::from_utf8(blob.content()).unwrap().contains("id: entities/alice"));
    }

    #[test]
    fn blob_to_worktree_decrypts_ciphertext_in_encrypted_vault() {
        let tmp = vault_with_repo();
        let wiki = wiki_dir(tmp.path());
        let store = MemStore::default();
        let key = enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();
        let keys = key.derive();
        let page = b"---\nid: entities/x\n---\nplaintext body\n";
        let ct = filter_clean(&keys, page);
        assert!(looks_encrypted(&ct));
        let out = blob_to_worktree_with_store(&wiki, &ct, &store).unwrap();
        assert_eq!(out.as_slice(), page, "restore must decrypt the blob to plaintext");
    }

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
