//! Verifying a downloaded update before it can run (decision 0031 §3): its SHA-256 must
//! match the manifest's and its minisign (Ed25519) signature must verify against the public
//! key compiled into this build, with a trusted (signed) comment naming this very release
//! (`super::manifest::trusted_comment`). Anything failing is discarded.
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

/// The committed public key: the base64 line of the owner's `lines-update.pub`, key ID
/// `B11DD18F53C6E999` (`docs/release.md` "Update signing key"). The secret key is the owner's
/// alone and is never in a repository.
const COMMITTED_KEY: Option<&str> =
    Some("RWSZ6cZTj9EdseUxeD0r05R5bTJtFcgCpSlQoVC61g9ms1ts/9LnE+R2");

/// The committed key's ID, as minisign prints it (a rotation is checked against it).
#[cfg(test)]
const COMMITTED_KEY_ID: &str = "B11DD18F53C6E999";

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

/// `data` is the file the manifest describes (`sha256`, `signature`), signed by `key` for this
/// release (`expected`: [`super::manifest::trusted_comment`]).
pub fn verify(
    data: &[u8],
    sha256: &str,
    signature: &str,
    key: Option<&str>,
    expected: &str,
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
        .map_err(|e| Rejected::Signature(e.to_string()))?;
    // The signed comment names the release: a file signed for another version, edition,
    // channel or system isn't this update.
    let said = sig.trusted_comment().trim();
    if said != expected {
        return Err(Rejected::Signature(format!(
            "it was signed for \"{said}\", not \"{expected}\""
        )));
    }
    Ok(())
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

        /// The `.minisig` text for `data`, its trusted comment `comment`.
        pub fn sign_as(&self, data: &[u8], comment: &str) -> String {
            minisign::sign(
                None,
                &self.secret,
                std::io::Cursor::new(data),
                Some(comment),
                None,
            )
            .expect("sign")
            .to_string()
        }

        /// Signed for [`super::tests::RELEASE`].
        pub fn sign(&self, data: &[u8]) -> String {
            self.sign_as(data, super::tests::RELEASE)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::test_key::TestKey;
    use super::*;

    /// The release the tests' files are signed for.
    pub(crate) const RELEASE: &str = "lines 0.3.12 public pre-release x86_64-pc-windows-msvc";

    fn check(data: &[u8], sha: &str, sig: &str, key: Option<&str>) -> Result<(), Rejected> {
        verify(data, sha, sig, key, RELEASE)
    }

    #[test]
    fn a_signed_file_with_its_checksum_verifies() {
        let k = TestKey::new();
        let data = b"lines 0.3.12";
        let sig = k.sign(data);
        assert_eq!(
            check(data, &sha256_hex(data), &sig, Some(&k.public)),
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
            check(bad, &sha256_hex(data), &sig, Some(&k.public)),
            Err(Rejected::Checksum)
        );
        assert!(matches!(
            check(bad, &sha256_hex(bad), &sig, Some(&k.public)),
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
            check(data, &sha256_hex(data), &sig, Some(&ours.public)),
            Err(Rejected::Signature(_))
        ));
        assert!(matches!(
            check(
                data,
                &sha256_hex(data),
                "not a signature",
                Some(&ours.public)
            ),
            Err(Rejected::Signature(_))
        ));
    }

    /// A file the project signed for another release (an older version, another edition or
    /// system) is refused: the manifest isn't signed, the trusted comment is.
    #[test]
    fn a_signed_file_from_another_release_is_rejected() {
        let k = TestKey::new();
        let old = b"lines 0.3.5";
        let sig = k.sign_as(old, "lines 0.3.5 public pre-release x86_64-pc-windows-msvc");
        assert!(matches!(
            check(old, &sha256_hex(old), &sig, Some(&k.public)),
            Err(Rejected::Signature(_))
        ));
        let official = k.sign_as(
            old,
            "lines 0.3.12 official pre-release x86_64-pc-windows-msvc",
        );
        assert!(matches!(
            check(old, &sha256_hex(old), &official, Some(&k.public)),
            Err(Rejected::Signature(_))
        ));
    }

    /// Without a public key nothing verifies, so nothing updates.
    #[test]
    fn no_key_no_update() {
        let data = b"x";
        assert_eq!(
            check(data, &sha256_hex(data), "", None),
            Err(Rejected::NoKey)
        );
    }

    /// The committed key is the owner's (its ID as `docs/release.md` records it), parses as
    /// `verify` reads it, and refuses a file signed by any other key.
    #[test]
    fn the_committed_key_is_the_owners_and_refuses_other_signatures() {
        let key = COMMITTED_KEY.expect("the owner's public key is committed");
        assert!(
            minisign_verify::PublicKey::from_base64(key).is_ok(),
            "parses"
        );
        let boxed = minisign::PublicKey::from_base64(key)
            .unwrap()
            .to_box()
            .unwrap()
            .to_string();
        assert!(boxed.contains(COMMITTED_KEY_ID), "{boxed}");
        // A file signed by another key (this test's own) for this very release: refused.
        let other = TestKey::new();
        let data = b"lines 0.3.12";
        let sig = other.sign(data);
        assert!(matches!(
            check(data, &sha256_hex(data), &sig, Some(key)),
            Err(Rejected::Signature(_))
        ));
    }

    #[test]
    fn sha256_is_lower_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
