//! S11 phase 6 — remote sync over HTTPS: fetch, 3-way merge, push.
//!
//! Transport is git2's `https` feature (WinHTTP on Windows, SecureTransport
//! on macOS, OpenSSL on Linux). Credentials (a GitHub PAT) come from the OS
//! keychain, never the repo. fetch/push move blobs verbatim, so they are
//! encryption-agnostic — the ciphertext travels as-is.
//!
//! **Merge of encrypted content.** A committed blob is ciphertext, so git
//! cannot line-merge two divergent edits. Non-overlapping merges (fast
//! forward, or edits to different pages) resolve at the tree level with no
//! content merge and need no key. A true same-page conflict is resolved
//! by decrypt → 3-way plaintext merge (`git merge-file`) → re-encrypt, so
//! the merge — and any conflict markers — happen on plaintext. After any
//! merge the working tree is re-materialised as plaintext
//! ([`encryption::smudge_worktree_from_head`]).
//!
//! Coupling (S11): a network remote can only be attached to an encrypted
//! vault — an unencrypted push to a hosting service is not selectable.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use git2::{build::CheckoutBuilder, Oid, Repository, Signature};

use crate::crypto::keychain::{self, KeyringStore, MasterKeyStore};
use crate::crypto::{gitfilter, MasterKey};
use crate::vault::layout::wiki_dir;

use super::encryption;
use super::git::{init_repo, COMMITTER_EMAIL, COMMITTER_NAME};
use super::{WikiError, WikiResult};

/// The single sync remote BRAIN manages.
pub const DEFAULT_REMOTE: &str = "origin";

/// Outcome of a merge from the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Local already contains the remote's commits.
    UpToDate,
    /// Local was strictly behind; fast-forwarded to `sha`.
    FastForward(String),
    /// A real merge commit `sha` was created. `conflicted_pages` lists the
    /// repo-relative paths whose plaintext genuinely overlapped and now
    /// carry conflict markers for the user to resolve (empty = clean merge).
    Merged {
        sha: String,
        conflicted_pages: Vec<String>,
    },
}

fn is_network_url(url: &str) -> bool {
    ["https://", "http://", "ssh://", "git://"]
        .iter()
        .any(|p| url.starts_with(p))
        || url.starts_with("git@")
}

/// Attach (or update) the sync remote. Enforces the S11 coupling: a
/// network remote requires the vault to be encrypted first — an
/// unencrypted push to a hosting service is refused.
pub fn set_remote(wiki: &Path, url: &str) -> WikiResult<()> {
    let vault = wiki.parent().unwrap_or(wiki);
    if is_network_url(url) && !encryption::is_encrypted(vault) {
        return Err(WikiError::Encryption(
            "refusing to attach a network remote to an unencrypted vault — \
             enable encryption (convert) first"
                .to_string(),
        ));
    }
    let repo = init_repo(wiki)?;
    if repo.find_remote(DEFAULT_REMOTE).is_ok() {
        repo.remote_set_url(DEFAULT_REMOTE, url)?;
    } else {
        repo.remote(DEFAULT_REMOTE, url)?;
    }
    Ok(())
}

/// Store the git remote credential (PAT) for this vault in the keychain.
/// Trims surrounding whitespace — a PAT pasted from a browser often picks
/// up a trailing newline/space, which would otherwise make every auth
/// attempt fail.
pub fn set_remote_credential(wiki: &Path, pat: &str) -> WikiResult<()> {
    let account = account_for(wiki)?;
    keychain::store_git_pat(&account, pat.trim()).map_err(|e| WikiError::Encryption(e.to_string()))
}

/// The configured sync remote URL, if any.
pub fn remote_url(wiki: &Path) -> Option<String> {
    let repo = init_repo(wiki).ok()?;
    repo.find_remote(DEFAULT_REMOTE)
        .ok()?
        .url()
        .map(str::to_string)
}

/// Whether the configured remote is a network (hosted) remote — used to
/// refuse disabling encryption while a plaintext push could leak content.
pub fn has_network_remote(wiki: &Path) -> bool {
    remote_url(wiki).map(|u| is_network_url(&u)).unwrap_or(false)
}

