//! Minisign signature verification.
//!
//! The Brain build embeds the signing public key. Update bundles are only
//! installed when their `.minisig` signature verifies under this key. Failed
//! verification means: drop the bundle, log, leave the running version.

use minisign_verify::{PublicKey, Signature};

use super::{UpdateError, UpdateResult};

pub fn verify_bundle(public_key: &str, bundle: &[u8], signature: &str) -> UpdateResult<()> {
    let pk = PublicKey::decode(public_key).map_err(|err| {
        UpdateError::Network(format!("invalid embedded public key: {err}"))
    })?;
    let sig = Signature::decode(signature).map_err(|err| {
        UpdateError::Network(format!("invalid signature encoding: {err}"))
    })?;
    pk.verify(bundle, &sig, false)
        .map_err(|_| UpdateError::SignatureMismatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    // These keys/signatures are synthetic test fixtures generated for this
    // unit test only; they are not used to ship anything.
    const PK: &str = "RWQrPJqDGmgQ3iC++WRl9xEhx30CO0ehNG6PMqg9N2DTwFevlktO6tHV";
    const BAD_SIG: &str = "untrusted comment: this is not a valid signature\n+RNGAAAAAAAAAAAA\n";

    #[test]
    fn verify_bundle_returns_signature_mismatch_for_garbage_signature() {
        let bundle = b"hello world";
        // The signature parser is strict; we expect either a Network error
        // (parse rejection) or a SignatureMismatch (valid layout, wrong key).
        let err = verify_bundle(PK, bundle, BAD_SIG).unwrap_err();
        assert!(matches!(
            err,
            UpdateError::SignatureMismatch | UpdateError::Network(_)
        ));
    }

    #[test]
    fn verify_bundle_rejects_invalid_public_keys() {
        let err = verify_bundle("NOT_A_KEY", b"data", BAD_SIG).unwrap_err();
        assert!(matches!(err, UpdateError::Network(_)));
    }
}
