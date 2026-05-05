//! Vault directory layout constants and idempotent skeleton creation.
//!
//! Layout per architecture.md §2.1.

use std::path::{Path, PathBuf};

use super::{VaultError, VaultResult};

pub const META_DIR: &str = "00_meta";
pub const RAW_DIR: &str = "01_raw";
pub const WIKI_DIR: &str = "02_wiki";
pub const DB_DIR: &str = "03_db";
pub const MODELS_DIR: &str = "04_models";
pub const CACHE_DIR: &str = "05_cache";
pub const LOGS_DIR: &str = "06_logs";

pub const BRAIN_MARKER_FILENAME: &str = "brain-marker.json";
pub const MCP_CONFIG_FILENAME: &str = ".mcp.json";
pub const AGENTS_FILENAME: &str = "AGENTS.md";
pub const CLAUDE_FILENAME: &str = "CLAUDE.md";
pub const VAULT_SETTINGS_FILENAME: &str = "settings-internal.json";

pub const WIKI_SUBDIRS: &[&str] = &["entities", "concepts", "sources", "topics"];
pub const RAW_SUBDIRS: &[&str] = &["email", "confluence", "notes"];

pub fn meta_dir(vault: &Path) -> PathBuf {
    vault.join(META_DIR)
}

pub fn wiki_dir(vault: &Path) -> PathBuf {
    vault.join(WIKI_DIR)
}

pub fn db_dir(vault: &Path) -> PathBuf {
    vault.join(DB_DIR)
}

pub fn raw_dir(vault: &Path) -> PathBuf {
    vault.join(RAW_DIR)
}

pub fn models_dir(vault: &Path) -> PathBuf {
    vault.join(MODELS_DIR)
}

pub fn logs_dir(vault: &Path) -> PathBuf {
    vault.join(LOGS_DIR)
}

pub fn cache_dir(vault: &Path) -> PathBuf {
    vault.join(CACHE_DIR)
}

/// Returns true if the path looks like a Brain vault (has the marker file).
pub fn is_vault(vault: &Path) -> bool {
    meta_dir(vault).join(BRAIN_MARKER_FILENAME).exists()
}

/// Creates the canonical directory skeleton if missing. Idempotent.
pub fn ensure_skeleton(vault: &Path) -> VaultResult<()> {
    if !vault.exists() {
        return Err(VaultError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("vault path does not exist: {}", vault.display()),
        )));
    }
    let top_dirs = [META_DIR, RAW_DIR, WIKI_DIR, DB_DIR, MODELS_DIR, CACHE_DIR, LOGS_DIR];
    for d in top_dirs {
        std::fs::create_dir_all(vault.join(d))?;
    }
    for sub in WIKI_SUBDIRS {
        std::fs::create_dir_all(wiki_dir(vault).join(sub))?;
    }
    for sub in RAW_SUBDIRS {
        std::fs::create_dir_all(raw_dir(vault).join(sub))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn ensure_skeleton_creates_all_top_dirs_and_wiki_subdirs() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        for d in [META_DIR, RAW_DIR, WIKI_DIR, DB_DIR, MODELS_DIR, CACHE_DIR, LOGS_DIR] {
            assert!(tmp.path().join(d).is_dir(), "missing top dir {d}");
        }
        for sub in WIKI_SUBDIRS {
            assert!(wiki_dir(tmp.path()).join(sub).is_dir(), "missing wiki sub {sub}");
        }
    }

    #[test]
    fn ensure_skeleton_is_idempotent_when_run_twice() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        // touch a file — must not be deleted on second run
        std::fs::write(wiki_dir(tmp.path()).join("entities").join("keep.md"), "hi").unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        assert!(wiki_dir(tmp.path()).join("entities").join("keep.md").exists());
    }

    #[test]
    fn is_vault_returns_false_for_directories_without_marker() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        assert!(!is_vault(tmp.path()));
    }

    #[test]
    fn ensure_skeleton_errors_when_target_path_is_missing() {
        let err = ensure_skeleton(Path::new("/no/such/dir/anywhere/here")).unwrap_err();
        assert!(matches!(err, VaultError::Io(_)));
    }
}