/// Whether a remote credential (PAT) is stored for this vault.
pub fn has_credential(wiki: &Path) -> bool {
    account_for(wiki)
        .ok()
        .and_then(|a| keychain::load_git_pat(&a).ok().flatten())
        .is_some()
}

fn account_for(wiki: &Path) -> WikiResult<String> {
    let vault = wiki.parent().unwrap_or(wiki);
    keychain::vault_account(vault).map_err(|e| WikiError::Encryption(e.to_string()))
}

/// Build remote callbacks that authenticate with `pat` (if present).
///
/// Sends the credential at most ONCE per connection. If the server
/// rejects it, libgit2 calls back for credentials again; returning the
/// same one spins into git2's opaque "too many authentication replays"
/// error, so the retry fails with a clear message instead. A missing PAT
/// on an authenticating remote also fails fast (rather than falling
/// through to `Cred::default()`, which on HTTPS just triggers the same
/// reject/replay loop).
fn credentials_callbacks<'a>(pat: Option<String>) -> git2::RemoteCallbacks<'a> {
    let mut cb = git2::RemoteCallbacks::new();
    let mut sent = false;
    cb.credentials(move |_url, username_from_url, allowed| {
        if allowed.contains(git2::CredentialType::USER_PASS_PLAINTEXT) {
            let Some(pat) = pat.as_deref() else {
                return Err(git2::Error::from_str(
                    "this remote needs an access token — add one in Settings → Git sync",
                ));
            };
            if sent {
                return Err(git2::Error::from_str(
                    "authentication failed — check the access token and that it has \
                     push access to this repository",
                ));
            }
            sent = true;
            // GitHub authenticates on the PAT (the password); the username
            // is a conventional placeholder when the URL carries none.
            return git2::Cred::userpass_plaintext(username_from_url.unwrap_or("x-access-token"), pat);
        }
        if allowed.contains(git2::CredentialType::DEFAULT) {
            return git2::Cred::default();
        }
        Err(git2::Error::from_str("no supported authentication method for this remote"))
    });
    cb
}

/// Callbacks that pull the PAT from the keychain for `account` (fetch/push).
fn remote_callbacks<'a>(account: Option<String>) -> git2::RemoteCallbacks<'a> {
    let pat = account.and_then(|a| keychain::load_git_pat(&a).ok().flatten());
    credentials_callbacks(pat)
}

/// Clone an encrypted BRAIN wiki repo into `<vault>/02_wiki` and prepare
/// it for use on this machine. Steps: clone → verify the recovery key
/// against the committed canary → store the key (and the PAT) in the
/// keychain → configure the local clean/smudge filter → materialise the
/// plaintext working tree. The vault MUST already have a marker (the
/// caller creates a fresh one so `vault_account` resolves) but no wiki
/// dir yet.
///
/// Returns a clear error — and stores nothing — when the recovery key is
/// wrong, so a bad key never mounts garbage. The caller does the rest of
/// onboarding (skeleton, templates + fresh bearer token, MCP, index).
pub fn clone_and_prepare(
    url: &str,
    pat: Option<&str>,
    vault: &Path,
    recovery_key_hex: &str,
    brain_exe: &Path,
) -> WikiResult<()> {
    let wiki = wiki_dir(vault);

    // Validate the key format up front so a typo fails before the network.
    let key = MasterKey::from_hex(recovery_key_hex.trim())
        .ok_or_else(|| WikiError::Encryption("recovery key must be 64 hex characters".into()))?;

    // Clone (working tree is ciphertext — no filter configured yet).
    let mut fo = git2::FetchOptions::new();
    fo.remote_callbacks(credentials_callbacks(pat.map(str::to_string)));
    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fo);
    builder.remote_create(|repo, _name, url| repo.remote(DEFAULT_REMOTE, url));
    builder
        .clone(url, &wiki)
        .map_err(|e| WikiError::Encryption(format!("clone failed: {e}")))?;

    // Verify the key against the canary BEFORE storing anything.
    let keys = key.derive();
    if !gitfilter::canary_matches(&wiki, &keys) {
        return Err(WikiError::Encryption(
            "recovery key does not match this vault — wrong key".into(),
        ));
    }

    // Persist the key (and PAT, so later syncs work) under this machine's
    // fresh vault_id account.
    let account = keychain::vault_account(vault).map_err(|e| WikiError::Encryption(e.to_string()))?;
    keychain::store_master_key(&KeyringStore, &account, &key)
        .map_err(|e| WikiError::Encryption(e.to_string()))?;
    if let Some(pat) = pat {
        keychain::store_git_pat(&account, pat).map_err(|e| WikiError::Encryption(e.to_string()))?;
    }

    // Configure the local filter (interop for plain git) and decrypt the
    // working tree in place.
    gitfilter::configure_repo_filter(&wiki, brain_exe)
        .map_err(|e| WikiError::Encryption(e.to_string()))?;
    encryption::smudge_worktree_from_head(&wiki)?;
    Ok(())
}

