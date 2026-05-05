//! Skip-list logic — the user can mark a specific version as "do not offer
//! again". Future versions are still offered.

use crate::config::ConfigStore;

pub fn is_skipped(store: &ConfigStore, version: &str) -> bool {
    store
        .snapshot()
        .skipped_versions
        .iter()
        .any(|v| v == version)
}

pub fn skip(store: &ConfigStore, version: &str) -> std::io::Result<()> {
    store.update(|s| {
        if !s.skipped_versions.iter().any(|v| v == version) {
            s.skipped_versions.push(version.to_string());
        }
    })
}

pub fn unskip(store: &ConfigStore, version: &str) -> std::io::Result<()> {
    store.update(|s| s.skipped_versions.retain(|v| v != version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_then_is_skipped_returns_true_for_that_version() {
        let store = ConfigStore::default();
        skip(&store, "1.2.3").unwrap();
        assert!(is_skipped(&store, "1.2.3"));
        unskip(&store, "1.2.3").unwrap();
        assert!(!is_skipped(&store, "1.2.3"));
    }

    #[test]
    fn skipping_same_version_twice_is_idempotent() {
        let store = ConfigStore::default();
        skip(&store, "9.9.9").unwrap();
        skip(&store, "9.9.9").unwrap();
        let count = store
            .snapshot()
            .skipped_versions
            .iter()
            .filter(|v| *v == "9.9.9")
            .count();
        assert_eq!(count, 1);
        unskip(&store, "9.9.9").unwrap();
    }
}
