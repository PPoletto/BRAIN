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

/// Write `data` to `target` so that a reader sees either the old or the
/// new content, never a half-written file: write a uniquely named temp
/// file next to it, then rename it over `target`. A rename refused with a
/// transient Windows lock error is retried with back-off; if it still
/// fails, the temp file is removed and the error returned.
pub fn atomic_write(target: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = unique_temp_path(target);
    if let Err(err) = std::fs::write(&tmp, data) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    let mut backoff = RENAME_FIRST_BACKOFF;
    let mut attempt = 1;
    loop {
        match std::fs::rename(&tmp, target) {
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
