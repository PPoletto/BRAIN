//! Vault initialization: directory skeleton + marker + template population.
//!
//! Idempotent: running on an existing vault only fills in missing pieces.

use std::path::Path;

use crate::vault::layout::ensure_skeleton;
use crate::vault::marker::{read_marker, write_marker, VaultMarker};
use crate::vault::VaultResult;

use super::template;
use super::OnboardingResult;

/// Crate version pulled in at compile time.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn initialize(vault: &Path) -> OnboardingResult<VaultMarker> {
    if !vault.exists() {
        std::fs::create_dir_all(vault)?;
    }
    ensure_skeleton(vault)?;

    let marker = match read_marker(vault)? {
        Some(existing) => existing,
        None => {
            let m = VaultMarker::new(CLIENT_VERSION);
            write_marker(vault, &m)?;
            m
        }
    };

    template::populate(vault)?;
    write_gitignore_if_missing(vault)?;
    Ok(marker)
}

fn write_gitignore_if_missing(vault: &Path) -> VaultResult<()> {
    let wiki = crate::vault::layout::wiki_dir(vault);
    let gitignore = wiki.join(".gitignore");
    if gitignore.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&wiki)?;
    let contents = "# Editor artifacts\n.DS_Store\nThumbs.db\n*~\n*.swp\n";
    std::fs::write(gitignore, contents)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::{is_vault, wiki_dir, BRAIN_MARKER_FILENAME};
    use tempfile::TempDir;

    #[test]
    fn initialize_creates_full_layout_in_a_fresh_directory() {
        let tmp = TempDir::new().unwrap();
        let m = initialize(tmp.path()).unwrap();
        assert!(is_vault(tmp.path()));
        assert_eq!(m.format, "brain-v1");
        assert!(tmp
            .path()
            .join("00_meta")
            .join(BRAIN_MARKER_FILENAME)
            .exists());
        assert!(wiki_dir(tmp.path()).join(".gitignore").exists());
    }

    #[test]
    fn initialize_is_idempotent_when_run_on_an_already_initialized_vault() {
        let tmp = TempDir::new().unwrap();
        let first = initialize(tmp.path()).unwrap();
        let second = initialize(tmp.path()).unwrap();
        assert_eq!(first.vault_id, second.vault_id);
    }

    #[test]
    fn initialize_does_not_overwrite_existing_user_files() {
        let tmp = TempDir::new().unwrap();
        initialize(tmp.path()).unwrap();
        let custom = wiki_dir(tmp.path()).join("entities").join("custom.md");
        std::fs::write(&custom, "user wrote this").unwrap();
        initialize(tmp.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&custom).unwrap(), "user wrote this");
    }
}
