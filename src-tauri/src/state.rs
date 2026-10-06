//! Application-wide state held in a shared `Arc<AppState>`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use crate::config::ConfigStore;
use crate::db::DbHandle;
use crate::mcp::registration::RegistrationReport;
use crate::onboarding::disks::DiskInfo;
use crate::wiki::audit::AuditHandle;
use crate::wiki::auto_sync::AutoSyncHandle;
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

/// Everything the "what's running?" display needs, behind one lock.
#[derive(Default)]
struct OpsState {
    /// Blocking operations (one entry per active op). The length is the
    /// "N ops active" count — the unmount gate refuses while it is > 0.
    labels: Vec<String>,
    /// Non-blocking background tasks (e.g. the daily audit): shown with
    /// the other labels, but never counted by [`AppState::active_ops`], so
    /// they do not hold up an unmount.
    background: Vec<String>,
    /// `(done, total)` per active op label, shown as a suffix
    /// ("Rebuilding the index (120/843)").
    progress: HashMap<String, (usize, usize)>,
}

pub struct AppState {
    pub config: ConfigStore,
    mount: RwLock<MountState>,
    vault_path: RwLock<Option<PathBuf>>,
    db: RwLock<Option<DbHandle>>,
    last_registration: RwLock<Option<RegistrationReport>>,
    disk_cache: RwLock<Option<Vec<DiskInfo>>>,
    /// Operations in flight, their progress and the background tasks —
    /// ONE lock for all of it, so no two locks are ever taken in
    /// different orders (the tray polls the labels constantly).
    ops: RwLock<OpsState>,
    /// The running wiki file-watcher, kept so operations that rewrite the
    /// working tree in bulk (e.g. the encrypt/convert step) can pause it
    /// — abort, do the work, respawn — instead of racing its auto-commit.
    watcher: RwLock<Option<WikiWatcher>>,
    /// The running auto-sync scheduler (S11 phase 6), if enabled. Kept so
    /// it can be aborted on unmount or when auto-sync is toggled off.
    auto_sync_task: RwLock<Option<AutoSyncHandle>>,
    /// The running daily wiki-audit scheduler (A5), replaced on every
    /// mount so a remount never leaves two schedulers behind.
    audit_task: RwLock<Option<AuditHandle>>,
    /// Wakes the auto-sync scheduler early. The watcher nudges this after
    /// every successful auto-commit so local edits reach the remote within
    /// seconds instead of waiting out the polling interval (which remains
    /// the fallback that PULLS remote changes).
    sync_nudge: tokio::sync::Notify,
}

impl AppState {
    pub fn new() -> Self {
        Self::with_config(ConfigStore::new())
    }

    /// [`AppState`] with an injected config store. Tests pass one rooted
    /// in a TempDir (via [`ConfigStore::load_from`]) so their config
    /// reads/writes never touch the machine's real settings.json.
    pub fn with_config(config: ConfigStore) -> Self {
        Self {
            config,
            mount: RwLock::new(MountState::Disconnected),
            vault_path: RwLock::new(None),
            db: RwLock::new(None),
            last_registration: RwLock::new(None),
            disk_cache: RwLock::new(None),
            ops: RwLock::new(OpsState::default()),
            watcher: RwLock::new(None),
            auto_sync_task: RwLock::new(None),
            audit_task: RwLock::new(None),
            sync_nudge: tokio::sync::Notify::new(),
        }
    }

    /// Wake the auto-sync scheduler now (called by the watcher after a
    /// successful auto-commit). No-op when no scheduler is waiting.
    pub fn nudge_sync(&self) {
        self.sync_nudge.notify_one();
    }

    /// The notifier the auto-sync scheduler waits on between ticks.
    pub fn sync_nudge(&self) -> &tokio::sync::Notify {
        &self.sync_nudge
    }

    /// Store the running auto-sync scheduler, aborting any previous one.
    pub fn set_auto_sync_task(&self, task: Option<AutoSyncHandle>) {
        let mut guard = self.auto_sync_task.write().expect("auto_sync_task write lock");
        if let Some(old) = guard.take() {
            old.abort();
        }
        *guard = task;
    }

    /// Store the running audit scheduler, aborting any previous one.
    pub fn set_audit_task(&self, task: Option<AuditHandle>) {
        let mut guard = self.audit_task.write().expect("audit_task write lock");
        if let Some(old) = guard.take() {
            old.abort();
        }
        *guard = task;
    }

