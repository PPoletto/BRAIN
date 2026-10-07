//! Small file-system helpers shared by modules that rewrite a file in
//! place (the eval set, the client instruction files of the self-install).
//!
//! Windows needs care here: deleting or replacing a file another thread or
//! process still has open — or one that is "delete pending" because its
//! last handle is just being closed — fails with `ERROR_ACCESS_DENIED`
//! (`io::ErrorKind::PermissionDenied`) for a few milliseconds instead of
//! succeeding as on Unix. [`is_transient_lock_error`] names that case and
//! [`atomic_write`] retries through it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How often [`atomic_write`] retries a rename that failed with a
/// transient lock error, and the first back-off (doubled per attempt:
/// 5, 10, 20 … ms — about 2.5 s in total).
const RENAME_ATTEMPTS: u32 = 9;
const RENAME_FIRST_BACKOFF: Duration = Duration::from_millis(5);

/// True for an error that, on Windows, means "someone holds this path for
/// a moment" (an open handle without delete sharing, or a file whose
/// deletion is pending) — worth retrying. Always false elsewhere, where a
/// permission error is a real permission error.
pub fn is_transient_lock_error(err: &io::Error) -> bool {
    cfg!(windows) && err.kind() == io::ErrorKind::PermissionDenied
}

/// A temporary sibling of `target` whose name is unique per process and
/// call (`<name>.<pid>-<n>.tmp`), so two writers never share a temp file.
fn unique_temp_path(target: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    target.with_file_name(format!("{name}.{}-{n}.tmp", std::process::id()))
}

/// The path a write to `target` must go to: `target` itself, or — when
/// `target` is a symbolic link (dotfile managers such as stow, yadm or
/// home-manager link `~/.claude/CLAUDE.md` into a repository) — the file
/// the link points to, so the link survives the write. A link whose
/// target cannot be resolved is an error: BRAIN does not replace a link
/// with a regular file.
pub fn write_destination(target: &Path) -> io::Result<PathBuf> {
    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(target).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "{} is a symbolic link whose target cannot be resolved ({err}) — BRAIN does \
                     not replace the link with a file",
                    target.display()
                ),
            )
        }),
        _ => Ok(target.to_path_buf()),
    }
}

/// True when `path` itself is a symbolic link.
pub fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Write `data` into a new file `tmp`, flushed to disk, with the
/// permissions of `like` when that exists (Unix mode bits).
fn write_temp(tmp: &Path, data: &[u8], like: &Path) -> io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(tmp)?;
    file.write_all(data)?;
    file.sync_all()?;
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(like) {
        std::fs::set_permissions(tmp, meta.permissions())?;
    }
    #[cfg(not(unix))]
    let _ = like;
    Ok(())
}

/// Write `data` to `target` so that a reader sees either the old or the
/// new content, never a half-written file: write a uniquely named temp
/// file next to the destination (flushed to disk, Unix mode of the old
/// file kept), then rename it over the destination. When `target` is a
/// symbolic link the destination is the file it points to (see
/// [`write_destination`]), so the link stays a link. A rename refused
/// with a transient Windows lock error is retried with back-off; if it
/// still fails, the temp file is removed and the error returned.
pub fn atomic_write(target: &Path, data: &[u8]) -> io::Result<()> {
    let dest = write_destination(target)?;
    let tmp = unique_temp_path(&dest);
    if let Err(err) = write_temp(&tmp, data, &dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    let mut backoff = RENAME_FIRST_BACKOFF;
    let mut attempt = 1;
    loop {
        match std::fs::rename(&tmp, &dest) {
            Ok(()) => return Ok(()),
            Err(err) if is_transient_lock_error(&err) && attempt < RENAME_ATTEMPTS => {
                std::thread::sleep(backoff);
                backoff *= 2;
                attempt += 1;
            }
            Err(err) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(err);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_creates_a_missing_file_with_the_given_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        atomic_write(&target, b"hello").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
    }

    #[test]
    fn atomic_write_replaces_an_existing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, b"old").unwrap();
        atomic_write(&target, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn atomic_write_leaves_no_temporary_file_behind() {
        let tmp = tempfile::TempDir::new().unwrap();
        atomic_write(&tmp.path().join("a.txt"), b"x").unwrap();
        let names: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect();
        assert_eq!(names, vec!["a.txt".to_string()]);
    }

    /// Create `link` → `original`; false when the platform refuses (a
    /// Windows account without the symlink privilege) — the test then
    /// has nothing to check.
    fn try_symlink(original: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(original, link);
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(original, link);
        made.is_ok()
    }

    #[test]
    fn a_symlinked_target_stays_a_symlink_after_a_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real.md");
        std::fs::write(&real, b"old").unwrap();
        let link = tmp.path().join("link.md");
        if !try_symlink(&real, &link) {
            return;
        }
        atomic_write(&link, b"new").unwrap();
        assert!(is_symlink(&link));
    }

    #[test]
    fn a_write_through_a_symlink_lands_in_the_linked_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real.md");
        std::fs::write(&real, b"old").unwrap();
        let link = tmp.path().join("link.md");
        if !try_symlink(&real, &link) {
            return;
        }
        atomic_write(&link, b"new").unwrap();
        assert_eq!(std::fs::read(&real).unwrap(), b"new");
    }

    #[test]
    fn a_dangling_symlink_is_not_replaced() {
        let tmp = tempfile::TempDir::new().unwrap();
        let link = tmp.path().join("link.md");
        if !try_symlink(&tmp.path().join("gone.md"), &link) {
            return;
        }
        assert!(atomic_write(&link, b"new").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_the_unix_mode_of_the_old_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, b"old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        atomic_write(&target, b"new").unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn two_temp_paths_for_the_same_target_differ() {
        let target = Path::new("dir/a.txt");
        assert_ne!(unique_temp_path(target), unique_temp_path(target));
    }

    #[test]
    fn a_permission_error_counts_as_transient_only_on_windows() {
        let err = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(is_transient_lock_error(&err), cfg!(windows));
    }
}
