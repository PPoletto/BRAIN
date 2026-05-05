//! Vault-internal settings (persisted inside `00_meta/settings-internal.json`).

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::layout::{meta_dir, VAULT_SETTINGS_FILENAME};
use super::VaultResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultSettings {
    pub embedding_model: String,
    pub host_label: Option<String>,
}

impl Default for VaultSettings {
    fn default() -> Self {
        Self {
            embedding_model: "bge-m3".to_string(),
            host_label: None,
        }
    }
}

pub fn read(vault: &Path) -> VaultResult<VaultSettings> {
    let path = meta_dir(vault).join(VAULT_SETTINGS_FILENAME);
    if !path.exists() {
        return Ok(VaultSettings::default());
    }
    let raw = std::fs::read_to_string(&path)?;
    let s: VaultSettings = serde_json::from_str(&raw)?;
    Ok(s)
}

pub fn write(vault: &Path, settings: &VaultSettings) -> VaultResult<()> {
    let dir = meta_dir(vault);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(VAULT_SETTINGS_FILENAME);
    let raw = serde_json::to_string_pretty(settings)?;
    std::fs::write(path, raw)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn reading_when_file_missing_returns_default_with_bge_m3() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(meta_dir(tmp.path())).unwrap();
        let s = read(tmp.path()).unwrap();
        assert_eq!(s.embedding_model, "bge-m3");
    }

    #[test]
    fn settings_round_trip_through_disk() {
        let tmp = TempDir::new().unwrap();
        let s = VaultSettings {
            embedding_model: "bge-m3".into(),
            host_label: Some("MacBook Pascal".into()),
        };
        write(tmp.path(), &s).unwrap();
        let parsed = read(tmp.path()).unwrap();
        assert_eq!(parsed.host_label.as_deref(), Some("MacBook Pascal"));
    }
}
