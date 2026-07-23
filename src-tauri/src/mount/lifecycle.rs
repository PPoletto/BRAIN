//! Mount/unmount lifecycle: validates a source, transitions app-state, and
//! manages the unclean-shutdown marker.

use std::path::{Path, PathBuf};

use crate::state::{AppState, MountState};
use crate::vault::layout::{is_vault, meta_dir};

use super::{MountError, MountResult};

const UNCLEAN_FLAG: &str = "unclean-shutdown.flag";

/// Marker tracking unclean shutdowns. Set on force-eject or when the source
/// disappears mid-write; cleared on the next successful clean unmount.
pub struct UncleanFlag;

impl UncleanFlag {
    pub fn path(vault: &Path) -> PathBuf {
        meta_dir(vault).join(UNCLEAN_FLAG)
    }

    pub fn set(vault: &Path) -> std::io::Result<()> {
        let dir = meta_dir(vault);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(UNCLEAN_FLAG), b"1")
    }

    pub fn clear(vault: &Path) -> std::io::Result<()> {
        let p = Self::path(vault);
        if p.exists() {
            std::fs::remove_file(p)?;
        }
        Ok(())
    }

    pub fn is_set(vault: &Path) -> bool {
        Self::path(vault).exists()
    }
}

/// Mounts a source: validates the marker, sets state to MountedIdle.
pub fn mount_source(state: &AppState, path: &Path) -> MountResult<()> {
    if !is_vault(path) {
        return Err(MountError::Vault(crate::vault::VaultError::NotAVault {
            path: path.to_path_buf(),
        }));
    }
    if let Some(existing) = state.vault_path() {
        if existing != path {
            return Err(MountError::AlreadyMounted(existing.display().to_string()));
        }
    }
    state.set_vault_path(Some(path.to_path_buf()));
    state.set_mount(MountState::MountedIdle);
    Ok(())
}

/// Reaction to "the vault disappeared from disk while it was mounted",
/// typically caused by the user yanking the SSD without ejecting first.
/// Differs from `unmount`:
///
///  - Doesn't try to write the unclean-shutdown flag (the disk is gone,
///    the write would fail with EIO or NotFound).
///  - Doesn't unregister MCP from the LLM clients (the MCP entry stays
///    valid and self-heals on the next mount, so leaving it avoids a
///    re-registration round-trip when the user just briefly bumped the
///    cable).
///  - Doesn't clear `last_active_vault_path` from persistent config —
///    the auto-reconnect path (see `try_auto_reconnect`) needs it to
///    know where to probe for a returning disk.
///  - Closes the DB handle so the next read fails fast with a clear
///    error rather than racing on stale page-cache.
///
/// Returns `true` when this call actually transitioned the state (i.e.
/// we were mounted and detected the disappearance), `false` if there
/// was nothing to do. The polling caller uses the bool to decide
/// whether to log + emit a notification.
pub fn handle_vault_disappearance(state: &AppState) -> bool {
    let Some(vault) = state.vault_path() else {
        return false;
    };
    if crate::vault::layout::is_vault(&vault) {
        return false;
    }
    state.set_db(None);
    state.set_mount(MountState::Disconnected);
    state.set_vault_path(None);
    true
}

/// Reverse of `handle_vault_disappearance`: when the state is currently
/// `Disconnected` and we have a remembered `last_active_vault_path`,
/// probe whether that path is a valid vault again. If it is — the user
/// just plugged the SSD back in — re-mount and re-open the DB so the
/// app picks up where it left off.
///
/// Returns `Some(vault_path)` when this call actually re-mounted (so
/// the caller can spawn the watcher, emit a `mount-state` event, etc.),
/// `None` if nothing changed.
pub fn try_auto_reconnect(state: &AppState) -> Option<std::path::PathBuf> {
    // Only auto-reconnect from a clean Disconnected state; anything
    // else means we're already mounted, mid-mount, or in error.
    if state.mount() != MountState::Disconnected {
        return None;
    }
    let path = state.config.snapshot().last_active_vault_path?;
    if !crate::vault::layout::is_vault(&path) {
        return None;
    }
    if mount_source(state, &path).is_err() {
        return None;
    }
    Some(path)
}

