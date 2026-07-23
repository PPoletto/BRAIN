//! Git clean/smudge filter wiring for encrypted vaults.
//!
//! Two responsibilities:
//!  1. **Configure** a wiki repo so git runs `brain git-filter` as the
//!     `brain-crypt` clean/smudge filter for every `*.md` file, and
//!     store the canary that lets a fresh clone validate the key.
//!  2. **Run** the filter (called by the `brain git-filter` subcommand):
//!     resolve the vault, load the key, transform stdin→stdout.
//!
//! The actual crypto is in the parent module ([`super::filter_clean`] /
//! [`super::filter_smudge`]); this file is the git + filesystem glue.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::keychain::{self, KeyringStore};
use super::{DerivedKeys, MasterKey};

/// Filter name used in `.gitattributes` and the `filter.<name>.*` config.
pub const FILTER_NAME: &str = "brain-crypt";
/// Encrypted canary blob, stored in the wiki repo so a clone can
/// validate the entered key before mounting. Not `*.md`, so the filter
/// itself never touches it — we write the ciphertext bytes directly.
pub const CANARY_FILENAME: &str = ".brain-canary";

/// Lines written to the wiki repo's `.gitattributes`.
///
/// `-text` is load-bearing, not cosmetic: git's check-in pipeline runs
/// the `clean` filter FIRST and only then applies eol/`ident`
/// normalization, and the check-out pipeline applies eol normalization
/// BEFORE `smudge`. On a Windows clone (`core.autocrlf=true`) git would
/// otherwise CRLF-mangle the opaque ciphertext whenever its binary
/// auto-detection guessed "text" — corrupting the committed blob or
/// breaking decryption on checkout. `-text` disables all eol/`ident`
/// conversion on these paths so the filter's bytes pass through
/// untouched. The canary is raw ciphertext we write directly (the
/// filter never sees it), so it needs the same protection.
const GITATTRIBUTES_LINES: &[&str] = &["*.md filter=brain-crypt -text", ".brain-canary -text"];

#[derive(Debug, thiserror::Error)]
pub enum GitFilterError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("git: {0}")]
    Git(#[from] git2::Error),
}

/// Point the wiki repo at the brain-crypt filter: write (or extend)
/// `.gitattributes` so `*.md` is filtered, and set the local repo
/// config `filter.brain-crypt.{clean,smudge,required}` to invoke
/// `brain_exe git-filter …`. `required=true` makes git fail rather
/// than silently commit plaintext if the filter ever can't run — a
/// safety property for a vault whose whole point is encryption at rest.
///
/// Idempotent: re-running does not duplicate the `.gitattributes` line
/// and just re-sets the config.
pub fn configure_repo_filter(wiki_path: &Path, brain_exe: &Path) -> Result<(), GitFilterError> {
    // .gitattributes — append each required line only if not already
    // present, preserving any lines already in the file.
    let attrs_path = wiki_path.join(".gitattributes");
    let existing = std::fs::read_to_string(&attrs_path).unwrap_or_default();
    let mut updated = existing.clone();
    for line in GITATTRIBUTES_LINES {
        if !existing.lines().any(|l| l.trim() == *line) {
            if !updated.is_empty() && !updated.ends_with('\n') {
                updated.push('\n');
            }
            updated.push_str(line);
            updated.push('\n');
        }
    }
    if updated != existing {
        std::fs::write(&attrs_path, updated)?;
    }

    // Local repo config. The command must quote the exe path (spaces on
    // Windows) and is invoked by git with the blob on stdin.
    let repo = git2::Repository::open(wiki_path)?;
    let mut cfg = repo.config()?;
    let exe = brain_exe.to_string_lossy();
    cfg.set_str(
        &format!("filter.{FILTER_NAME}.clean"),
        &format!("\"{exe}\" git-filter clean"),
    )?;
    cfg.set_str(
        &format!("filter.{FILTER_NAME}.smudge"),
        &format!("\"{exe}\" git-filter smudge"),
    )?;
    cfg.set_bool(&format!("filter.{FILTER_NAME}.required"), true)?;
    Ok(())
}

