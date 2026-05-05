//! Volume / folder watcher.
//!
//! A platform-agnostic abstraction. The default implementation polls
//! `sysinfo::Disks` for disk-mode detection and polls the registered folder
//! paths for folder-mode. Platform-specific watchers (DiskArbitration / udev
//! / WMI) can later replace the polling strategy without changing callers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::vault::layout::is_vault;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeEvent {
    /// A Brain source appeared at this path (disk-mode label `BRAIN`, or a
    /// registered folder that now contains a marker).
    Appeared(PathBuf),
    /// A previously known source disappeared.
    Disappeared(PathBuf),
}

/// Brain-source watcher.
pub struct SourceWatcher {
    handle: JoinHandle<()>,
    rx: mpsc::Receiver<ChangeEvent>,
}

impl SourceWatcher {
    /// Spawns a polling task that emits change events. The poll interval is
    /// short (500 ms) to meet the 5 s mount-latency budget from S01.
    pub fn spawn(folder_sources: Vec<PathBuf>) -> Self {
        let (tx, rx) = mpsc::channel(32);
        let mut known: HashSet<PathBuf> = HashSet::new();
        let folders: Arc<Vec<PathBuf>> = Arc::new(folder_sources);

        let handle = tokio::spawn(async move {
            loop {
                let current = scan_for_sources(&folders);
                for path in &current {
                    if !known.contains(path) {
                        known.insert(path.clone());
                        let _ = tx.send(ChangeEvent::Appeared(path.clone())).await;
                    }
                }
                let stale: Vec<PathBuf> = known.difference(&current).cloned().collect();
                for path in stale {
                    known.remove(&path);
                    let _ = tx.send(ChangeEvent::Disappeared(path)).await;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });

        Self { handle, rx }
    }

    pub async fn next(&mut self) -> Option<ChangeEvent> {
        self.rx.recv().await
    }

    pub fn abort(self) {
        self.handle.abort();
    }
}

fn scan_for_sources(folder_sources: &[PathBuf]) -> HashSet<PathBuf> {
    let mut out: HashSet<PathBuf> = HashSet::new();
    for f in folder_sources {
        if is_vault(f) {
            out.insert(f.clone());
        }
    }
    for path in scan_disk_volumes() {
        if is_vault(&path) {
            out.insert(path);
        }
    }
    out
}

fn scan_disk_volumes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let disks = sysinfo::Disks::new_with_refreshed_list();
    for disk in &disks {
        let mount = disk.mount_point().to_path_buf();
        let label_matches_brain = disk
            .name()
            .to_string_lossy()
            .to_uppercase()
            .contains("BRAIN");
        let mount_str = mount.to_string_lossy().to_uppercase();
        let path_matches_brain = mount_str.ends_with("BRAIN") || mount_str.ends_with("BRAIN/");
        if label_matches_brain || path_matches_brain {
            out.push(mount);
        }
    }
    out
}

/// Volume-label match according to S01: the Brain SSD carries the volume
/// label `BRAIN`. Other labels are ignored.
pub fn matches_brain_label(label: &str) -> bool {
    label.eq_ignore_ascii_case("BRAIN")
}

/// True when `path` looks like a Brain source (folder-mode or mounted disk
/// containing the marker file).
pub fn looks_like_brain_source(path: &Path) -> bool {
    is_vault(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::ensure_skeleton;
    use crate::vault::marker::{write_marker, VaultMarker};
    use tempfile::TempDir;

    #[test]
    fn matches_brain_label_ignores_case_and_rejects_other_labels() {
        assert!(matches_brain_label("BRAIN"));
        assert!(matches_brain_label("brain"));
        assert!(matches_brain_label("Brain"));
        assert!(!matches_brain_label("OTHER"));
        assert!(!matches_brain_label("BACKUP"));
    }

    #[test]
    fn looks_like_brain_source_is_true_only_with_marker_file() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        assert!(!looks_like_brain_source(tmp.path()));
        write_marker(tmp.path(), &VaultMarker::new("0.1.0")).unwrap();
        assert!(looks_like_brain_source(tmp.path()));
    }

    #[tokio::test]
    async fn watcher_emits_appeared_when_a_registered_folder_becomes_a_vault() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_marker(tmp.path(), &VaultMarker::new("0.1.0")).unwrap();

        let mut w = SourceWatcher::spawn(vec![tmp.path().to_path_buf()]);
        // The watcher also enumerates physical Brain volumes that may be
        // attached to the host running the tests, so the first emitted
        // event is not necessarily our temp folder. Drain events until we
        // see the one we care about — or until the deadline expires.
        let target = tmp.path().to_path_buf();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut found = false;
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = match tokio::time::timeout(remaining, w.next()).await {
                Ok(Some(ev)) => ev,
                _ => break,
            };
            if let ChangeEvent::Appeared(p) = &event {
                if p == &target {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "watcher should report the temp vault as Appeared");
    }
}