/// Cleanly unmounts the current source. If `force` is true and an operation
/// is still active, the unclean flag is set so the next mount can offer a
/// recovery flow (S07).
pub fn unmount(state: &AppState, force: bool) -> MountResult<()> {
    let Some(vault) = state.vault_path() else {
        return Err(MountError::NotMounted);
    };

    let busy = state.active_ops() > 0;
    if busy && !force {
        return Err(MountError::Io(std::io::Error::other(
            "active operations in progress; refuse to unmount",
        )));
    }

    if busy && force {
        UncleanFlag::set(&vault).ok();
    } else {
        UncleanFlag::clear(&vault).ok();
    }

    state.set_vault_path(None);
    state.set_mount(MountState::Disconnected);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::ensure_skeleton;
    use crate::vault::marker::{write_marker, VaultMarker};
    use tempfile::TempDir;

    fn prepare_vault(tmp: &TempDir) -> &Path {
        ensure_skeleton(tmp.path()).unwrap();
        write_marker(tmp.path(), &VaultMarker::new("0.1.0")).unwrap();
        tmp.path()
    }

    #[test]
    fn mount_source_refuses_paths_without_a_vault_marker() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let state = AppState::new();
        let err = mount_source(&state, tmp.path()).unwrap_err();
        assert!(matches!(err, MountError::Vault(_)));
        assert_eq!(state.mount(), MountState::Disconnected);
    }

    #[test]
    fn mount_source_succeeds_when_marker_is_present_and_sets_state_idle() {
        let tmp = TempDir::new().unwrap();
        let path = prepare_vault(&tmp);
        let state = AppState::new();
        mount_source(&state, path).unwrap();
        assert_eq!(state.mount(), MountState::MountedIdle);
        assert_eq!(state.vault_path().as_deref(), Some(path));
    }

    #[test]
    fn unmount_clears_state_and_removes_unclean_flag_on_clean_path() {
        let tmp = TempDir::new().unwrap();
        let path = prepare_vault(&tmp);
        let state = AppState::new();
        mount_source(&state, path).unwrap();
        UncleanFlag::set(path).unwrap();
        unmount(&state, false).unwrap();
        assert_eq!(state.mount(), MountState::Disconnected);
        assert!(!UncleanFlag::is_set(path));
    }

    #[test]
    fn unmount_force_with_active_ops_sets_unclean_flag() {
        let tmp = TempDir::new().unwrap();
        let path = prepare_vault(&tmp);
        let state = AppState::new();
        mount_source(&state, path).unwrap();
        state.begin_op("test");
        unmount(&state, true).unwrap();
        assert!(UncleanFlag::is_set(path));
    }

    #[test]
    fn unmount_without_force_refuses_when_ops_are_active() {
        let tmp = TempDir::new().unwrap();
        let path = prepare_vault(&tmp);
        let state = AppState::new();
        mount_source(&state, path).unwrap();
        state.begin_op("test");
        let err = unmount(&state, false).unwrap_err();
        assert!(matches!(err, MountError::Io(_)));
        assert_eq!(state.mount(), MountState::MountedIdle);
    }

    #[test]
    fn unmount_when_not_mounted_returns_not_mounted_error() {
        let state = AppState::new();
        let err = unmount(&state, false).unwrap_err();
        assert!(matches!(err, MountError::NotMounted));
    }

    #[test]
    fn handle_vault_disappearance_returns_false_when_vault_still_present() {
        let tmp = TempDir::new().unwrap();
        let path = prepare_vault(&tmp);
        let state = AppState::new();
        mount_source(&state, path).unwrap();
        // Disk still there → no transition, nothing to do.
        assert!(!handle_vault_disappearance(&state));
        assert_eq!(state.mount(), MountState::MountedIdle);
        assert_eq!(state.vault_path().as_deref(), Some(path));
    }

    #[test]
    fn handle_vault_disappearance_transitions_to_disconnected_when_marker_gone() {
        // Simulates "user pulled the SSD" by deleting the marker file
        // while the state still claims MountedIdle. The helper must
        // detect the divergence and reset the in-memory state.
        let tmp = TempDir::new().unwrap();
        let path = prepare_vault(&tmp);
        let state = AppState::new();
        mount_source(&state, path).unwrap();
        // Delete the marker — same end-state as a yanked drive from
        // the perspective of `is_vault`.
        std::fs::remove_file(
            crate::vault::layout::meta_dir(path)
                .join(crate::vault::layout::BRAIN_MARKER_FILENAME),
        )
        .unwrap();
        assert!(handle_vault_disappearance(&state));
        assert_eq!(state.mount(), MountState::Disconnected);
        assert!(state.vault_path().is_none());
    }

    #[test]
    fn handle_vault_disappearance_returns_false_when_no_vault_was_mounted() {
        let state = AppState::new();
        assert!(!handle_vault_disappearance(&state));
    }

    #[test]
    fn try_auto_reconnect_remounts_when_disk_reappears_with_remembered_path() {
        let tmp = TempDir::new().unwrap();
        let path = prepare_vault(&tmp);
        let state = AppState::new();
        // Simulate prior session: vault was mounted, the persisted
        // config remembered the path, then the disk vanished.
        let _ = state.config.update(|s| {
            s.last_active_vault_path = Some(path.to_path_buf());
        });
        // Disconnected state with no vault_path is the resting state
        // after `handle_vault_disappearance`.
        state.set_mount(MountState::Disconnected);
        assert!(state.vault_path().is_none());

        // Disk is back online (the marker is still on disk because we
        // never removed it for this test).
        let result = try_auto_reconnect(&state);
        assert_eq!(result, Some(path.to_path_buf()));
        assert_eq!(state.mount(), MountState::MountedIdle);
        assert_eq!(state.vault_path().as_deref(), Some(path));
    }

    #[test]
    fn try_auto_reconnect_does_nothing_when_state_is_already_mounted() {
        let tmp = TempDir::new().unwrap();
        let path = prepare_vault(&tmp);
        let state = AppState::new();
        mount_source(&state, path).unwrap();
        // Already mounted → must not double-mount.
        assert!(try_auto_reconnect(&state).is_none());
    }

    #[test]
    fn try_auto_reconnect_does_nothing_when_remembered_path_is_not_a_vault() {
        let tmp = TempDir::new().unwrap();
        // Remember a path that doesn't have a marker file.
        let state = AppState::new();
        let _ = state.config.update(|s| {
            s.last_active_vault_path = Some(tmp.path().to_path_buf());
        });
        state.set_mount(MountState::Disconnected);
        assert!(try_auto_reconnect(&state).is_none());
        assert_eq!(state.mount(), MountState::Disconnected);
    }

    #[test]
    fn try_auto_reconnect_does_nothing_when_no_remembered_path() {
        let state = AppState::new();
        state.set_mount(MountState::Disconnected);
        assert!(try_auto_reconnect(&state).is_none());
    }
}
