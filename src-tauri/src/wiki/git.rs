//! Git operations for the wiki: init, add, commit, status.

use std::path::Path;

use git2::{IndexAddOption, Repository, Signature};

use super::WikiResult;

pub const COMMITTER_NAME: &str = "Brain Client";
pub const COMMITTER_EMAIL: &str = "brain@local";

/// Initializes a Git repository in `wiki_path` with platform-tolerant config:
/// `core.ignorecase=true`, `core.fileMode=false`. Idempotent.
pub fn init_repo(wiki_path: &Path) -> WikiResult<Repository> {
    std::fs::create_dir_all(wiki_path)?;
    let repo = match Repository::open(wiki_path) {
        Ok(r) => r,
        Err(_) => Repository::init(wiki_path)?,
    };
    {
        let mut cfg = repo.config()?;
        cfg.set_bool("core.ignorecase", true)?;
        cfg.set_bool("core.fileMode", false)?;
        cfg.set_str("user.name", COMMITTER_NAME)?;
        cfg.set_str("user.email", COMMITTER_EMAIL)?;
    }
    Ok(repo)
}

/// Stages all changes in the wiki and creates a commit. Returns the commit
/// SHA (hex string). Skips the commit when nothing changed.
pub fn commit_all(wiki_path: &Path, message: &str) -> WikiResult<Option<String>> {
    let repo = init_repo(wiki_path)?;
    let mut index = repo.index()?;
    index.add_all(["."].iter(), IndexAddOption::DEFAULT, None)?;
    commit_index(&repo, &mut index, message)
}

/// Writes the caller-populated `index` to a tree and commits it on HEAD,
/// returning the commit SHA. Skips (returns `None`) when the resulting
/// tree equals HEAD's — so identical content never produces an empty
/// commit. Does **not** stage anything itself: the caller decides what
/// the index holds. This is the seam the encrypting-commit path uses to
/// commit ciphertext blobs it has staged by hand (git2 will not run our
/// external clean filter), while [`commit_all`] uses it after a plain
/// `add_all`.
pub fn commit_index(
    repo: &Repository,
    index: &mut git2::Index,
    message: &str,
) -> WikiResult<Option<String>> {
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;

    let sig = Signature::now(COMMITTER_NAME, COMMITTER_EMAIL)?;

    let parents: Vec<git2::Commit> = match repo.head() {
        Ok(head) => match head.peel_to_commit() {
            Ok(c) => vec![c],
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    };

    if let Some(parent) = parents.first() {
        if parent.tree_id() == tree_id {
            return Ok(None);
        }
    }

    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    let oid = repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)?;
    Ok(Some(oid.to_string()))
}

/// Like [`commit_index`], but commits the staged tree as an ORPHAN — a
/// fresh root commit with no parents — after preserving the current
/// history under `backup_ref` (local-only; sync pushes just the current
/// branch). The current branch is then repointed at the new root.
///
/// Used by the encryption convert: everything before the convert contains
/// PLAINTEXT blobs and human filenames, and a normal commit would keep
/// that history reachable — the first push would publish it. Re-rooting
/// makes the encrypted snapshot the only thing a remote can ever see.
pub fn commit_index_as_new_root(
    repo: &Repository,
    index: &mut git2::Index,
    message: &str,
    backup_ref: &str,
) -> WikiResult<Option<String>> {
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    let sig = Signature::now(COMMITTER_NAME, COMMITTER_EMAIL)?;

    // Preserve the old history locally before disconnecting it.
    if let Ok(old) = repo.head().and_then(|h| h.peel_to_commit()) {
        repo.reference(
            backup_ref,
            old.id(),
            true,
            "convert: preserve pre-encryption history locally",
        )?;
    }

    // The branch HEAD points at (resolves for an unborn branch too).
    let branch_ref = repo
        .find_reference("HEAD")
        .ok()
        .and_then(|r| r.symbolic_target().map(str::to_string))
        .unwrap_or_else(|| "refs/heads/master".to_string());

    let oid = repo.commit(None, &sig, &sig, message, &tree, &[])?;
    repo.reference(&branch_ref, oid, true, "convert: fresh encrypted root")?;
    repo.set_head(&branch_ref)?;
    Ok(Some(oid.to_string()))
}

/// Returns the number of commits on HEAD.
pub fn commit_count(wiki_path: &Path) -> WikiResult<usize> {
    let repo = init_repo(wiki_path)?;
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(0),
    };
    let mut walk = repo.revwalk()?;
    walk.push(head.target().unwrap_or_else(|| {
        // Empty repo case
        git2::Oid::zero()
    }))?;
    Ok(walk.count())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn init_repo_sets_ignorecase_true_and_filemode_false() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path()).unwrap();
        let cfg = repo.config().unwrap();
        assert!(cfg.get_bool("core.ignorecase").unwrap());
        assert!(!cfg.get_bool("core.fileMode").unwrap());
    }

    #[test]
    fn init_repo_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path()).unwrap();
        init_repo(tmp.path()).unwrap();
        assert!(tmp.path().join(".git").exists());
    }

    #[test]
    fn commit_all_creates_commit_when_files_added() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("hello.md"), "hi").unwrap();
        let sha = commit_all(tmp.path(), "first").unwrap();
        assert!(sha.is_some());
        assert_eq!(commit_count(tmp.path()).unwrap(), 1);
    }

    #[test]
    fn commit_all_returns_none_when_nothing_changed() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("a.md"), "x").unwrap();
        commit_all(tmp.path(), "first").unwrap();
        let second = commit_all(tmp.path(), "noop").unwrap();
        assert!(second.is_none());
    }
}
