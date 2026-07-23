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
    finish_enable(vault, brain_exe, &key)?;
    Ok(key)
}

/// Enable content encryption using a *specific existing* recovery key
/// rather than generating one. This is how a second machine joins a vault
/// that already lives on a remote: its diverged plaintext vault is
/// encrypted with the SAME master key, so its blobs (and opaque paths)
/// line up with the remote's and a same-key merge (Variant 2) can union
/// the two histories. The provided key REPLACES any key currently cached
/// in the keychain for this vault. Fails before touching the vault if the
/// key isn't 64 hex characters.
pub fn enable_encryption_with_key(
    vault: &Path,
    store: &impl MasterKeyStore,
    brain_exe: &Path,
    recovery_key_hex: &str,
) -> anyhow::Result<MasterKey> {
    use anyhow::Context;
    let key = MasterKey::from_hex(recovery_key_hex.trim())
        .ok_or_else(|| anyhow::anyhow!("recovery key must be 64 hex characters"))?;
    let account = keychain::vault_account(vault).context("resolving the vault keychain id")?;
    keychain::store_master_key(store, &account, &key)
        .context("storing the provided recovery key in the keychain")?;
    finish_enable(vault, brain_exe, &key)?;
    Ok(key)
}

/// Shared tail of both enable paths: wire up the clean/smudge filter and
/// write the canary so the key is provable and future commits encrypt.
fn finish_enable(vault: &Path, brain_exe: &Path, key: &MasterKey) -> anyhow::Result<()> {
    use anyhow::Context;
    let wiki = wiki_dir(vault);
    gitfilter::configure_repo_filter(&wiki, brain_exe)
        .context("configuring the brain-crypt git filter")?;
    gitfilter::write_canary(&wiki, &key.derive()).context("writing the encryption canary")?;
    Ok(())
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
    // Attachments and the customisable meta files travel too: mirror
    // them into the index (encrypted + opaque names when the vault is
    // encrypted, verbatim otherwise).
    let vault = wiki.parent().unwrap_or(wiki);
    stage_raw_mirror(vault, &mut index, keys.as_ref())?;
    stage_meta_mirror(vault, &mut index, keys.as_ref())?;
    crate::wiki::git::commit_index(&repo, &mut index, message)
}

/// Local-only ref that preserves the pre-conversion history when
/// encryption is enabled. Never pushed — sync pushes only the current
/// branch — so the plaintext commits from before the convert can never
/// reach a remote.
pub const PRE_ENCRYPTION_BACKUP_REF: &str = "refs/heads/pre-encryption-backup";

/// The convert commit: stage the tree exactly like [`commit_wiki`]
/// (opaque-rename + encrypt `*.md`), but commit it as a fresh history
/// ROOT and park the old history under [`PRE_ENCRYPTION_BACKUP_REF`].
/// Everything before the convert is plaintext with human filenames; a
/// normal commit would keep it reachable and the first push would
/// publish it. Requires the vault to already be encrypted (canary set).
pub fn commit_encrypted_snapshot_as_new_root(
    wiki: &Path,
    message: &str,
) -> WikiResult<Option<String>> {
    commit_encrypted_snapshot_as_new_root_with_store(wiki, message, &KeyringStore)
}

