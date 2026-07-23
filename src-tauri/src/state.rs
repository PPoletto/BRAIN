//! Application-wide state held in a shared `Arc<AppState>`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::RwLock;

use crate::config::ConfigStore;
use crate::db::DbHandle;
use crate::mcp::registration::RegistrationReport;
use crate::onboarding::disks::DiskInfo;
use crate::wiki::watcher::WikiWatcher;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountState {
    Disconnected,
    Mounting,
    MountedIdle,
    MountedBusy,
    Error(String),
}

impl MountState {
    pub fn as_tag(&self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Mounting => "mounting",
            Self::MountedIdle => "mounted-idle",
            Self::MountedBusy => "mounted-busy",
            Self::Error(_) => "error",
        }
    }
}

pub struct AppState {
    pub config: ConfigStore,
    mount: RwLock<MountState>,
    vault_path: RwLock<Option<PathBuf>>,
    db: RwLock<Option<DbHandle>>,
    last_registration: RwLock<Option<RegistrationReport>>,
    disk_cache: RwLock<Option<Vec<DiskInfo>>>,
    active_ops: AtomicU32,
    /// The running wiki file-watcher, kept so operations that rewrite the
    /// working tree in bulk (e.g. the encrypt/convert step) can pause it
    /// — abort, do the work, respawn — instead of racing its auto-commit.
    watcher: RwLock<Option<WikiWatcher>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            config: ConfigStore::new(),
            mount: RwLock::new(MountState::Disconnected),
            vault_path: RwLock::new(None),
            db: RwLock::new(None),
            last_registration: RwLock::new(None),
            disk_cache: RwLock::new(None),
            active_ops: AtomicU32::new(0),
            watcher: RwLock::new(None),
        }
    }

    /// Store the running watcher, aborting any previous one first.
    pub fn set_watcher(&self, watcher: Option<WikiWatcher>) {
        let mut guard = self.watcher.write().expect("watcher write lock");
        if let Some(old) = guard.take() {
            old.abort();
        }
        *guard = watcher;
    }

    /// Take the running watcher out (aborting is the caller's job — used
    /// to pause auto-commit around a bulk working-tree rewrite).
    pub fn take_watcher(&self) -> Option<WikiWatcher> {
        self.watcher.write().expect("watcher write lock").take()
    }

    pub fn disk_cache(&self) -> Option<Vec<DiskInfo>> {
        self.disk_cache.read().expect("disk_cache read lock").clone()
    }

    pub fn set_disk_cache(&self, disks: Vec<DiskInfo>) {
        *self
            .disk_cache
            .write()
            .expect("disk_cache write lock") = Some(disks);
    }

    pub fn clear_disk_cache(&self) {
        *self
            .disk_cache
            .write()
            .expect("disk_cache write lock") = None;
    }

    pub fn last_registration(&self) -> Option<RegistrationReport> {
        self.last_registration
            .read()
            .expect("last_registration read lock")
            .clone()
    }

    pub fn set_last_registration(&self, report: Option<RegistrationReport>) {
        *self
            .last_registration
            .write()
            .expect("last_registration write lock") = report;
    }

    pub fn db(&self) -> Option<DbHandle> {
        self.db.read().expect("db read lock").clone()
    }

    pub fn set_db(&self, handle: Option<DbHandle>) {
        *self.db.write().expect("db write lock") = handle;
    }

    pub fn mount(&self) -> MountState {
        self.mount.read().expect("mount read lock").clone()
    }

    pub fn set_mount(&self, state: MountState) {
        *self.mount.write().expect("mount write lock") = state;
    }

    pub fn vault_path(&self) -> Option<PathBuf> {
        self.vault_path.read().expect("vault path read lock").clone()
    }

    pub fn set_vault_path(&self, path: Option<PathBuf>) {
        *self.vault_path.write().expect("vault path write lock") = path;
    }

    pub fn begin_op(&self) -> u32 {
        self.active_ops.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn end_op(&self) -> u32 {
        let prev = self.active_ops.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0, "end_op without matching begin_op");
        prev.saturating_sub(1)
    }

    pub fn active_ops(&self) -> u32 {
        self.active_ops.load(Ordering::SeqCst)
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_disconnected_with_no_vault_and_zero_ops() {
        let s = AppState::new();
        assert_eq!(s.mount(), MountState::Disconnected);
        assert!(s.vault_path().is_none());
        assert_eq!(s.active_ops(), 0);
    }

    #[test]
    fn begin_and_end_op_track_active_count() {
        let s = AppState::new();
        assert_eq!(s.begin_op(), 1);
        assert_eq!(s.begin_op(), 2);
        assert_eq!(s.end_op(), 1);
        assert_eq!(s.end_op(), 0);
        assert_eq!(s.active_ops(), 0);
    }

    #[test]
    fn mount_state_tag_strings_match_frontend_contract() {
        assert_eq!(MountState::Disconnected.as_tag(), "disconnected");
        assert_eq!(MountState::Mounting.as_tag(), "mounting");
        assert_eq!(MountState::MountedIdle.as_tag(), "mounted-idle");
        assert_eq!(MountState::MountedBusy.as_tag(), "mounted-busy");
        assert_eq!(MountState::Error("boom".into()).as_tag(), "error");
    }
}