/// Fetch the remote's refs into the remote-tracking namespace.
pub fn fetch(wiki: &Path) -> WikiResult<()> {
    let repo = init_repo(wiki)?;
    let account = account_for(wiki).ok();
    let mut remote = repo.find_remote(DEFAULT_REMOTE)?;
    let mut fo = git2::FetchOptions::new();
    fo.remote_callbacks(remote_callbacks(account));
    remote.fetch(&[] as &[&str], Some(&mut fo), None)?;
    Ok(())
}

/// Merge the fetched remote branch into the current branch. See the module
/// docs for the encrypted-merge strategy. Uses the OS keychain.
pub fn merge_from_remote(wiki: &Path, branch: &str) -> WikiResult<MergeOutcome> {
    merge_from_remote_with_store(wiki, branch, &KeyringStore)
}

pub(crate) fn merge_from_remote_with_store(
    wiki: &Path,
    branch: &str,
    store: &impl MasterKeyStore,
) -> WikiResult<MergeOutcome> {
    let repo = init_repo(wiki)?;
    // No remote-tracking ref yet (e.g. the very first push to an empty
    // remote) → nothing to merge.
    let their_ref = match repo.find_reference(&format!("refs/remotes/{DEFAULT_REMOTE}/{branch}")) {
        Ok(r) => r,
        Err(_) => return Ok(MergeOutcome::UpToDate),
    };
    let their_commit = their_ref.peel_to_commit()?;
    let their_annotated = repo.reference_to_annotated_commit(&their_ref)?;
    let (analysis, _) = repo.merge_analysis(&[&their_annotated])?;

    if analysis.is_up_to_date() {
        return Ok(MergeOutcome::UpToDate);
    }

    let branch_ref = format!("refs/heads/{branch}");
    if analysis.is_fast_forward() {
        // Move the branch ref forward, then materialise the working tree.
        match repo.find_reference(&branch_ref) {
            Ok(mut r) => {
                r.set_target(their_commit.id(), "sync: fast-forward")?;
            }
            Err(_) => {
                repo.reference(&branch_ref, their_commit.id(), true, "sync: fast-forward")?;
            }
        }
        repo.set_head(&branch_ref)?;
        materialise_worktree(&repo, wiki, store)?;
        return Ok(MergeOutcome::FastForward(their_commit.id().to_string()));
    }

    // Divergent — real merge. This also covers "unrelated histories"
    // (two Brains created independently, then pointed at the same repo —
    // S11 Variant 2): merge_commits uses an empty base, so shared-id pages
    // surface as add/add conflicts (resolved below via decrypt→merge→
    // re-encrypt) and unique pages union. A repo encrypted with a DIFFERENT
    // key fails safely — its blobs won't decrypt during conflict handling.
    let our_commit = repo.head()?.peel_to_commit()?;
    let mut index = repo.merge_commits(&our_commit, &their_commit, None)?;
    let mut conflicted_pages = Vec::new();

    if index.has_conflicts() {
        // Snapshot conflicts first (can't mutate the index while iterating).
        struct Conflict {
            path: Vec<u8>,
            base: Option<Oid>,
            ours: Option<Oid>,
            theirs: Option<Oid>,
        }
        let mut conflicts = Vec::new();
        for c in index.conflicts()? {
            let c = c?;
            let path = c
                .our
                .as_ref()
                .or(c.their.as_ref())
                .or(c.ancestor.as_ref())
                .map(|e| e.path.clone())
                .unwrap_or_default();
            conflicts.push(Conflict {
                path,
                base: c.ancestor.map(|e| e.id),
                ours: c.our.map(|e| e.id),
                theirs: c.their.map(|e| e.id),
            });
        }
        for c in conflicts {
            let rel = std::str::from_utf8(&c.path)
                .map_err(|_| WikiError::Encryption("non-UTF-8 conflict path".to_string()))?;
            // Resolve the conflict: drop all three stages for the path
            // (moves them to the REUC), then stage the merged blob.
            index.remove_path(Path::new(rel))?;
            let base_pt = blob_plaintext(&repo, wiki, c.base, store)?;
            let ours_pt = blob_plaintext(&repo, wiki, c.ours, store)?;
            let theirs_pt = blob_plaintext(&repo, wiki, c.theirs, store)?;
            let (merged_pt, had_markers) = three_way_merge(&ours_pt, &base_pt, &theirs_pt)?;
            if had_markers {
                conflicted_pages.push(rel.to_string());
            }
            let blob_bytes = encryption::worktree_to_blob_with_store(wiki, &merged_pt, store)?;
            // The index from `merge_commits` is in-memory and not backed by
            // a repo, so `add_frombuffer` (which would create the blob)
            // fails. Write the blob to the repo's odb first, then add an
            // entry referencing that oid.
            let oid = repo.blob(&blob_bytes)?;
            let mut entry = index_entry(c.path.clone());
            entry.id = oid;
            entry.file_size = blob_bytes.len() as u32;
            index.add(&entry)?;
        }
    }

    let tree_id = index.write_tree_to(&repo)?;
    let tree = repo.find_tree(tree_id)?;
    let sig = Signature::now(COMMITTER_NAME, COMMITTER_EMAIL)?;
    let message = format!("sync: merge {DEFAULT_REMOTE}/{branch}");
    let sha = repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        &message,
        &tree,
        &[&our_commit, &their_commit],
    )?;
    materialise_worktree(&repo, wiki, store)?;
    Ok(MergeOutcome::Merged {
        sha: sha.to_string(),
        conflicted_pages,
    })
}