/// Undo [`configure_repo_filter`]: drop the brain-crypt lines from
/// `.gitattributes` (removing the file if nothing else remains) and delete
/// the `filter.brain-crypt.*` config. Used when disabling encryption.
pub fn remove_repo_filter(wiki_path: &Path) -> Result<(), GitFilterError> {
    let attrs_path = wiki_path.join(".gitattributes");
    if let Ok(existing) = std::fs::read_to_string(&attrs_path) {
        let kept: Vec<&str> = existing
            .lines()
            .filter(|l| !GITATTRIBUTES_LINES.contains(&l.trim()))
            .collect();
        if kept.iter().all(|l| l.trim().is_empty()) {
            let _ = std::fs::remove_file(&attrs_path);
        } else {
            std::fs::write(&attrs_path, format!("{}\n", kept.join("\n")))?;
        }
    }
    let repo = git2::Repository::open(wiki_path)?;
    let mut cfg = repo.config()?;
    // Ignore "not found" — removal is idempotent.
    let _ = cfg.remove(&format!("filter.{FILTER_NAME}.clean"));
    let _ = cfg.remove(&format!("filter.{FILTER_NAME}.smudge"));
    let _ = cfg.remove(&format!("filter.{FILTER_NAME}.required"));
    Ok(())
}

/// Write the encrypted canary into the wiki repo.
pub fn write_canary(wiki_path: &Path, keys: &DerivedKeys) -> Result<(), GitFilterError> {
    std::fs::write(wiki_path.join(CANARY_FILENAME), super::make_canary(keys))?;
    Ok(())
}

/// Validate a candidate key against the stored canary. `false` when the
/// canary is missing, unreadable, or the key is wrong — the caller
/// turns that into a clear "wrong passphrase" for the clone flow.
pub fn canary_matches(wiki_path: &Path, keys: &DerivedKeys) -> bool {
    match std::fs::read(wiki_path.join(CANARY_FILENAME)) {
        Ok(stored) => super::check_canary(keys, &stored),
        Err(_) => false,
    }
}

/// Resolve the vault root from a git filter invocation. Git runs
/// clean/smudge filters with the current directory set to the top of
/// the working tree — i.e. the wiki dir (`<vault>/02_wiki`) — so the
/// vault is its parent. Returns `None` if the cwd doesn't look like a
/// mounted vault's wiki dir.
pub fn vault_from_filter_cwd() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let vault = cwd.parent()?.to_path_buf();
    if crate::vault::layout::is_vault(&vault) {
        Some(vault)
    } else {
        None
    }
}

/// Filter mode parsed from the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterMode {
    Clean,
    Smudge,
}

impl FilterMode {
    pub fn parse(s: Option<&str>) -> Option<Self> {
        match s {
            Some("clean") => Some(Self::Clean),
            Some("smudge") => Some(Self::Smudge),
            _ => None,
        }
    }
}

/// Transform `input` for the given mode with the given keys. Pure —
/// the IO/vault/keychain resolution happens in [`run`]. Separated so
/// the transform is unit-testable without a process/stdin.
pub fn transform(mode: FilterMode, keys: &DerivedKeys, input: &[u8]) -> Result<Vec<u8>, String> {
    match mode {
        FilterMode::Clean => Ok(super::filter_clean(keys, input)),
        FilterMode::Smudge => super::filter_smudge(keys, input).map_err(|e| e.to_string()),
    }
}

