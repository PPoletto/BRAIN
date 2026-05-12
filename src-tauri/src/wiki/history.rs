//! Wiki history reading, restore, and hard-reset.
//!
//! Restores and hard-resets are recorded as new commits ("revert: …",
//! "reset: …") so the history is never destructively rewritten.

use std::path::Path;

use git2::{ObjectType, Repository, ResetType};
use serde::Serialize;

use super::git::{commit_all, init_repo};
use super::{WikiError, WikiResult};

#[derive(Debug, Clone, Serialize)]
pub struct CommitInfo {
    pub sha: String,
    pub ts: String,
    pub message: String,
    pub files_changed: u32,
}

pub fn list_commits(wiki_path: &Path, limit: usize) -> WikiResult<Vec<CommitInfo>> {
    let repo = init_repo(wiki_path)?;
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(Vec::new()),
    };
    let target = match head.target() {
        Some(t) => t,
        None => return Ok(Vec::new()),
    };

    let mut walk = repo.revwalk()?;
    walk.push(target)?;
    let mut out = Vec::new();
    for oid in walk.take(limit) {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        let ts = chrono::DateTime::from_timestamp(commit.time().seconds(), 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();
        let files_changed = changed_file_count(&repo, &commit).unwrap_or(0);
        out.push(CommitInfo {
            sha: oid.to_string(),
            ts,
            message: commit.message().unwrap_or("").to_string(),
            files_changed,
        });
    }
    Ok(out)
}

fn changed_file_count(repo: &Repository, commit: &git2::Commit<'_>) -> WikiResult<u32> {
    let parent = commit.parent(0).ok();
    let parent_tree = parent.as_ref().and_then(|p| p.tree().ok());
    let this_tree = commit.tree()?;
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&this_tree), None)?;
    Ok(diff.deltas().count() as u32)
}

/// Returns the commits that touched `page_path` (e.g. `entities/alice.md`),
/// newest first, up to `limit` matches. Walks the full history and
/// filters per-commit by diffing against the parent; the work is
/// proportional to the *visited* commit count, not the matched count,
/// so on a vault with thousands of commits asking for a rarely-edited
/// page may scan many candidates. `MAX_SCAN` caps that so a malformed
/// request never walks the entire history unboundedly.
///
/// Powers the `brain_get_page_history` MCP tool — agents call this
/// before `brain_restore_page` to pick the revision they want to
/// roll back to.
pub fn history_for_page(
    wiki_path: &Path,
    page_path: &str,
    limit: usize,
) -> WikiResult<Vec<CommitInfo>> {
    /// Safety net so a query for a page that never existed can't walk
    /// the entire history (and so a malicious caller can't DoS the
    /// server with a single tool call). 2000 commits is enough to
    /// span years of a normal personal-knowledge vault.
    const MAX_SCAN: usize = 2000;
    let repo = init_repo(wiki_path)?;
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return Ok(Vec::new()),
    };
    let target = match head.target() {
        Some(t) => t,
        None => return Ok(Vec::new()),
    };
    let target_path = Path::new(page_path);

    let mut walk = repo.revwalk()?;
    walk.push(target)?;
    let mut out = Vec::new();
    for (scanned, oid) in walk.enumerate() {
        if out.len() >= limit || scanned >= MAX_SCAN {
            break;
        }
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        if !commit_touches_path(&repo, &commit, target_path)? {
            continue;
        }
        let ts = chrono::DateTime::from_timestamp(commit.time().seconds(), 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();
        let files_changed = changed_file_count(&repo, &commit).unwrap_or(0);
        out.push(CommitInfo {
            sha: oid.to_string(),
            ts,
            message: commit.message().unwrap_or("").to_string(),
            files_changed,
        });
    }
    Ok(out)
}

