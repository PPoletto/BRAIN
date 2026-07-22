//! Per-machine storage of the vault master key in the OS keychain.
//!
//! The master key's authoritative copy lives in the user's password
//! manager; this module caches it in the platform credential store
//! (macOS Keychain, Windows Credential Manager, Linux Secret Service)
//! so the git clean/smudge filter and the app can retrieve it without
//! re-prompting. The key is stored as its 64-char hex recovery string
//! (see [`super::MasterKey::to_hex`]).
//!
//! **Keyed by the vault's stable `vault_id`, not its filesystem path.**
//! The same portable vault mounts at different paths (`E:\` today,
//! `F:\` tomorrow, a clone dir on another machine) and the git filter
//! derives the vault from git's cwd in canonical form — so a path-based
//! account misses the entry whenever the spelling differs. The
//! `vault_id` (a ULID minted once at init, stored in the marker) is the
//! one identifier that is identical across every mount and machine.
//! [`vault_account`] resolves a vault path to that account string.
//!
//! Access goes through the [`MasterKeyStore`] trait — pure account
//! string in, hex string out — so the key logic (hex round-trip,
//! missing-vs-malformed handling) is unit-testable against an in-memory
//! store, while the real OS boundary ([`KeyringStore`]) stays a thin,
//! manually-verified wrapper. (The `keyring` mock backend builds a fresh
//! credential per `Entry`, so it can't emulate the shared cross-`Entry`
//! persistence a round-trip needs — hence our own in-memory store for
//! tests.)

use std::path::Path;

use super::MasterKey;

/// Keychain service name for all BRAIN vault master keys. The vault's
/// `vault_id` is the per-entry account, so distinct vaults never collide.
const SERVICE: &str = "eu.poletto.brain.vault-master-key";

/// Keychain service for the git remote credential (a GitHub PAT or
/// equivalent), kept OUT of the repo and distinct from the master key so
/// a leaked PAT never exposes vault content and vice-versa. Keyed by the
/// same `vault_id` account.
const PAT_SERVICE: &str = "eu.poletto.brain.git-pat";

/// Store (or overwrite) the git remote credential for `account`
/// (a vault_id from [`vault_account`]).
pub fn store_git_pat(account: &str, pat: &str) -> Result<(), KeychainError> {
    keyring::Entry::new(PAT_SERVICE, account)?.set_password(pat)?;
    Ok(())
}

