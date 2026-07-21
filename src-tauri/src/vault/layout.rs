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

/// The on-disk path of a page, relative to `02_wiki/`, for a given
/// page id. THE single source of truth for "id → filename" — every
/// site that reads, writes, or resolves a page file must go through
/// here (or [`page_path_for_id`]) rather than hand-building
/// `format!("{id}.md")`, so the upcoming opaque/encrypted layout
/// (S11) can be introduced in exactly one place.
///
/// Plaintext layout (today, and the default): the relative path is
/// simply `<id>.md` (e.g. `entities/alice` → `entities/alice.md`).
/// The opaque layout (`<type>/<HMAC(id)>.md`) will slot in here once
/// the S11 key infrastructure exists; until then this is a pure
/// centralisation with no behaviour change.
pub fn page_relpath_for_id(id: &str) -> String {
    format!("{id}.md")
}

/// Absolute on-disk path of a page for a given id — `wiki_dir(vault)`
/// joined with [`page_relpath_for_id`]. Use this for filesystem reads
/// and writes; use `page_relpath_for_id` when you need the repo-
/// relative path (e.g. matching git diff deltas or reading a blob by
/// path).
pub fn page_path_for_id(vault: &Path, id: &str) -> PathBuf {
    wiki_dir(vault).join(page_relpath_for_id(id))
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
    fn page_relpath_for_id_is_the_plaintext_id_dot_md() {
        // Pins the plaintext-layout contract. When the opaque/HMAC
        // layout lands (S11), this test is the tripwire that forces a
        // conscious decision rather than a silent behaviour change.
        assert_eq!(page_relpath_for_id("entities/alice"), "entities/alice.md");
        assert_eq!(page_relpath_for_id("concepts/nl-spec"), "concepts/nl-spec.md");
    }

    #[test]
    fn page_path_for_id_is_wiki_dir_joined_with_relpath() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            page_path_for_id(tmp.path(), "entities/alice"),
            wiki_dir(tmp.path()).join("entities/alice.md"),
        );
    }

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