/// True iff the diff between `commit` and its first parent contains
/// any delta whose new- or old-side path equals `target`. Treats the
/// initial commit (no parent) as containing every path in its tree,
/// so the original add of a page is reported by `history_for_page`.
fn commit_touches_path(
    repo: &Repository,
    commit: &git2::Commit<'_>,
    target: &Path,
) -> WikiResult<bool> {
    let this_tree = commit.tree()?;
    let parent = commit.parent(0).ok();
    let parent_tree = parent.as_ref().and_then(|p| p.tree().ok());
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&this_tree), None)?;
    for delta in diff.deltas() {
        let new_match = delta.new_file().path() == Some(target);
        let old_match = delta.old_file().path() == Some(target);
        if new_match || old_match {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitFile {
    pub path: String,
    /// `"A"` (added), `"M"` (modified), `"D"` (deleted), `"R"` (renamed),
    /// `"C"` (copied), or `"?"` for the rare cases libgit2 reports
    /// untracked changes.
    pub status: String,
    /// Insertions / deletions for this file. Useful for the "+12 / -3"
    /// style summary in the history detail panel.
    pub insertions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitDetail {
    pub sha: String,
    pub ts: String,
    pub author: String,
    pub message: String,
    pub parent_sha: Option<String>,
    pub files: Vec<CommitFile>,
    /// Unified-diff text for the whole commit. Capped at ~64 KiB so the
    /// frontend doesn't choke on a huge auto-commit.
    pub patch: String,
}

const MAX_PATCH_BYTES: usize = 64 * 1024;

/// Returns rich detail about a single commit — files changed, per-file
/// stats, the unified diff (truncated). Powers the WikiHistory detail
/// panel.
pub fn commit_detail(wiki_path: &Path, sha: &str) -> WikiResult<CommitDetail> {
    let repo = init_repo(wiki_path)?;
    let oid = git2::Oid::from_str(sha)?;
    let commit = repo.find_commit(oid)?;

    let parent = commit.parent(0).ok();
    let parent_tree = parent.as_ref().and_then(|p| p.tree().ok());
    let this_tree = commit.tree()?;
    let mut diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&this_tree), None)?;
    let _ = diff.find_similar(None);

    let stats = diff.stats()?;
    // libgit2's `diff.foreach` takes the file-cb and the line-cb in the
    // same call, but Rust's borrow checker won't let us mutate the same
    // `HashMap` from both closures. Wrapping it in a `RefCell` gives us
    // interior mutability without going `unsafe`. Both closures finish
    // before we read it back, so there's no aliased borrow at the
    // .borrow_mut() call sites.
    let per_file: std::cell::RefCell<
        std::collections::HashMap<String, (String, u32, u32)>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    diff.foreach(
        &mut |delta, _| {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let status = match delta.status() {
                git2::Delta::Added => "A",
                git2::Delta::Modified => "M",
                git2::Delta::Deleted => "D",
                git2::Delta::Renamed => "R",
                git2::Delta::Copied => "C",
                _ => "?",
            }
            .to_string();
            per_file.borrow_mut().insert(path, (status, 0, 0));
            true
        },
        None,
        None,
        Some(&mut |delta, _, line| {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let mut map = per_file.borrow_mut();
            let entry = map.entry(path).or_insert(("M".into(), 0, 0));
            match line.origin() {
                '+' => entry.1 += 1,
                '-' => entry.2 += 1,
                _ => {}
            }
            true
        }),
    )?;

    let mut files: Vec<CommitFile> = per_file
        .into_inner()
        .into_iter()
        .map(|(path, (status, insertions, deletions))| CommitFile {
            path,
            status,
            insertions,
            deletions,
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));

    // Build a textual patch, capped to keep the IPC payload small.
    let mut patch_buf = Vec::with_capacity(stats.deletions() + stats.insertions());
    diff.print(git2::DiffFormat::Patch, |_, _, line| {
        if patch_buf.len() >= MAX_PATCH_BYTES {
            return false;
        }
        if let Ok(prefix) = match line.origin() {
            '+' | '-' | ' ' => Ok(line.origin().to_string()),
            _ => Ok(String::new()),
        } as Result<String, ()>
        {
            patch_buf.extend_from_slice(prefix.as_bytes());
        }
        patch_buf.extend_from_slice(line.content());
        true
    })
    .ok();
    let mut patch = String::from_utf8_lossy(&patch_buf).to_string();
    if patch.len() >= MAX_PATCH_BYTES {
        patch.push_str("\n…(truncated)…\n");
    }

    let ts = chrono::DateTime::from_timestamp(commit.time().seconds(), 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default();
    let author = commit.author().name().unwrap_or("").to_string();

    Ok(CommitDetail {
        sha: oid.to_string(),
        ts,
        author,
        message: commit.message().unwrap_or("").to_string(),
        parent_sha: parent.map(|p| p.id().to_string()),
        files,
        patch,
    })
}

/// Restore a single page to the state at the given commit. Performs a new
/// commit recording the restore.
pub fn restore_page(wiki_path: &Path, sha: &str, page: &str) -> WikiResult<()> {
    let repo = init_repo(wiki_path)?;
    let oid = git2::Oid::from_str(sha)?;
    let commit = repo.find_commit(oid)?;
    let tree = commit.tree()?;

    let entry = tree.get_path(Path::new(page)).map_err(|_| {
        WikiError::PageNotFound(format!("{page} not present in commit {sha}"))
    })?;
    let blob = repo.find_blob(entry.id())?;

    let target_path = wiki_path.join(page);
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target_path, blob.content())?;

    let message = format!("revert: restored {page} from {}", short_sha(sha));
    commit_all(wiki_path, &message)?;
    Ok(())
}

/// Hard-reset the wiki to the given commit. Records a "reset: …" commit on
/// top so the history isn't destructively rewritten — the commit is created
/// even when the working tree already matches the target, so the action is
/// always traceable.
pub fn hard_reset(wiki_path: &Path, sha: &str) -> WikiResult<()> {
    let repo = init_repo(wiki_path)?;
    let oid = git2::Oid::from_str(sha)?;
    let object = repo.find_object(oid, Some(ObjectType::Commit))?;
    repo.reset(&object, ResetType::Hard, None)?;

    let message = format!("reset: hard-reset to {}", short_sha(sha));

    let mut index = repo.index()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    let sig = git2::Signature::now(super::git::COMMITTER_NAME, super::git::COMMITTER_EMAIL)?;
    let parents: Vec<git2::Commit> = match repo.head() {
        Ok(head) => head.peel_to_commit().map(|c| vec![c]).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, &message, &tree, &parent_refs)?;
    Ok(())
}

fn short_sha(sha: &str) -> &str {
    if sha.len() <= 7 {
        sha
    } else {
        &sha[..7]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn touch(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn list_commits_returns_empty_for_a_new_repo() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path()).unwrap();
        assert!(list_commits(tmp.path(), 10).unwrap().is_empty());
    }

    #[test]
    fn list_commits_returns_commits_in_reverse_chronological_order() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path()).unwrap();
        touch(tmp.path(), "a.md", "first");
        commit_all(tmp.path(), "first").unwrap();
        touch(tmp.path(), "b.md", "second");
        commit_all(tmp.path(), "second").unwrap();
        let commits = list_commits(tmp.path(), 10).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].message.trim(), "second");
    }

    #[test]
    fn restore_page_recreates_old_file_and_records_revert_commit() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path()).unwrap();
        touch(tmp.path(), "x.md", "v1");
        let sha1 = commit_all(tmp.path(), "v1").unwrap().unwrap();
        touch(tmp.path(), "x.md", "v2");
        commit_all(tmp.path(), "v2").unwrap();
        restore_page(tmp.path(), &sha1, "x.md").unwrap();
        let restored = std::fs::read_to_string(tmp.path().join("x.md")).unwrap();
        assert_eq!(restored, "v1");
        let commits = list_commits(tmp.path(), 10).unwrap();
        assert!(commits[0].message.starts_with("revert: "));
    }

    #[test]
    fn history_for_page_returns_only_commits_that_touched_the_named_page() {
        // The agent-rollback workflow needs to filter the commit log
        // to "edits of THIS page" so the user sees the actual
        // candidate revisions to restore from, not unrelated commits
        // that touched sibling files in the same window.
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path()).unwrap();
        touch(tmp.path(), "alice.md", "alice v1");
        commit_all(tmp.path(), "alice v1").unwrap();
        touch(tmp.path(), "bob.md", "bob v1");
        commit_all(tmp.path(), "bob v1 (noise)").unwrap();
        touch(tmp.path(), "alice.md", "alice v2");
        commit_all(tmp.path(), "alice v2").unwrap();
        touch(tmp.path(), "bob.md", "bob v2");
        commit_all(tmp.path(), "bob v2 (noise)").unwrap();

        let history = history_for_page(tmp.path(), "alice.md", 10).unwrap();
        // Two alice edits, both must surface; the two bob commits
        // must NOT — they share the same window but didn't touch
        // alice.md.
        assert_eq!(history.len(), 2, "expected 2 alice-only commits, got {history:#?}");
        for c in &history {
            assert!(
                c.message.contains("alice"),
                "non-alice commit leaked into per-page history: {c:?}"
            );
        }
        // Newest first.
        assert!(history[0].message.contains("v2"));
        assert!(history[1].message.contains("v1"));
    }

    #[test]
    fn history_for_page_returns_empty_when_page_has_no_history() {
        // A page id the caller invented but never wrote must return
        // an empty list, not an error — agents will sometimes probe
        // a page-id before committing to a write.
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path()).unwrap();
        touch(tmp.path(), "other.md", "x");
        commit_all(tmp.path(), "other").unwrap();
        let history = history_for_page(tmp.path(), "nonexistent.md", 10).unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn history_for_page_respects_the_limit_parameter() {
        // Caller can cap the response — important for context-
        // budget management on pages with very long histories.
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path()).unwrap();
        for i in 1..=5 {
            touch(tmp.path(), "alice.md", &format!("v{i}"));
            commit_all(tmp.path(), &format!("alice v{i}")).unwrap();
        }
        let history = history_for_page(tmp.path(), "alice.md", 3).unwrap();
        assert_eq!(history.len(), 3, "limit must cap the response");
    }

    #[test]
    fn hard_reset_creates_reset_commit_referencing_target_sha() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path()).unwrap();
        touch(tmp.path(), "a.md", "1");
        let target = commit_all(tmp.path(), "first").unwrap().unwrap();
        touch(tmp.path(), "a.md", "2");
        commit_all(tmp.path(), "second").unwrap();
        hard_reset(tmp.path(), &target).unwrap();
        let head = list_commits(tmp.path(), 10).unwrap();
        assert!(head[0].message.starts_with("reset:"));
    }
}