/// Load the git remote credential for `account`. `Ok(None)` when none is
/// stored (a vault with no configured remote credential on this machine).
pub fn load_git_pat(account: &str) -> Result<Option<String>, KeychainError> {
    match keyring::Entry::new(PAT_SERVICE, account)?.get_password() {
        Ok(pat) => Ok(Some(pat)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Remove the stored git remote credential for `account` (idempotent).
pub fn delete_git_pat(account: &str) -> Result<(), KeychainError> {
    match keyring::Entry::new(PAT_SERVICE, account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KeychainError {
    #[error("keychain: {0}")]
    Keyring(#[from] keyring::Error),
    #[error("stored master key is malformed (not a 64-char hex string)")]
    MalformedKey,
    #[error("vault has no marker file — cannot derive its keychain id")]
    NoMarker,
    #[error("reading the vault marker: {0}")]
    Marker(String),
}

/// Backend for persisting the master key's hex string under an opaque
/// account string. Real builds use [`KeyringStore`]; tests use an
/// in-memory implementation. Callers resolve the account via
/// [`vault_account`] — the store itself is path-agnostic.
pub trait MasterKeyStore {
    fn set_hex(&self, account: &str, hex: &str) -> Result<(), KeychainError>;
    fn get_hex(&self, account: &str) -> Result<Option<String>, KeychainError>;
    fn delete(&self, account: &str) -> Result<(), KeychainError>;
}

/// Production store backed by the OS keychain via the `keyring` crate.
pub struct KeyringStore;

impl KeyringStore {
    fn entry(account: &str) -> Result<keyring::Entry, KeychainError> {
        Ok(keyring::Entry::new(SERVICE, account)?)
    }
}

impl MasterKeyStore for KeyringStore {
    fn set_hex(&self, account: &str, hex: &str) -> Result<(), KeychainError> {
        Self::entry(account)?.set_password(hex)?;
        Ok(())
    }

    fn get_hex(&self, account: &str) -> Result<Option<String>, KeychainError> {
        match Self::entry(account)?.get_password() {
            Ok(hex) => Ok(Some(hex)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete(&self, account: &str) -> Result<(), KeychainError> {
        // Deletion is idempotent — a missing entry is success.
        match Self::entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Resolve a vault path to its keychain account string — the vault's
/// stable `vault_id`, read from `00_meta/brain-marker.json`. This is the
/// single place that binds "which credential" to "which vault", so the
/// app and the git filter always agree regardless of mount path.
pub fn vault_account(vault: &Path) -> Result<String, KeychainError> {
    match crate::vault::marker::read_marker(vault) {
        Ok(Some(marker)) => Ok(marker.vault_id),
        Ok(None) => Err(KeychainError::NoMarker),
        Err(e) => Err(KeychainError::Marker(e.to_string())),
    }
}

/// Store (or overwrite) the master key under `account`.
pub fn store_master_key(
    store: &impl MasterKeyStore,
    account: &str,
    key: &MasterKey,
) -> Result<(), KeychainError> {
    store.set_hex(account, &key.to_hex())
}

/// Load the master key for `account`. `Ok(None)` when none is stored yet
/// (a vault not unlocked on this machine); `Err(MalformedKey)` if the
/// stored value isn't a valid 64-char hex key; other `Err` on a real
/// keychain failure.
pub fn load_master_key(
    store: &impl MasterKeyStore,
    account: &str,
) -> Result<Option<MasterKey>, KeychainError> {
    match store.get_hex(account)? {
        Some(hex) => MasterKey::from_hex(&hex)
            .map(Some)
            .ok_or(KeychainError::MalformedKey),
        None => Ok(None),
    }
}

/// Remove the stored master key for `account` (e.g. on vault reset).
pub fn delete_master_key(
    store: &impl MasterKeyStore,
    account: &str,
) -> Result<(), KeychainError> {
    store.delete(account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::layout::ensure_skeleton;
    use crate::vault::marker::{write_marker, VaultMarker};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// In-memory store with real cross-call persistence, keyed by the
    /// account string — the shared-store semantics the OS keychain has
    /// but the `keyring` mock does not.
    #[derive(Default)]
    struct MemStore(Mutex<HashMap<String, String>>);

    impl MasterKeyStore for MemStore {
        fn set_hex(&self, account: &str, hex: &str) -> Result<(), KeychainError> {
            self.0.lock().unwrap().insert(account.to_string(), hex.to_string());
            Ok(())
        }
        fn get_hex(&self, account: &str) -> Result<Option<String>, KeychainError> {
            Ok(self.0.lock().unwrap().get(account).cloned())
        }
        fn delete(&self, account: &str) -> Result<(), KeychainError> {
            self.0.lock().unwrap().remove(account);
            Ok(())
        }
    }

    #[test]
    fn store_then_load_returns_the_same_key() {
        let store = MemStore::default();
        let key = MasterKey::from_bytes([42u8; 32]);
        store_master_key(&store, "01VAULT", &key).unwrap();
        let loaded = load_master_key(&store, "01VAULT").unwrap().expect("key present");
        assert_eq!(loaded.derive().content, key.derive().content);
    }

    #[test]
    fn load_of_an_unknown_vault_is_none_not_error() {
        let store = MemStore::default();
        assert!(load_master_key(&store, "no-such-vault").unwrap().is_none());
    }

    #[test]
    fn load_of_a_malformed_stored_value_is_a_clear_error() {
        let store = MemStore::default();
        store.set_hex("corrupt", "not-a-valid-hex-key").unwrap();
        assert!(matches!(
            load_master_key(&store, "corrupt"),
            Err(KeychainError::MalformedKey)
        ));
    }

    #[test]
    fn delete_removes_the_key_and_is_idempotent() {
        let store = MemStore::default();
        store_master_key(&store, "delete-me", &MasterKey::from_bytes([7u8; 32])).unwrap();
        assert!(load_master_key(&store, "delete-me").unwrap().is_some());
        delete_master_key(&store, "delete-me").unwrap();
        assert!(load_master_key(&store, "delete-me").unwrap().is_none());
        delete_master_key(&store, "delete-me").unwrap(); // idempotent
    }

    #[test]
    fn distinct_vaults_keep_distinct_keys() {
        let store = MemStore::default();
        store_master_key(&store, "a", &MasterKey::from_bytes([1u8; 32])).unwrap();
        store_master_key(&store, "b", &MasterKey::from_bytes([2u8; 32])).unwrap();
        assert_ne!(
            load_master_key(&store, "a").unwrap().unwrap().derive().content,
            load_master_key(&store, "b").unwrap().unwrap().derive().content,
        );
    }

    #[test]
    fn vault_account_returns_the_marker_vault_id() {
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        let marker = VaultMarker::new("0.0.0-test");
        write_marker(tmp.path(), &marker).unwrap();
        assert_eq!(vault_account(tmp.path()).unwrap(), marker.vault_id);
    }

    #[test]
    fn vault_account_is_stable_across_path_spelling() {
        // The bug this fixes: the same vault reached by a different path
        // string must resolve to the SAME account, so the key stored by
        // the app is found by the filter (which sees a canonical cwd).
        let tmp = TempDir::new().unwrap();
        ensure_skeleton(tmp.path()).unwrap();
        write_marker(tmp.path(), &VaultMarker::new("0.0.0-test")).unwrap();
        let via_plain = vault_account(tmp.path()).unwrap();
        let via_dot = vault_account(&tmp.path().join(".")).unwrap();
        assert_eq!(via_plain, via_dot, "account must not depend on path spelling");
    }

    #[test]
    fn vault_account_errors_when_marker_absent() {
        let tmp = TempDir::new().unwrap();
        assert!(matches!(vault_account(tmp.path()), Err(KeychainError::NoMarker)));
    }

    /// A KeyringStore can be constructed (real backend). We don't
    /// round-trip through the OS keychain in a unit test — that's a
    /// manual per-platform check — but this guards the type/trait wiring.
    #[test]
    fn keyring_store_constructs() {
        let _s = KeyringStore;
        let _: &dyn MasterKeyStore = &KeyringStore;
    }
}