    /// Whether an auto-sync scheduler is currently running.
    pub fn auto_sync_running(&self) -> bool {
        self.auto_sync_task.read().expect("auto_sync_task read lock").is_some()
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

    /// Start an operation with a human label (e.g. "Syncing with the
    /// remote"). Pair with [`end_op`] using the SAME label. Clears any
    /// stale progress recorded for that label.
    pub fn begin_op(&self, label: &str) {
        let mut ops = self.ops.write().expect("ops write lock");
        ops.progress.remove(label);
        ops.labels.push(label.to_string());
    }

    /// End an operation started with [`begin_op`]. Removes one entry with
    /// the matching label (and its progress once no such op is left).
    pub fn end_op(&self, label: &str) {
        let mut ops = self.ops.write().expect("ops write lock");
        if let Some(pos) = ops.labels.iter().position(|l| l == label) {
            ops.labels.remove(pos);
        } else {
            debug_assert!(false, "end_op without matching begin_op: {label}");
        }
        if !ops.labels.iter().any(|l| l == label) {
            ops.progress.remove(label);
        }
    }

    /// Report progress of the running op `label` (e.g. pages re-indexed so
    /// far). Shown as a `(done/total)` suffix on the label; the begin/end
    /// pairing keeps using the bare label. Ignored unless an op with that
    /// label is active, so a late report cannot leave a stale suffix.
    pub fn set_op_progress(&self, label: &str, done: usize, total: usize) {
        let mut ops = self.ops.write().expect("ops write lock");
        if ops.labels.iter().any(|l| l == label) {
            ops.progress.insert(label.to_string(), (done, total));
        }
    }

    /// Start a NON-blocking background task: listed in
    /// [`AppState::active_op_labels`] but not counted by
    /// [`AppState::active_ops`] (the unmount gate). Pair with
    /// [`AppState::end_background_op`] using the same label.
    pub fn begin_background_op(&self, label: &str) {
        self.ops
            .write()
            .expect("ops write lock")
            .background
            .push(label.to_string());
    }

    /// End a task started with [`AppState::begin_background_op`].
    pub fn end_background_op(&self, label: &str) {
        let mut ops = self.ops.write().expect("ops write lock");
        if let Some(pos) = ops.background.iter().position(|l| l == label) {
            ops.background.remove(pos);
        } else {
            debug_assert!(false, "end_background_op without matching begin: {label}");
        }
    }

    /// Number of BLOCKING ops in flight (background tasks excluded).
    pub fn active_ops(&self) -> u32 {
        self.ops.read().expect("ops read lock").labels.len() as u32
    }

    /// Labels of the operations and background tasks currently in flight,
    /// for the tray/UI tooltip ("what's running?").
    pub fn active_op_labels(&self) -> Vec<String> {
        let ops = self.ops.read().expect("ops read lock");
        ops.labels
            .iter()
            .map(|label| match ops.progress.get(label) {
                Some((done, total)) => format!("{label} ({done}/{total})"),
                None => label.clone(),
            })
            .chain(ops.background.iter().cloned())
            .collect()
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
    fn begin_and_end_op_track_active_count_and_labels() {
        let s = AppState::new();
        s.begin_op("Syncing");
        s.begin_op("Rebuilding index");
        assert_eq!(s.active_ops(), 2);
        assert_eq!(s.active_op_labels(), vec!["Syncing", "Rebuilding index"]);
        s.end_op("Syncing");
        assert_eq!(s.active_ops(), 1);
        assert_eq!(s.active_op_labels(), vec!["Rebuilding index"]);
        s.end_op("Rebuilding index");
        assert_eq!(s.active_ops(), 0);
    }

    #[test]
    fn active_op_label_shows_the_reported_progress_as_done_over_total() {
        let s = AppState::new();
        s.begin_op("Rebuilding the index");
        s.set_op_progress("Rebuilding the index", 120, 843);
        assert_eq!(s.active_op_labels(), vec!["Rebuilding the index (120/843)"]);
    }

    #[test]
    fn ending_an_op_forgets_its_progress_for_the_next_run() {
        let s = AppState::new();
        s.begin_op("Rebuilding the index");
        s.set_op_progress("Rebuilding the index", 50, 100);
        s.end_op("Rebuilding the index");
        s.begin_op("Rebuilding the index");
        assert_eq!(s.active_op_labels(), vec!["Rebuilding the index"]);
    }

    #[test]
    fn progress_reported_for_an_op_that_is_not_running_is_ignored() {
        let s = AppState::new();
        s.set_op_progress("Rebuilding the index", 5, 10);
        s.begin_op("Rebuilding the index");
        assert_eq!(s.active_op_labels(), vec!["Rebuilding the index"]);
    }

    #[test]
    fn a_background_task_is_listed_but_does_not_count_as_an_active_op() {
        let s = AppState::new();
        s.begin_background_op("Running the wiki audit");
        assert_eq!(
            (s.active_ops(), s.active_op_labels()),
            (0, vec!["Running the wiki audit".to_string()])
        );
    }

    #[test]
    fn ending_a_background_task_removes_its_label() {
        let s = AppState::new();
        s.begin_background_op("Running the wiki audit");
        s.end_background_op("Running the wiki audit");
        assert!(s.active_op_labels().is_empty());
    }

    #[test]
    fn op_bookkeeping_from_two_threads_at_once_does_not_deadlock() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        let s = Arc::new(AppState::new());
        let deadline = Instant::now() + Duration::from_millis(200);
        let spawn = |label: &'static str| {
            let s = s.clone();
            std::thread::spawn(move || {
                while Instant::now() < deadline {
                    s.begin_op(label);
                    s.set_op_progress(label, 1, 2);
                    let _ = s.active_op_labels();
                    s.end_op(label);
                    let _ = s.active_op_labels();
                }
            })
        };
        let workers = [spawn("a"), spawn("b")];
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for w in workers {
                let _ = w.join();
            }
            let _ = tx.send(());
        });
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok(), "op bookkeeping deadlocked");
    }

    #[test]
    fn set_audit_task_aborts_the_previous_scheduler() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(dropped.clone());
        let s = AppState::new();
        s.set_audit_task(Some(AuditHandle::from_future(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        })));
        s.set_audit_task(Some(AuditHandle::from_future(std::future::pending::<()>())));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !dropped.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(dropped.load(Ordering::SeqCst));
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