/// Push the current branch to the remote.
pub fn push(wiki: &Path) -> WikiResult<()> {
    let repo = init_repo(wiki)?;
    let account = account_for(wiki).ok();
    let branch = current_branch(&repo)?;
    let mut remote = repo.find_remote(DEFAULT_REMOTE)?;
    let mut po = git2::PushOptions::new();
    po.remote_callbacks(remote_callbacks(account));
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    remote.push(&[refspec.as_str()], Some(&mut po))?;
    Ok(())
}

/// fetch → merge → push in one shot. Returns the merge outcome so the
/// caller (which owns the DB) can reindex when the working tree changed.
pub fn sync(wiki: &Path) -> WikiResult<MergeOutcome> {
    let branch = current_branch(&init_repo(wiki)?)?;
    fetch(wiki)?;
    let outcome = merge_from_remote(wiki, &branch)?;
    push(wiki)?;
    Ok(outcome)
}

fn current_branch(repo: &Repository) -> WikiResult<String> {
    Ok(repo
        .head()
        .ok()
        .and_then(|h| h.shorthand().map(str::to_string))
        .unwrap_or_else(|| "main".to_string()))
}

/// Checkout HEAD's tree (verbatim = ciphertext for an encrypted vault),
/// then decrypt `*.md` back to plaintext. Correct for both modes: the
/// smudge step is a no-op on a plaintext vault.
fn materialise_worktree(
    repo: &Repository,
    wiki: &Path,
    store: &impl MasterKeyStore,
) -> WikiResult<()> {
    let mut co = CheckoutBuilder::new();
    co.force();
    repo.checkout_head(Some(&mut co))?;
    encryption::smudge_worktree_from_head_with_store(wiki, store)?;
    Ok(())
}

/// Plaintext of a (possibly-encrypted) blob at `oid`, or empty when the
/// stage is absent (an add/add conflict has no ancestor).
fn blob_plaintext(
    repo: &Repository,
    wiki: &Path,
    oid: Option<Oid>,
    store: &impl MasterKeyStore,
) -> WikiResult<Vec<u8>> {
    match oid {
        None => Ok(Vec::new()),
        Some(oid) => {
            let blob = repo.find_blob(oid)?;
            encryption::blob_to_worktree_with_store(wiki, blob.content(), store)
        }
    }
}

