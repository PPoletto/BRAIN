//! Vault marker `00_meta/brain-marker.json`.
//!
//! Format follows architecture.md §2.2. The `encryption.scheme` field is a
//! placeholder for the deferred encryption phase and currently always `"none"`.

use std::path::Path;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use super::layout::{meta_dir, BRAIN_MARKER_FILENAME};
use super::{VaultError, VaultResult};

pub const BRAIN_FORMAT_V1: &str = "brain-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultMarker {
    pub format: String,
    pub vault_id: String,
    pub created_at: String,
    pub client_version: String,
    pub encryption: EncryptionPlaceholder,
    pub embedding_model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionPlaceholder {
    pub scheme: String,
    pub params: serde_json::Value,
}

impl Default for EncryptionPlaceholder {
    fn default() -> Self {
        Self {
            scheme: "none".to_string(),
            params: serde_json::json!({}),
        }
    }
}

impl VaultMarker {
    pub fn new(client_version: impl Into<String>) -> Self {
        Self {
            format: BRAIN_FORMAT_V1.to_string(),
            vault_id: Ulid::new().to_string(),
            created_at: Utc::now().to_rfc3339(),
            client_version: client_version.into(),
            encryption: EncryptionPlaceholder::default(),
            embedding_model: "bge-m3".to_string(),
        }
    }
}

pub fn read_marker(vault_path: &Path) -> VaultResult<Option<VaultMarker>> {
    let marker_path = meta_dir(vault_path).join(BRAIN_MARKER_FILENAME);
    if !marker_path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&marker_path)?;
    let parsed: VaultMarker = serde_json::from_str(&raw)?;
    if parsed.format != BRAIN_FORMAT_V1 {
        return Err(VaultError::UnsupportedFormat {
            expected: BRAIN_FORMAT_V1.to_string(),
            actual: parsed.format,
        });
    }
    Ok(Some(parsed))
}

pub fn write_marker(vault_path: &Path, marker: &VaultMarker) -> VaultResult<()> {
    let dir = meta_dir(vault_path);
    std::fs::create_dir_all(&dir)?;
    let marker_path = dir.join(BRAIN_MARKER_FILENAME);
    let raw = serde_json::to_string_pretty(marker)?;
    std::fs::write(marker_path, raw)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn marker_default_uses_brain_v1_format_with_none_encryption() {
        let m = VaultMarker::new("0.1.0");
        assert_eq!(m.format, BRAIN_FORMAT_V1);
        assert_eq!(m.encryption.scheme, "none");
        assert_eq!(m.embedding_model, "bge-m3");
        assert!(!m.vault_id.is_empty());
    }

    #[test]
    fn writing_and_reading_marker_round_trips_all_fields() {
        let tmp = TempDir::new().unwrap();
        let m = VaultMarker::new("0.1.0");
        write_marker(tmp.path(), &m).unwrap();
        let read = read_marker(tmp.path()).unwrap().unwrap();
        assert_eq!(read.format, m.format);
        assert_eq!(read.vault_id, m.vault_id);
        assert_eq!(read.encryption.scheme, "none");
    }

    #[test]
    fn read_marker_returns_none_when_marker_file_does_not_exist() {
        let tmp = TempDir::new().unwrap();
        assert!(read_marker(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn read_marker_rejects_unsupported_format_versions() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("00_meta")).unwrap();
        std::fs::write(
            tmp.path().join("00_meta").join(BRAIN_MARKER_FILENAME),
            r#"{
                "format": "brain-v999",
                "vault_id": "x",
                "created_at": "2026-04-29T00:00:00Z",
                "client_version": "0.0.0",
                "encryption": {"scheme":"none","params":{}},
                "embedding_model": "bge-m3"
            }"#,
        )
        .unwrap();
        let err = read_marker(tmp.path()).unwrap_err();
        assert!(matches!(err, VaultError::UnsupportedFormat { .. }));
    }
}