pub(crate) fn commit_encrypted_snapshot_as_new_root_with_store(
    wiki: &Path,
    message: &str,
    store: &impl MasterKeyStore,
) -> WikiResult<Option<String>> {
    let repo = crate::wiki::git::init_repo(wiki)?;
    let keys = resolve_keys(wiki, store)?.ok_or_else(|| {
        WikiError::Encryption(
            "re-rooting the history requires an encrypted vault — enable encryption first"
                .to_string(),
        )
    })?;
    rename_pages_to_opaque(wiki, &keys)?;
    let mut index = repo.index()?;
    index.add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)?;
    encrypt_md_index_entries(wiki, &mut index, &keys)?;
    // The fresh encrypted root carries attachments + meta from day one.
    let vault = wiki.parent().unwrap_or(wiki);
    stage_raw_mirror(vault, &mut index, Some(&keys))?;
    stage_meta_mirror(vault, &mut index, Some(&keys))?;
    crate::wiki::git::commit_index_as_new_root(&repo, &mut index, message, PRE_ENCRYPTION_BACKUP_REF)
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
        // Mirror entries (raw/, meta/) have no working-tree file to read
        // — they are staged from their sources by stage_raw_mirror /
        // stage_meta_mirror right after this. Without this filter, the
        // convert of a vault whose PLAINTEXT mirror contains `.md` files
        // (e.g. 01_raw/notes/foo.md, meta/AGENTS.md) would try to read
        // `02_wiki/raw/...` from disk and fail.
        .filter(|path| !path.starts_with(b"raw/") && !path.starts_with(b"meta/"))
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
/// reset, a merge, or a fresh clone), an encrypted vault's working tree
/// holds ciphertext; this decrypts every `*.md` back to plaintext in
/// place. It ALSO restores `<vault>/01_raw` from the raw mirror (both
/// modes) and removes the mirror's worktree copies. The `*.md` decrypt is
/// a no-op for a plaintext vault. Also the basis for clone
/// materialisation (phase 6b).
pub fn smudge_worktree_from_head(wiki: &Path) -> WikiResult<()> {
    smudge_worktree_from_head_with_store(wiki, &KeyringStore)
}

pub(crate) fn smudge_worktree_from_head_with_store(
    wiki: &Path,
    store: &impl MasterKeyStore,
) -> WikiResult<()> {
    materialise_raw_from_head_with_store(wiki, store)?;
    materialise_meta_from_head_with_store(wiki, store)?;
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

/// Repo-relative directory that mirrors `<vault>/01_raw` inside the wiki
/// repo, so attachments travel with sync. The mirror lives ONLY in the
/// index/history — it is never materialised into the wiki working tree
/// (the real files live in `01_raw`).
pub const RAW_MIRROR_DIR: &str = "raw";
/// Encrypted manifest inside the mirror mapping filename token → original
/// `01_raw`-relative path. Needed because a PDF can't carry its own path
/// the way a page's frontmatter id does.
pub const RAW_MANIFEST_PATH: &str = "raw/.manifest";
/// Files larger than this are not synced (GitHub rejects blobs ≥ 100 MB);
/// they stay local with a warning.
const MAX_RAW_SYNC_BYTES: u64 = 95 * 1024 * 1024;

/// Mirror `<vault>/01_raw` into the index under [`RAW_MIRROR_DIR`].
/// Encrypted vault: each file is staged at `raw/<HMAC(relpath)>` with
/// encrypted content plus the encrypted manifest — a copy of the repo
/// reveals neither attachment names nor contents. Plaintext vault: clear
/// relative paths, verbatim bytes, no manifest. Stale mirror entries
/// (source file gone from `01_raw`) are removed; deterministic encryption
/// keeps unchanged files from churning history.
pub(crate) fn stage_raw_mirror(
    vault: &Path,
    index: &mut git2::Index,
    keys: Option<&DerivedKeys>,
) -> WikiResult<()> {
    let raw_root = crate::vault::layout::raw_dir(vault);
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    if raw_root.is_dir() {
        collect_raw_files(&raw_root, &raw_root, &mut files)?;
    }
    // Deterministic order → byte-stable manifest → no spurious diffs.
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut desired: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut manifest = serde_json::Map::new();
    for (rel, abs) in &files {
        if std::fs::metadata(abs).map(|m| m.len() > MAX_RAW_SYNC_BYTES).unwrap_or(false) {
            tracing::warn!(path = %abs.display(), "raw file exceeds the sync size limit — kept local only");
            continue;
        }
        let bytes = std::fs::read(abs)?;
        let (mirror_rel, blob) = match keys {
            Some(keys) => {
                let token = keys.filename_token(rel);
                manifest.insert(token.clone(), serde_json::Value::String(rel.clone()));
                (format!("{RAW_MIRROR_DIR}/{token}"), filter_clean(keys, &bytes))
            }
            None => (format!("{RAW_MIRROR_DIR}/{rel}"), bytes),
        };
        // Stat-zero entry (same as encrypted pages): the mirror has no
        // worktree file, so git must never consult stat data for it.
        index.add_frombuffer(&encrypted_index_entry(mirror_rel.clone().into_bytes()), &blob)?;
        desired.insert(mirror_rel);
    }
    if let Some(keys) = keys {
        if !manifest.is_empty() {
            let json = serde_json::Value::Object(manifest).to_string();
            index.add_frombuffer(
                &encrypted_index_entry(RAW_MANIFEST_PATH.as_bytes().to_vec()),
                &filter_clean(keys, json.as_bytes()),
            )?;
            desired.insert(RAW_MANIFEST_PATH.to_string());
        }
    }
    // Reconcile: drop mirror entries whose source file is gone (covers
    // local deletions and the encrypted↔plaintext naming switch).
    let prefix = format!("{RAW_MIRROR_DIR}/");
    let stale: Vec<String> = index
        .iter()
        .filter_map(|e| String::from_utf8(e.path).ok())
        .filter(|p| p.starts_with(&prefix) && !desired.contains(p))
        .collect();
    for p in stale {
        index.remove_path(Path::new(&p))?;
    }
    Ok(())
}

fn collect_raw_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> WikiResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_raw_files(root, &path, out)?;
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        // Forward-slash relpath; skip (rare) non-UTF-8 names rather than
        // lossy-mangling them beyond restoration.
        let mut parts: Vec<&str> = Vec::new();
        let mut utf8 = true;
        for c in rel.components() {
            match c.as_os_str().to_str() {
                Some(s) => parts.push(s),
                None => {
                    utf8 = false;
                    break;
                }
            }
        }
        if !utf8 {
            tracing::warn!(path = %path.display(), "skipping raw file with a non-UTF-8 name");
            continue;
        }
        out.push((parts.join("/"), path));
    }
    Ok(())
}

