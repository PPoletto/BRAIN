//! Per-machine storage of the vault master key in the OS keychain.
//!
//! The master key's authoritative copy lives in the user's password
//! manager; this module caches it in the platform credential store
//! (macOS Keychain, Windows Credential Manager, Linux Secret Service)
//! so the git clean/smudge filter and the app can retrieve it without
//! re-prompting. The key is stored as its 64-char hex recovery string
//! (see [`super::MasterKey::to_hex`]), keyed by the vault's filesystem
//! path so multiple vaults on one machine keep separate keys.
//!
//! Access goes through the [`MasterKeyStore`] trait so the key logic
//! (hex round-trip, missing-vs-malformed handling) is unit-testable
//! against an in-memory store, while the real OS boundary
//! ([`KeyringStore`]) stays a thin, manually-verified wrapper. (The
//! `keyring` mock backend builds a fresh credential per `Entry`, so it
//! can't emulate the shared cross-`Entry` persistence a round-trip
//! needs — hence our own in-memory store for tests.)

use std::path::Path;

use super::MasterKey;

/// Keychain service name for all BRAIN vault master keys. The vault
/// path is the per-entry account, so distinct vaults never collide.
const SERVICE: &str = "eu.poletto.brain.vault-master-key";

#[derive(Debug, thiserror::Error)]
pub enum KeychainError {
    #[error("keychain: {0}")]
    Keyring(#[from] keyring::Error),
    #[error("stored master key is malformed (not a 64-char hex string)")]
    MalformedKey,
}

/// Backend for persisting the master key's hex string. Real builds use
/// [`KeyringStore`]; tests use an in-memory implementation.
pub trait MasterKeyStore {
    fn set_hex(&self, vault: &Path, hex: &str) -> Result<(), KeychainError>;
    fn get_hex(&self, vault: &Path) -> Result<Option<String>, KeychainError>;
    fn delete(&self, vault: &Path) -> Result<(), KeychainError>;
}

/// Production store backed by the OS keychain via the `keyring` crate.
pub struct KeyringStore;

impl KeyringStore {
    fn entry(vault: &Path) -> Result<keyring::Entry, KeychainError> {
        Ok(keyring::Entry::new(SERVICE, &vault.to_string_lossy())?)
    }
}

impl MasterKeyStore for KeyringStore {
    fn set_hex(&self, vault: &Path, hex: &str) -> Result<(), KeychainError> {
        Self::entry(vault)?.set_password(hex)?;
        Ok(())
    }

    fn get_hex(&self, vault: &Path) -> Result<Option<String>, KeychainError> {
        match Self::entry(vault)?.get_password() {
            Ok(hex) => Ok(Some(hex)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete(&self, vault: &Path) -> Result<(), KeychainError> {
        // Deletion is idempotent — a missing entry is success.
        match Self::entry(vault)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Store (or overwrite) the master key for `vault`.
pub fn store_master_key(
    store: &impl MasterKeyStore,
    vault: &Path,
    key: &MasterKey,
) -> Result<(), KeychainError> {
    store.set_hex(vault, &key.to_hex())
}

/// Load the master key for `vault`. `Ok(None)` when none is stored yet
/// (a vault not unlocked on this machine); `Err(MalformedKey)` if the
/// stored value isn't a valid 64-char hex key; other `Err` on a real
/// keychain failure.
pub fn load_master_key(
    store: &impl MasterKeyStore,
    vault: &Path,
) -> Result<Option<MasterKey>, KeychainError> {
    match store.get_hex(vault)? {
        Some(hex) => MasterKey::from_hex(&hex)
            .map(Some)
            .ok_or(KeychainError::MalformedKey),
        None => Ok(None),
    }
}

/// Remove the stored master key for `vault` (e.g. on vault reset).
pub fn delete_master_key(
    store: &impl MasterKeyStore,
    vault: &Path,
) -> Result<(), KeychainError> {
    store.delete(vault)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// In-memory store with real cross-call persistence, keyed by vault
    /// path — the shared-store semantics the OS keychain has but the
    /// `keyring` mock does not.
    #[derive(Default)]
    struct MemStore(Mutex<HashMap<String, String>>);

    impl MasterKeyStore for MemStore {
        fn set_hex(&self, vault: &Path, hex: &str) -> Result<(), KeychainError> {
            self.0
                .lock()
                .unwrap()
                .insert(vault.to_string_lossy().into(), hex.to_string());
            Ok(())
        }
        fn get_hex(&self, vault: &Path) -> Result<Option<String>, KeychainError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(&*vault.to_string_lossy())
                .cloned())
        }
        fn delete(&self, vault: &Path) -> Result<(), KeychainError> {
            self.0.lock().unwrap().remove(&*vault.to_string_lossy());
            Ok(())
        }
    }

    #[test]
    fn store_then_load_returns_the_same_key() {
        let store = MemStore::default();
        let vault = PathBuf::from("/vaults/store-load");
        let key = MasterKey::from_bytes([42u8; 32]);
        store_master_key(&store, &vault, &key).unwrap();
        let loaded = load_master_key(&store, &vault).unwrap().expect("key present");
        assert_eq!(loaded.derive().content, key.derive().content);
    }

    #[test]
    fn load_of_an_unknown_vault_is_none_not_error() {
        let store = MemStore::default();
        assert!(load_master_key(&store, Path::new("/vaults/none")).unwrap().is_none());
    }

    #[test]
    fn load_of_a_malformed_stored_value_is_a_clear_error() {
        let store = MemStore::default();
        let vault = PathBuf::from("/vaults/corrupt");
        store.set_hex(&vault, "not-a-valid-hex-key").unwrap();
        assert!(matches!(
            load_master_key(&store, &vault),
            Err(KeychainError::MalformedKey)
        ));
    }

    #[test]
    fn delete_removes_the_key_and_is_idempotent() {
        let store = MemStore::default();
        let vault = PathBuf::from("/vaults/delete-me");
        store_master_key(&store, &vault, &MasterKey::from_bytes([7u8; 32])).unwrap();
        assert!(load_master_key(&store, &vault).unwrap().is_some());
        delete_master_key(&store, &vault).unwrap();
        assert!(load_master_key(&store, &vault).unwrap().is_none());
        delete_master_key(&store, &vault).unwrap(); // idempotent
    }

    #[test]
    fn distinct_vaults_keep_distinct_keys() {
        let store = MemStore::default();
        let a = PathBuf::from("/vaults/a");
        let b = PathBuf::from("/vaults/b");
        store_master_key(&store, &a, &MasterKey::from_bytes([1u8; 32])).unwrap();
        store_master_key(&store, &b, &MasterKey::from_bytes([2u8; 32])).unwrap();
        assert_ne!(
            load_master_key(&store, &a).unwrap().unwrap().derive().content,
            load_master_key(&store, &b).unwrap().unwrap().derive().content,
        );
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