/// Entry point for `brain git-filter <mode>`. Resolves the vault from
/// the cwd, loads the master key from the OS keychain, reads the whole
/// blob from stdin, transforms it, and writes the result to stdout.
/// Returns a process exit code: 0 on success, non-zero on any failure
/// (git then aborts the operation rather than committing plaintext or
/// checking out garbage). Reads the key via the real [`KeyringStore`].
pub fn run(mode_arg: Option<&str>) -> i32 {
    let Some(mode) = FilterMode::parse(mode_arg) else {
        eprintln!("brain git-filter: expected 'clean' or 'smudge'");
        return 2;
    };
    let Some(vault) = vault_from_filter_cwd() else {
        eprintln!("brain git-filter: not run from inside a BRAIN vault's wiki dir");
        return 3;
    };
    let account = match keychain::vault_account(&vault) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("brain git-filter: cannot identify vault: {e}");
            return 5;
        }
    };
    let key: MasterKey = match keychain::load_master_key(&KeyringStore, &account) {
        Ok(Some(k)) => k,
        Ok(None) => {
            eprintln!(
                "brain git-filter: no master key for this vault in the keychain — \
                 unlock the vault in BRAIN first"
            );
            return 4;
        }
        Err(e) => {
            eprintln!("brain git-filter: keychain error: {e}");
            return 5;
        }
    };
    let keys = key.derive();

    let mut input = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut input) {
        eprintln!("brain git-filter: reading stdin: {e}");
        return 6;
    }
    match transform(mode, &keys, &input) {
        Ok(output) => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            if lock.write_all(&output).and_then(|_| lock.flush()).is_err() {
                return 7;
            }
            0
        }
        Err(msg) => {
            eprintln!("brain git-filter {mode:?}: {msg}");
            8
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn keys() -> DerivedKeys {
        MasterKey::from_bytes([5u8; 32]).derive()
    }

    #[test]
    fn filter_mode_parses_clean_and_smudge_only() {
        assert_eq!(FilterMode::parse(Some("clean")), Some(FilterMode::Clean));
        assert_eq!(FilterMode::parse(Some("smudge")), Some(FilterMode::Smudge));
        assert_eq!(FilterMode::parse(Some("bogus")), None);
        assert_eq!(FilterMode::parse(None), None);
    }

    #[test]
    fn transform_clean_then_smudge_round_trips() {
        let k = keys();
        let pt = b"---\nid: entities/x\ntype: entity\n---\n\nbody\n";
        let cleaned = transform(FilterMode::Clean, &k, pt).unwrap();
        assert!(super::super::looks_encrypted(&cleaned));
        assert_eq!(transform(FilterMode::Smudge, &k, &cleaned).unwrap(), pt);
    }

    #[test]
    fn transform_smudge_errors_on_wrong_key() {
        let a = MasterKey::from_bytes([1u8; 32]).derive();
        let b = MasterKey::from_bytes([2u8; 32]).derive();
        let cleaned = transform(FilterMode::Clean, &a, b"x").unwrap();
        assert!(transform(FilterMode::Smudge, &b, &cleaned).is_err());
    }

    #[test]
    fn configure_repo_filter_writes_attributes_and_config_idempotently() {
        let tmp = TempDir::new().unwrap();
        let wiki = tmp.path();
        git2::Repository::init(wiki).unwrap();
        let exe = Path::new("/opt/brain/brain");

        configure_repo_filter(wiki, exe).unwrap();
        configure_repo_filter(wiki, exe).unwrap(); // idempotent

        let attrs = std::fs::read_to_string(wiki.join(".gitattributes")).unwrap();
        for line in GITATTRIBUTES_LINES {
            assert_eq!(
                attrs.lines().filter(|l| l.trim() == *line).count(),
                1,
                "attribute line {line:?} must appear exactly once"
            );
        }
        // `-text` is the property that keeps ciphertext from being
        // CRLF-mangled — assert it explicitly, not just by line match.
        assert!(
            attrs.lines().any(|l| l.contains("*.md") && l.contains("-text")),
            "*.md must be marked -text so git never eol-converts ciphertext"
        );
        let repo = git2::Repository::open(wiki).unwrap();
        let cfg = repo.config().unwrap();
        assert!(cfg
            .get_string("filter.brain-crypt.clean")
            .unwrap()
            .contains("git-filter clean"));
        assert!(cfg
            .get_string("filter.brain-crypt.smudge")
            .unwrap()
            .contains("git-filter smudge"));
        assert!(cfg.get_bool("filter.brain-crypt.required").unwrap());
    }

    #[test]
    fn configure_repo_filter_preserves_existing_gitattributes_lines() {
        let tmp = TempDir::new().unwrap();
        let wiki = tmp.path();
        git2::Repository::init(wiki).unwrap();
        std::fs::write(wiki.join(".gitattributes"), "*.png binary\n").unwrap();
        configure_repo_filter(wiki, Path::new("/opt/brain/brain")).unwrap();
        let attrs = std::fs::read_to_string(wiki.join(".gitattributes")).unwrap();
        assert!(attrs.contains("*.png binary"), "pre-existing line kept");
        for line in GITATTRIBUTES_LINES {
            assert!(attrs.contains(line), "filter line {line:?} added");
        }
    }

    #[test]
    fn canary_write_then_match_true_for_right_key_false_for_wrong() {
        let tmp = TempDir::new().unwrap();
        let wiki = tmp.path();
        let right = MasterKey::from_bytes([9u8; 32]).derive();
        let wrong = MasterKey::from_bytes([8u8; 32]).derive();
        write_canary(wiki, &right).unwrap();
        assert!(canary_matches(wiki, &right));
        assert!(!canary_matches(wiki, &wrong));
    }

    #[test]
    fn canary_matches_is_false_when_absent() {
        let tmp = TempDir::new().unwrap();
        assert!(!canary_matches(tmp.path(), &keys()));
    }
}