/// Inverse of [`stage_raw_mirror`]: restore `<vault>/01_raw` from the raw
/// mirror committed at HEAD, and delete any `raw/` copies a forced
/// checkout dumped into the wiki working tree (the mirror is
/// history-only). Additions and updates propagate; deletions do NOT —
/// `01_raw` is append-only by design (see the vault templates), so a file
/// deleted remotely simply stays here.
pub(crate) fn materialise_raw_from_head_with_store(
    wiki: &Path,
    store: &impl MasterKeyStore,
) -> WikiResult<()> {
    // Never keep a worktree copy of the mirror, ciphertext or otherwise.
    let _ = std::fs::remove_dir_all(wiki.join(RAW_MIRROR_DIR));

    let repo = crate::wiki::git::init_repo(wiki)?;
    let tree = match repo.head() {
        Ok(head) => head.peel_to_tree()?,
        Err(_) => return Ok(()), // empty repo
    };
    let raw_tree = match tree.get_path(Path::new(RAW_MIRROR_DIR)) {
        Ok(entry) => match entry.to_object(&repo)?.into_tree() {
            Ok(t) => t,
            Err(_) => return Ok(()),
        },
        Err(_) => return Ok(()), // no mirror committed yet
    };
    let keys = resolve_keys(wiki, store)?;
    let vault = wiki.parent().unwrap_or(wiki);
    let raw_root = crate::vault::layout::raw_dir(vault);

    match keys {
        Some(keys) => {
            // Encrypted mirror: flat tokens + manifest. Without a manifest
            // (or for tokens missing from it) there is nothing to restore.
            let Some(manifest_entry) = raw_tree.get_name(".manifest") else {
                return Ok(());
            };
            let manifest_blob = repo.find_blob(manifest_entry.id())?;
            let manifest_json = filter_smudge(&keys, manifest_blob.content())
                .map_err(|e| WikiError::Encryption(format!("raw manifest: {e}")))?;
            let manifest: serde_json::Map<String, serde_json::Value> =
                serde_json::from_slice(&manifest_json).map_err(|e| {
                    WikiError::Encryption(format!("raw manifest is not valid JSON: {e}"))
                })?;
            for entry in raw_tree.iter() {
                let Some(name) = entry.name() else { continue };
                if name == ".manifest" {
                    continue;
                }
                let Some(rel) = manifest.get(name).and_then(|v| v.as_str()) else {
                    tracing::warn!(token = name, "raw mirror entry missing from the manifest — skipped");
                    continue;
                };
                let Some(dest) = safe_raw_dest(&raw_root, rel) else {
                    tracing::warn!(rel, "raw manifest path escapes 01_raw — skipped");
                    continue;
                };
                let blob = repo.find_blob(entry.id())?;
                let bytes = filter_smudge(&keys, blob.content())
                    .map_err(|e| WikiError::Encryption(format!("raw {rel}: {e}")))?;
                write_if_changed(&dest, &bytes)?;
            }
        }
        None => {
            // Plaintext mirror: nested clear paths, verbatim bytes.
            let mut targets: Vec<(String, git2::Oid)> = Vec::new();
            raw_tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
                if entry.kind() == Some(git2::ObjectType::Blob) {
                    if let Some(name) = entry.name() {
                        targets.push((format!("{root}{name}"), entry.id()));
                    }
                }
                git2::TreeWalkResult::Ok
            })?;
            for (rel, oid) in targets {
                let Some(dest) = safe_raw_dest(&raw_root, &rel) else {
                    tracing::warn!(rel, "raw mirror path escapes 01_raw — skipped");
                    continue;
                };
                let blob = repo.find_blob(oid)?;
                write_if_changed(&dest, blob.content())?;
            }
        }
    }
    Ok(())
}

