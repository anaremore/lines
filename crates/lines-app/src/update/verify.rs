//! Verifying a downloaded update before it can run (decision 0031 §3): its SHA-256 must
//! match the manifest's and its minisign (Ed25519) signature must verify against the public
//! key compiled into this build. Either failing discards the file.
//!
//! The real public key is the owner's ([`PUBLIC_KEY`], `docs/release.md`). Until it is set
//! this build has no key and never updates itself: no request, no download. Tests sign with a
//! key they generate at run time; no private key is ever in the repository.

use sha2::{Digest, Sha256};

/// The project's update-signing public key (minisign, base64 key line), committed by the
/// owner when the signing key exists (`docs/release.md` "Update signing key"). A build may
/// also take it from `LINES_UPDATE_PUBLIC_KEY` at compile time. `None`: this build never
/// updates itself.
pub const PUBLIC_KEY: Option<&str> = match option_env!("LINES_UPDATE_PUBLIC_KEY") {
    Some(k) => Some(k),
    None => COMMITTED_KEY,
};

/// The committed public key (none yet: the owner generates the key pair).
const COMMITTED_KEY: Option<&str> = None;

/// Why a download was discarded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// No public key in this build: it can't verify, so it doesn't update.
    NoKey,
    Checksum,
    Signature(String),
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejected::NoKey => f.write_str("this build has no update key"),
            Rejected::Checksum => f.write_str("its checksum didn't match"),
            Rejected::Signature(e) => write!(f, "its signature didn't verify ({e})"),
        }
    }
}

/// Lower-case hex SHA-256.
pub fn sha256_hex(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in d {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `data` is the file the manifest describes (`sha256`, `signature`), signed by `key`.
pub fn verify(
    data: &[u8],
    sha256: &str,
    signature: &str,
    key: Option<&str>,
) -> Result<(), Rejected> {
    let key = key.ok_or(Rejected::NoKey)?;
    if !sha256_hex(data).eq_ignore_ascii_case(sha256.trim()) {
        return Err(Rejected::Checksum);
    }
    let pk = minisign_verify::PublicKey::from_base64(key.trim())
        .or_else(|_| minisign_verify::PublicKey::decode(key.trim()))
        .map_err(|e| Rejected::Signature(format!("bad key: {e}")))?;
    let sig = minisign_verify::Signature::decode(signature)
        .map_err(|e| Rejected::Signature(e.to_string()))?;
    pk.verify(data, &sig, false)
        .map_err(|e| Rejected::Signature(e.to_string()))
}

#[cfg(test)]
pub(crate) mod test_key {
    //! A key pair generated per test run: it exists only in memory, never on disk or in git.
    pub struct TestKey {
        pub public: String,
        secret: minisign::SecretKey,
    }

    impl TestKey {
        pub fn new() -> TestKey {
            let kp = minisign::KeyPair::generate_unencrypted_keypair().expect("keypair");
            TestKey {
                public: kp.pk.to_base64(),
                secret: kp.sk,
            }
        }

        /// The `.minisig` text for `data`.
        pub fn sign(&self, data: &[u8]) -> String {
            minisign::sign(None, &self.secret, std::io::Cursor::new(data), None, None)
                .expect("sign")
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_key::TestKey;
    use super::*;

    #[test]
    fn a_signed_file_with_its_checksum_verifies() {
        let k = TestKey::new();
        let data = b"lines 0.3.12";
        let sig = k.sign(data);
        assert_eq!(
            verify(data, &sha256_hex(data), &sig, Some(&k.public)),
            Ok(())
        );
    }

    /// A file changed after signing fails its checksum; one whose checksum was changed to
    /// match still fails its signature.
    #[test]
    fn a_tampered_file_is_rejected() {
        let k = TestKey::new();
        let data = b"lines 0.3.12";
        let sig = k.sign(data);
        let bad = b"lines 0.3.12 + something";
        assert_eq!(
            verify(bad, &sha256_hex(data), &sig, Some(&k.public)),
            Err(Rejected::Checksum)
        );
        assert!(matches!(
            verify(bad, &sha256_hex(bad), &sig, Some(&k.public)),
            Err(Rejected::Signature(_))
        ));
    }

    #[test]
    fn a_signature_from_another_key_is_rejected() {
        let ours = TestKey::new();
        let theirs = TestKey::new();
        let data = b"lines 0.3.12";
        let sig = theirs.sign(data);
        assert!(matches!(
            verify(data, &sha256_hex(data), &sig, Some(&ours.public)),
            Err(Rejected::Signature(_))
        ));
        assert!(matches!(
            verify(
                data,
                &sha256_hex(data),
                "not a signature",
                Some(&ours.public)
            ),
            Err(Rejected::Signature(_))
        ));
    }

    /// Without a public key nothing verifies, so nothing updates.
    #[test]
    fn no_key_no_update() {
        let data = b"x";
        assert_eq!(
            verify(data, &sha256_hex(data), "", None),
            Err(Rejected::NoKey)
        );
        assert!(
            COMMITTED_KEY.is_none(),
            "the owner commits the real key; tests never put one here"
        );
    }

    #[test]
    fn sha256_is_lower_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
