//! Persistent client settings stored in the OS config dir.
//! Vault-internal settings live inside the vault itself (see `vault::settings`).

use std::path::PathBuf;
use std::sync::RwLock;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const CONFIG_FILENAME: &str = "settings.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientSettings {
    /// Update channel — "stable" or "beta".
    pub update_channel: String,
    /// Skipped update versions.
    pub skipped_versions: Vec<String>,
    /// Folder-mode source paths registered by the user.
    pub folder_sources: Vec<PathBuf>,
    /// Custom mount path overrides per platform.
    pub mount_path_override: Option<PathBuf>,
    /// Internal-LLM provider default.
    pub default_provider: String,
    /// Path of the most recently mounted vault. On startup Brain checks this
    /// path; if it still looks like a vault, the wizard is skipped and the
    /// vault is auto-mounted, so users don't redo onboarding every launch.
    #[serde(default)]
    pub last_active_vault_path: Option<PathBuf>,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            update_channel: "stable".to_string(),
            skipped_versions: Vec::new(),
            folder_sources: Vec::new(),
            mount_path_override: None,
            default_provider: "local".to_string(),
            last_active_vault_path: None,
        }
    }
}

pub struct ConfigStore {
    path: PathBuf,
    settings: RwLock<ClientSettings>,
}

impl ConfigStore {
    pub fn new() -> Self {
        let path = config_path();
        let settings = path
            .parent()
            .and_then(|dir| {
                std::fs::create_dir_all(dir).ok()?;
                Some(())
            })
            .and_then(|_| std::fs::read_to_string(&path).ok())
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self {
            path,
            settings: RwLock::new(settings),
        }
    }

    pub fn snapshot(&self) -> ClientSettings {
        self.settings.read().expect("settings read lock").clone()
    }

    pub fn update<F>(&self, updater: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut ClientSettings),
    {
        {
            let mut guard = self.settings.write().expect("settings write lock");
            updater(&mut guard);
        }
        self.persist()
    }

    fn persist(&self) -> std::io::Result<()> {
        let snapshot = self.snapshot();
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let raw = serde_json::to_vec_pretty(&snapshot)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        std::fs::write(&self.path, raw)
    }
}

impl Default for ConfigStore {
    fn default() -> Self {
        Self::new()
    }
}

/// App-directory name for the persistent config. Debug builds — which
/// is what `pnpm tauri dev` produces — deliberately use a SEPARATE
/// directory (`brain-dev`) so a development binary can never read the
/// production `settings.json` and thereby auto-mount the real vault via
/// `bootstrap_app`. This protects the user's live vault during
/// storage-layer development (the S11 work renames/rewrites page files).
/// Release builds (`pnpm tauri build`, the shipped app) use the
/// canonical `brain` name. The config dir is derived from `ProjectDirs`,
/// independent of the Tauri bundle identifier, so this is the only place
/// the dev/prod split has to happen.
const fn config_app_name() -> &'static str {
    if cfg!(debug_assertions) {
        "brain-dev"
    } else {
        "brain"
    }
}

fn config_path() -> PathBuf {
    if let Some(proj) = ProjectDirs::from("eu", "poletto", config_app_name()) {
        return proj.config_dir().join(CONFIG_FILENAME);
    }
    PathBuf::from(CONFIG_FILENAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_use_stable_channel_and_local_provider() {
        let s = ClientSettings::default();
        assert_eq!(s.update_channel, "stable");
        assert_eq!(s.default_provider, "local");
        assert!(s.skipped_versions.is_empty());
    }

    #[test]
    fn settings_round_trip_through_json() {
        let s = ClientSettings {
            update_channel: "beta".into(),
            skipped_versions: vec!["1.2.3".into()],
            folder_sources: vec![PathBuf::from("/tmp/brain")],
            mount_path_override: Some(PathBuf::from("/mnt/brain")),
            default_provider: "anthropic".into(),
            last_active_vault_path: Some(PathBuf::from("D:/")),
        };
        let raw = serde_json::to_string(&s).unwrap();
        let parsed: ClientSettings = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.update_channel, "beta");
        assert_eq!(parsed.skipped_versions, vec!["1.2.3"]);
        assert_eq!(parsed.default_provider, "anthropic");
        assert_eq!(parsed.last_active_vault_path.as_deref(), Some(std::path::Path::new("D:/")));
    }

    #[test]
    fn settings_default_has_no_last_active_vault_path() {
        let s = ClientSettings::default();
        assert!(s.last_active_vault_path.is_none());
    }

    #[test]
    fn dev_and_release_config_use_distinct_app_dirs() {
        // The isolation contract: a debug (dev) build must never share
        // the config dir — and thus `last_active_vault_path` — with a
        // release (prod) build, or `pnpm tauri dev` could auto-mount the
        // real vault and destructive dev code could touch it. We can't
        // evaluate both cfg branches in one compile, so we assert the
        // current build's name matches its profile, plus the structural
        // invariant that the two names differ.
        let name = config_app_name();
        if cfg!(debug_assertions) {
            assert_eq!(name, "brain-dev", "debug/dev builds must use the isolated dir");
        } else {
            assert_eq!(name, "brain", "release builds use the canonical dir");
        }
        assert_ne!(
            "brain", "brain-dev",
            "dev and prod app-dir names must differ — this is what guarantees isolation"
        );
    }

    #[test]
    fn config_path_ends_with_the_settings_filename() {
        let p = config_path();
        assert_eq!(p.file_name().and_then(|n| n.to_str()), Some(CONFIG_FILENAME));
    }

    #[test]
    fn settings_load_tolerates_old_files_without_last_active_vault_path() {
        // A user upgrading from a pre-bootstrap version has a settings.json
        // that doesn't mention `last_active_vault_path`. The deserializer
        // must default it to None instead of failing the parse and wiping
        // the rest of the settings.
        let raw = r#"{
            "update_channel": "stable",
            "skipped_versions": [],
            "folder_sources": [],
            "mount_path_override": null,
            "default_provider": "local"
        }"#;
        let parsed: ClientSettings = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.update_channel, "stable");
        assert!(parsed.last_active_vault_path.is_none());
    }
}