/// Repo-relative directory mirroring the SYNCED subset of `00_meta`:
/// the agent-instruction files the user may customise. Same rules as the
/// raw mirror — history-only, never in the wiki working tree.
pub const META_MIRROR_DIR: &str = "meta";
/// The fixed set of `00_meta` files that sync. Machine-specific meta
/// files (marker, .mcp.json bearer token, settings-internal, log/index)
/// deliberately stay local.
pub const MIRRORED_META_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// Mirror the synced `00_meta` files into the index under
/// [`META_MIRROR_DIR`]. Encrypted vault: `meta/<HMAC("00_meta/<name>")>`
/// with encrypted content — the set is FIXED, so unlike raw files no
/// manifest is needed (the restore side recomputes the expected tokens).
/// Plaintext vault: `meta/<name>` verbatim. A missing local file drops
/// its mirror entry (reconcile), so template resets sync too.
pub(crate) fn stage_meta_mirror(
    vault: &Path,
    index: &mut git2::Index,
    keys: Option<&DerivedKeys>,
) -> WikiResult<()> {
    let meta_root = crate::vault::layout::meta_dir(vault);
    let mut desired: std::collections::HashSet<String> = std::collections::HashSet::new();
    for name in MIRRORED_META_FILES {
        let abs = meta_root.join(name);
        if !abs.is_file() {
            continue;
        }
        let bytes = std::fs::read(&abs)?;
        let (mirror_rel, blob) = match keys {
            Some(keys) => (
                format!("{META_MIRROR_DIR}/{}", meta_mirror_token(keys, name)),
                filter_clean(keys, &bytes),
            ),
            None => (format!("{META_MIRROR_DIR}/{name}"), bytes),
        };
        index.add_frombuffer(&encrypted_index_entry(mirror_rel.clone().into_bytes()), &blob)?;
        desired.insert(mirror_rel);
    }
    let prefix = format!("{META_MIRROR_DIR}/");
    let stale: Vec<String> = index
        .iter()
        .filter_map(|e| String::from_utf8(e.path).ok())
        .filter(|p| p.starts_with(&prefix) && !desired.contains(p))
        .collect();
    for p in stale {
        index.remove_path(Path::new(&p))?;
    }
    Ok(())
}

/// Opaque token for a mirrored meta file. Domain-prefixed with the meta
/// dir so it can never collide with a raw file named `AGENTS.md`.
fn meta_mirror_token(keys: &DerivedKeys, name: &str) -> String {
    keys.filename_token(&format!("00_meta/{name}"))
}