/// Index entry for a hand-staged blob (mode + path only; `add_frombuffer`
/// fills id/size).
fn index_entry(path: Vec<u8>) -> git2::IndexEntry {
    git2::IndexEntry {
        ctime: git2::IndexTime::new(0, 0),
        mtime: git2::IndexTime::new(0, 0),
        dev: 0,
        ino: 0,
        mode: 0o100644,
        uid: 0,
        gid: 0,
        file_size: 0,
        id: Oid::zero(),
        flags: 0,
        flags_extended: 0,
        path,
    }
}

/// 3-way plaintext merge via `git merge-file`. Returns the merged bytes
/// and whether conflict markers were produced (true = the user must
/// resolve). `git` is already a hard dependency (the clean/smudge filter
/// interop), so shelling to it here is consistent.
fn three_way_merge(ours: &[u8], base: &[u8], theirs: &[u8]) -> WikiResult<(Vec<u8>, bool)> {
    // Process-global sequence keeps each invocation's temp dir unique even
    // when merges run concurrently (a per-call counter starting at 0 would
    // collide across threads).
    static MERGE_SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = MERGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("brain-merge-{}-{}", std::process::id(), seq));
    std::fs::create_dir_all(&dir)?;
    let ours_p = dir.join("ours");
    let base_p = dir.join("base");
    let theirs_p = dir.join("theirs");
    std::fs::write(&ours_p, ours)?;
    std::fs::write(&base_p, base)?;
    std::fs::write(&theirs_p, theirs)?;

    let output = Command::new("git")
        .arg("merge-file")
        .arg("--stdout")
        .args(["-L", "ours", "-L", "base", "-L", "theirs"])
        .arg(&ours_p)
        .arg(&base_p)
        .arg(&theirs_p)
        .output();
    let _ = std::fs::remove_dir_all(&dir);

    let output = output.map_err(|e| {
        WikiError::Encryption(format!("could not run `git merge-file` for the merge: {e}"))
    })?;
    // Exit code: 0 = clean, >0 = number of conflicts, <0 = error.
    match output.status.code() {
        Some(code) if code >= 0 => Ok((output.stdout, code > 0)),
        _ => Err(WikiError::Encryption(
            "`git merge-file` failed to merge the page".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::{ensure_skeleton, wiki_dir};
    use crate::vault::marker::{write_marker, VaultMarker};
    use tempfile::TempDir;

    #[test]
    fn set_remote_refuses_network_url_on_unencrypted_vault() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_marker(tmp.path(), &VaultMarker::new("0.0.0")).unwrap();
        let wiki = wiki_dir(tmp.path());
        init_repo(&wiki).unwrap();
        let err = set_remote(&wiki, "https://github.com/example/brain.git").unwrap_err();
        assert!(matches!(err, WikiError::Encryption(_)), "network remote must require encryption");
    }

    #[test]
    fn set_remote_allows_local_path_on_unencrypted_vault() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_marker(tmp.path(), &VaultMarker::new("0.0.0")).unwrap();
        let wiki = wiki_dir(tmp.path());
        init_repo(&wiki).unwrap();
        // A local filesystem path is not a network remote — allowed.
        assert!(set_remote(&wiki, "/srv/backups/brain.git").is_ok());
    }

    #[test]
    fn three_way_merge_is_clean_when_sides_touch_different_lines() {
        let base = b"line1\nline2\nline3\n";
        let ours = b"line1 CHANGED\nline2\nline3\n";
        let theirs = b"line1\nline2\nline3 CHANGED\n";
        let (merged, markers) = three_way_merge(ours, base, theirs).unwrap();
        assert!(!markers, "non-overlapping edits merge cleanly");
        let s = String::from_utf8(merged).unwrap();
        assert!(s.contains("line1 CHANGED") && s.contains("line3 CHANGED"), "both edits survive: {s}");
    }

    #[test]
    fn three_way_merge_marks_conflict_on_overlapping_edits() {
        let base = b"shared line\n";
        let ours = b"our version\n";
        let theirs = b"their version\n";
        let (merged, markers) = three_way_merge(ours, base, theirs).unwrap();
        assert!(markers, "overlapping edits must be flagged as a conflict");
        let s = String::from_utf8(merged).unwrap();
        assert!(s.contains("<<<<<<<") && s.contains(">>>>>>>"), "conflict markers present: {s}");
    }

    // In-memory key store so the merge's decrypt/encrypt runs against a
    // controlled key rather than the OS keychain.
    #[derive(Default)]
    struct MemStore(std::sync::Mutex<std::collections::HashMap<String, String>>);
    impl MasterKeyStore for MemStore {
        fn set_hex(&self, a: &str, h: &str) -> Result<(), crate::crypto::keychain::KeychainError> {
            self.0.lock().unwrap().insert(a.into(), h.into());
            Ok(())
        }
        fn get_hex(&self, a: &str) -> Result<Option<String>, crate::crypto::keychain::KeychainError> {
            Ok(self.0.lock().unwrap().get(a).cloned())
        }
        fn delete(&self, a: &str) -> Result<(), crate::crypto::keychain::KeychainError> {
            self.0.lock().unwrap().remove(a);
            Ok(())
        }
    }

    /// Build a standalone encrypted vault keyed with `key`, its pages
    /// committed (as ciphertext at opaque paths). Each call has its own
    /// git history — so two of them are "unrelated histories".
    fn make_encrypted_vault(
        key: &crate::crypto::MasterKey,
        pages: &[(&str, &str)],
        store: &MemStore,
    ) -> TempDir {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_marker(tmp.path(), &VaultMarker::new("0.0.0")).unwrap();
        let wiki = wiki_dir(tmp.path());
        init_repo(&wiki).unwrap();
        let account = keychain::vault_account(tmp.path()).unwrap();
        keychain::store_master_key(store, &account, key).unwrap();
        crate::crypto::gitfilter::write_canary(&wiki, &key.derive()).unwrap();
        for (id, body) in pages {
            let abs = wiki.join(format!("{id}.md"));
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(&abs, format!("---\nid: {id}\ntype: entity\ntitle: t\n---\n\n{body}\n"))
                .unwrap();
        }
        crate::wiki::encryption::commit_wiki_with_store(&wiki, "init", store).unwrap();
        tmp
    }

    #[test]
    fn merges_unrelated_histories_with_the_same_key() {
        // Variant 2: two Brains created independently (no common ancestor)
        // but keyed identically. Unique pages must union; a page that
        // exists on both with different content must surface as a conflict.
        let store = MemStore::default();
        let key = crate::crypto::MasterKey::from_bytes([9u8; 32]);
        let a = make_encrypted_vault(
            &key,
            &[("entities/only-a", "aaa"), ("entities/shared", "shared from A")],
            &store,
        );
        let b = make_encrypted_vault(
            &key,
            &[("entities/only-b", "bbb"), ("entities/shared", "shared from B")],
            &store,
        );
        let a_wiki = wiki_dir(a.path());
        let b_wiki = wiki_dir(b.path());

        set_remote(&b_wiki, &a_wiki.to_string_lossy()).unwrap();
        fetch(&b_wiki).unwrap();
        let branch = current_branch(&init_repo(&b_wiki).unwrap()).unwrap();
        let outcome = merge_from_remote_with_store(&b_wiki, &branch, &store).unwrap();

        let (sha, conflicts) = match outcome {
            MergeOutcome::Merged { sha, conflicted_pages } => (sha, conflicted_pages),
            other => panic!("expected a merge of unrelated histories, got {other:?}"),
        };
        assert!(!sha.is_empty());

        let keys = key.derive();
        let shared = format!("entities/{}.md", keys.filename_token("entities/shared"));
        assert!(conflicts.contains(&shared), "the shared id must conflict: {conflicts:?}");

        // Union: every side's pages are in the merged tree.
        let repo = git2::Repository::open(&b_wiki).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        for id in ["entities/only-a", "entities/only-b", "entities/shared"] {
            let p = format!("entities/{}.md", keys.filename_token(id));
            assert!(tree.get_path(Path::new(&p)).is_ok(), "merged tree missing {id}");
        }
        // The conflicted page's working tree carries BOTH versions (markers).
        let merged_shared = std::fs::read_to_string(b_wiki.join(&shared)).unwrap();
        assert!(
            merged_shared.contains("shared from A") && merged_shared.contains("shared from B"),
            "both versions must be present for the user to resolve: {merged_shared}"
        );
    }
}