/// Inverse of [`stage_meta_mirror`]: restore the synced `00_meta` files
/// from HEAD and drop any worktree copies of the mirror. Only writes when
/// bytes differ. A file missing from HEAD is left alone locally.
pub(crate) fn materialise_meta_from_head_with_store(
    wiki: &Path,
    store: &impl MasterKeyStore,
) -> WikiResult<()> {
    let _ = std::fs::remove_dir_all(wiki.join(META_MIRROR_DIR));

    let repo = crate::wiki::git::init_repo(wiki)?;
    let tree = match repo.head() {
        Ok(head) => head.peel_to_tree()?,
        Err(_) => return Ok(()),
    };
    let Ok(meta_entry) = tree.get_path(Path::new(META_MIRROR_DIR)) else {
        return Ok(()); // no meta mirror committed yet
    };
    let Ok(meta_tree) = meta_entry.to_object(&repo)?.into_tree() else {
        return Ok(());
    };
    let keys = resolve_keys(wiki, store)?;
    let vault = wiki.parent().unwrap_or(wiki);
    let meta_root = crate::vault::layout::meta_dir(vault);

    for name in MIRRORED_META_FILES {
        let (lookup, decrypt) = match &keys {
            Some(keys) => (meta_mirror_token(keys, name), true),
            None => ((*name).to_string(), false),
        };
        let Some(entry) = meta_tree.get_name(&lookup) else {
            continue;
        };
        let blob = repo.find_blob(entry.id())?;
        let bytes = if decrypt {
            filter_smudge(keys.as_ref().expect("keys present when decrypting"), blob.content())
                .map_err(|e| WikiError::Encryption(format!("meta {name}: {e}")))?
        } else {
            blob.content().to_vec()
        };
        write_if_changed(&meta_root.join(name), &bytes)?;
    }
    Ok(())
}

/// Resolve a mirror-relative path under `01_raw`, refusing traversal
/// (`..`) and absolute components — the mirror comes from a remote and is
/// not implicitly trusted, AEAD or not.
fn safe_raw_dest(raw_root: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    let ok = rel_path.components().all(|c| matches!(c, std::path::Component::Normal(_)));
    if !ok || rel.is_empty() {
        return None;
    }
    Some(raw_root.join(rel_path))
}

/// Write only when the on-disk bytes differ — keeps repeated
/// materialisations from re-triggering the file watcher.
fn write_if_changed(abs: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Ok(existing) = std::fs::read(abs) {
        if existing == bytes {
            return Ok(());
        }
    }
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(abs, bytes)
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
/// and old encrypted history stays readable. Enforces "no Git without
/// encryption": any configured remote (and its stored PAT) is torn down
/// first, so no later plaintext push can leak content. No-op if the vault
/// isn't encrypted.
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
    // Enforce "no Git without encryption" in reverse: tearing down
    // encryption also tears down the remote (and its stored PAT), so a
    // later push cannot leak plaintext. Done before the plaintext commit.
    if crate::wiki::sync::remote_url(&wiki).is_some() {
        crate::wiki::sync::disconnect(&wiki)?;
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
    fn raw_files_are_mirrored_encrypted_with_opaque_names_and_manifest() {
        // Attachments in 01_raw hold customer documents — a copy of the
        // repo must reveal neither their names nor their contents.
        let tmp = vault_with_repo();
        let store = MemStore::default();
        let key =
            enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();
        let raw = crate::vault::layout::raw_dir(tmp.path());
        std::fs::create_dir_all(raw.join("email/work")).unwrap();
        std::fs::write(raw.join("email/work/kunde-a.eml"), b"vertraulicher Inhalt").unwrap();

        commit_wiki_with_store(&wiki_dir(tmp.path()), "wiki: raw", &store).unwrap().unwrap();

        let repo = git2::Repository::open(wiki_dir(tmp.path())).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let keys = key.derive();
        let token = keys.filename_token("email/work/kunde-a.eml");
        let entry = tree.get_path(Path::new(&format!("raw/{token}"))).unwrap();
        let blob = repo.find_blob(entry.id()).unwrap();
        assert!(looks_encrypted(blob.content()), "raw blob must be ciphertext");
        assert!(
            tree.get_path(Path::new("raw/email/work/kunde-a.eml")).is_err(),
            "the clear attachment path must not appear in the tree"
        );
        // The manifest exists, is encrypted, and maps token → clear path.
        let m = tree.get_path(Path::new("raw/.manifest")).unwrap();
        let m_blob = repo.find_blob(m.id()).unwrap();
        assert!(looks_encrypted(m_blob.content()), "manifest must be ciphertext");
        let json = filter_smudge(&keys, m_blob.content()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v[&token], "email/work/kunde-a.eml");
    }

    #[test]
    fn raw_files_are_mirrored_verbatim_in_a_plaintext_vault() {
        let tmp = vault_with_repo();
        let store = MemStore::default();
        let raw = crate::vault::layout::raw_dir(tmp.path());
        std::fs::create_dir_all(raw.join("docs")).unwrap();
        std::fs::write(raw.join("docs/notiz.txt"), b"klartext").unwrap();

        commit_wiki_with_store(&wiki_dir(tmp.path()), "wiki: raw", &store).unwrap().unwrap();

        let repo = git2::Repository::open(wiki_dir(tmp.path())).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let entry = tree.get_path(Path::new("raw/docs/notiz.txt")).unwrap();
        let blob = repo.find_blob(entry.id()).unwrap();
        assert_eq!(blob.content(), b"klartext", "plaintext vault mirrors verbatim");
    }

    #[test]
    fn deleting_a_raw_file_removes_its_mirror_entry_on_the_next_commit() {
        let tmp = vault_with_repo();
        let store = MemStore::default();
        let key =
            enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();
        let raw = crate::vault::layout::raw_dir(tmp.path());
        std::fs::create_dir_all(&raw).unwrap();
        std::fs::write(raw.join("old.bin"), b"bytes").unwrap();
        let wiki = wiki_dir(tmp.path());
        commit_wiki_with_store(&wiki, "add", &store).unwrap().unwrap();

        std::fs::remove_file(raw.join("old.bin")).unwrap();
        commit_wiki_with_store(&wiki, "del", &store).unwrap().unwrap();

        let repo = git2::Repository::open(&wiki).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let token = key.derive().filename_token("old.bin");
        assert!(
            tree.get_path(Path::new(&format!("raw/{token}"))).is_err(),
            "a locally deleted raw file must leave the mirror"
        );
        assert!(
            tree.get_path(Path::new("raw/.manifest")).is_err(),
            "an empty mirror carries no manifest"
        );
    }

    #[test]
    fn meta_files_are_mirrored_encrypted_without_clear_names() {
        let tmp = vault_with_repo();
        let store = MemStore::default();
        let key =
            enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();
        let meta = crate::vault::layout::meta_dir(tmp.path());
        std::fs::write(meta.join("AGENTS.md"), b"# custom agent rules").unwrap();

        commit_wiki_with_store(&wiki_dir(tmp.path()), "wiki: meta", &store).unwrap().unwrap();

        let repo = git2::Repository::open(wiki_dir(tmp.path())).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        let keys = key.derive();
        let token = keys.filename_token("00_meta/AGENTS.md");
        let entry = tree.get_path(Path::new(&format!("meta/{token}"))).unwrap();
        let blob = repo.find_blob(entry.id()).unwrap();
        assert!(looks_encrypted(blob.content()), "meta blob must be ciphertext");
        assert_eq!(filter_smudge(&keys, blob.content()).unwrap(), b"# custom agent rules");
        assert!(
            tree.get_path(Path::new("meta/AGENTS.md")).is_err(),
            "the clear meta filename must not appear in an encrypted tree"
        );
    }

    #[test]
    fn converting_a_vault_with_plaintext_mirrors_succeeds_and_retokens_them() {
        // Regression: the pre-convert PLAINTEXT history mirrors raw/meta
        // files at clear `.md` paths (e.g. raw/notes/foo.md). The convert
        // loads that index; encrypt_md_index_entries must skip mirror
        // paths (no working-tree file to read) instead of failing, and
        // the re-staged mirrors must come out token-named.
        let tmp = vault_with_repo();
        let store = MemStore::default();
        let raw = crate::vault::layout::raw_dir(tmp.path());
        std::fs::create_dir_all(raw.join("notes")).unwrap();
        std::fs::write(raw.join("notes/plan.md"), b"raw markdown file").unwrap();
        let meta = crate::vault::layout::meta_dir(tmp.path());
        std::fs::write(meta.join("AGENTS.md"), b"rules").unwrap();
        let wiki = wiki_dir(tmp.path());
        // Plaintext era commit — mirrors land at clear .md paths.
        commit_wiki_with_store(&wiki, "plaintext era", &store).unwrap().unwrap();

        let key =
            enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();
        commit_encrypted_snapshot_as_new_root_with_store(&wiki, "encrypt: convert", &store)
            .unwrap()
            .unwrap();

        let repo = git2::Repository::open(&wiki).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        assert!(tree.get_path(Path::new("raw/notes/plan.md")).is_err(), "clear raw path gone");
        assert!(tree.get_path(Path::new("meta/AGENTS.md")).is_err(), "clear meta path gone");
        let keys = key.derive();
        let raw_token = keys.filename_token("notes/plan.md");
        assert!(tree.get_path(Path::new(&format!("raw/{raw_token}"))).is_ok());
        let meta_token = keys.filename_token("00_meta/AGENTS.md");
        assert!(tree.get_path(Path::new(&format!("meta/{meta_token}"))).is_ok());
    }

    #[test]
    fn converting_reroots_history_so_the_plaintext_past_is_not_pushable() {
        // Before the convert the git history holds plaintext blobs at
        // human-named paths. A normal convert commit would keep that
        // history reachable — and the FIRST PUSH WOULD PUBLISH IT. The
        // convert must instead re-root: HEAD becomes a single encrypted
        // commit, and the old history survives only on a local backup ref.
        let tmp = vault_with_repo();
        let store = MemStore::default();
        let wiki = wiki_dir(tmp.path());
        let page = "---\nid: entities/geheim\ntype: entity\ntitle: Geheim\n---\n\nKlartext.\n";
        std::fs::create_dir_all(wiki.join("entities")).unwrap();
        std::fs::write(wiki.join("entities/geheim.md"), page).unwrap();
        commit_wiki_with_store(&wiki, "wiki: plaintext era", &store).unwrap().unwrap();
        assert!(crate::wiki::git::commit_count(&wiki).unwrap() >= 1);

        let key =
            enable_encryption(tmp.path(), &store, &PathBuf::from("/opt/brain/brain")).unwrap();
        commit_encrypted_snapshot_as_new_root_with_store(&wiki, "encrypt: convert", &store)
            .unwrap()
            .unwrap();

        // HEAD is a fresh root: exactly one commit, nothing plaintext
        // reachable from it.
        assert_eq!(crate::wiki::git::commit_count(&wiki).unwrap(), 1);
        let repo = git2::Repository::open(&wiki).unwrap();
        let head_tree = repo.head().unwrap().peel_to_tree().unwrap();
        assert!(
            head_tree.get_path(Path::new("entities/geheim.md")).is_err(),
            "the human-named path must be gone from the pushable history"
        );
        let token = key.derive().filename_token("entities/geheim");
        let entry = head_tree.get_path(Path::new(&format!("entities/{token}.md"))).unwrap();
        let blob = repo.find_blob(entry.id()).unwrap();
        assert!(looks_encrypted(blob.content()), "the committed blob must be ciphertext");

        // The plaintext era still exists, but only on the local backup ref.
        let backup = repo.find_reference(PRE_ENCRYPTION_BACKUP_REF).unwrap();
        let backup_tree = backup.peel_to_tree().unwrap();
        assert!(
            backup_tree.get_path(Path::new("entities/geheim.md")).is_ok(),
            "the backup ref preserves the pre-encryption history"
        );
    }

    #[test]
    fn enable_encryption_with_key_uses_the_provided_key() {
        let tmp = vault_with_repo();
        let store = MemStore::default();
        let exe = PathBuf::from("/opt/brain/brain");
        let provided = MasterKey::from_bytes([42u8; 32]);

        let key = enable_encryption_with_key(tmp.path(), &store, &exe, &provided.to_hex()).unwrap();

        // The vault is encrypted with exactly the key we handed in — this
        // is what lets a second machine join an existing vault and merge.
        assert_eq!(key.to_hex(), provided.to_hex());
        let account = keychain::vault_account(tmp.path()).unwrap();
        let loaded = keychain::load_master_key(&store, &account).unwrap().unwrap();
        assert_eq!(loaded.to_hex(), provided.to_hex());
        assert!(is_encrypted(tmp.path()));
        assert!(gitfilter::canary_matches(&wiki_dir(tmp.path()), &provided.derive()));
    }

    #[test]
    fn enable_encryption_with_key_rejects_a_malformed_key() {
        let tmp = vault_with_repo();
        let store = MemStore::default();
        let exe = PathBuf::from("/opt/brain/brain");
        // `MasterKey` has no `Debug` (secrets don't get logged), so match
        // instead of `unwrap_err`.
        let err = match enable_encryption_with_key(tmp.path(), &store, &exe, "not-a-valid-key") {
            Ok(_) => panic!("a malformed recovery key must be rejected"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("64 hex"), "clear error: {err:#}");
        // Nothing was written — the vault stays plaintext.
        assert!(!is_encrypted(tmp.path()));
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
