//! The Databento API key: the user's own (decision 0033), read when a command needs it, never
//! logged, printed, recorded or committed.
//!
//! Source order:
//! 1. `DATABENTO_API_KEY` (Databento's own convention): an override for this run.
//! 2. The OS keychain, entry [`CREDENTIAL`] (`lines-databento`; the key is its password, any
//!    user name): the Windows Credential Manager (a generic credential), the macOS Keychain (a
//!    generic password, service `lines-databento`), or the Secret Service on Linux (attribute
//!    `service` = `lines-databento`, through `secret-tool`).
//! 3. The file named by `LINES_DATABENTO_KEY_FILE` (the key alone, surrounding whitespace
//!    ignored; keep it outside any repository).
//!
//! With none, the source is "not set up" and makes no requests.

use anyhow::{Result, bail};

pub const CREDENTIAL: &str = "lines-databento";
pub const ENV: &str = "DATABENTO_API_KEY";
pub const FILE_ENV: &str = "LINES_DATABENTO_KEY_FILE";

/// The key. `Debug` and `Display` never show it.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: String) -> Result<Self> {
        let key = key.trim().to_string();
        // Databento keys are 32 printable ASCII characters starting with "db-".
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_graphic()) {
            bail!("the Databento API key is empty or has non-printable characters");
        }
        Ok(ApiKey(key))
    }

    pub(crate) fn secret(&self) -> &str {
        &self.0
    }

    /// The environment first (an override), then the OS keychain, then a key file.
    pub fn load() -> Result<Self> {
        if let Ok(k) = std::env::var(ENV)
            && !k.trim().is_empty()
        {
            return ApiKey::new(k);
        }
        match keychain::read(CREDENTIAL) {
            Ok(Some(k)) if !k.trim().is_empty() => return ApiKey::new(k),
            Ok(_) => {}
            // Never the key: the store and its error only.
            Err(e) => tracing::warn!("{} {CREDENTIAL:?}: {e}", keychain::NAME),
        }
        if let Ok(path) = std::env::var(FILE_ENV)
            && !path.trim().is_empty()
        {
            return ApiKey::from_file(std::path::Path::new(path.trim()));
        }
        bail!(
            "not set up: no Databento API key (add it to the {} as {CREDENTIAL:?}, set {ENV}, \
             or name a key file in {FILE_ENV})",
            keychain::NAME
        )
    }

    /// The key alone in a file (surrounding whitespace ignored). Errors name the file, never
    /// its content.
    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let k = std::fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!(
                "reading the key file {} (named by {FILE_ENV}): {e}",
                path.display()
            )
        })?;
        ApiKey::new(k)
    }

    /// Safe to print: the shape only.
    pub fn describe(&self) -> String {
        format!(
            "{} characters, starts with \"db-\": {}",
            self.0.len(),
            self.0.starts_with("db-")
        )
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// The OS keychain's entry `target`: its password, `Ok(None)` when there is none.
mod keychain {
    use anyhow::Result;

    #[cfg(windows)]
    pub const NAME: &str = "Windows Credential Manager";
    #[cfg(target_os = "macos")]
    pub const NAME: &str = "macOS Keychain";
    #[cfg(not(any(windows, target_os = "macos")))]
    pub const NAME: &str = "Secret Service keyring";

    #[cfg(windows)]
    pub fn read(target: &str) -> Result<Option<String>> {
        super::windows::read_generic_password(target)
    }

    /// `security find-generic-password -s <target> -w`: the password alone on stdout. Exit 44
    /// is "not found".
    #[cfg(target_os = "macos")]
    pub fn read(target: &str) -> Result<Option<String>> {
        let out = std::process::Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", target, "-w"])
            .stdin(std::process::Stdio::null())
            .output()?;
        match out.status.code() {
            Some(0) => Ok(Some(String::from_utf8(out.stdout)?)),
            Some(44) => Ok(None),
            code => anyhow::bail!("security exited with {code:?}"),
        }
    }

    /// `secret-tool lookup service <target>` (libsecret). Not installed, or nothing stored:
    /// no key.
    #[cfg(not(any(windows, target_os = "macos")))]
    pub fn read(target: &str) -> Result<Option<String>> {
        let out = match std::process::Command::new("secret-tool")
            .args(["lookup", "service", target])
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if !out.status.success() || out.stdout.is_empty() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8(out.stdout)?))
    }
}

#[cfg(windows)]
mod windows {
    use anyhow::{Result, bail};
    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
    use windows_sys::Win32::Security::Credentials::{
        CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW,
    };

    /// The password of a generic credential; `Ok(None)` if there is none.
    pub fn read_generic_password(target: &str) -> Result<Option<String>> {
        let name: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated UTF-16 that outlives the call; on success `cred`
        // points to a system allocation freed below with CredFree.
        let ok = unsafe { CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) };
        if ok == 0 {
            // SAFETY: no preconditions.
            let err = unsafe { GetLastError() };
            if err == ERROR_NOT_FOUND {
                return Ok(None);
            }
            bail!("CredReadW failed (error {err})");
        }
        // SAFETY: CredReadW succeeded: the blob pointer and size are valid until CredFree.
        let blob = unsafe {
            let c = &*cred;
            if c.CredentialBlob.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
            }
        };
        // SAFETY: freed exactly once.
        unsafe { CredFree(cred as *const _) };
        Ok(Some(decode_blob(&blob).unwrap_or_default()))
    }

    /// `cmdkey` stores UTF-16LE; other tools store UTF-8.
    fn decode_blob(blob: &[u8]) -> Option<String> {
        if blob.len() >= 2
            && blob.len().is_multiple_of(2)
            && blob.iter().skip(1).step_by(2).all(|&b| b == 0)
        {
            let units: Vec<u16> = blob
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            return String::from_utf16(&units).ok();
        }
        String::from_utf8(blob.to_vec()).ok()
    }

    #[cfg(test)]
    #[test]
    fn blob_encodings() {
        assert_eq!(decode_blob(b"d\0b\0").as_deref(), Some("db"));
        assert_eq!(decode_blob(b"db").as_deref(), Some("db"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_never_shows() {
        let k = ApiKey::new("db-SECRETSECRETSECRETSECRETSECRE".into()).unwrap();
        assert!(!format!("{k:?}").contains("SECRET"));
        assert!(!k.describe().contains("SECRET"));
        assert!(ApiKey::new("  ".into()).is_err());
        assert!(ApiKey::new("db-abc\u{7}".into()).is_err());
    }

    #[test]
    fn a_key_file_is_read_trimmed_and_errors_never_show_it() {
        let dir = std::env::temp_dir().join(format!("lines-databento-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("key.txt");
        std::fs::write(&p, "  db-FILEKEYFILEKEYFILEKEYFILEKEYF\r\n").unwrap();
        let k = ApiKey::from_file(&p).unwrap();
        assert_eq!(k.secret(), "db-FILEKEYFILEKEYFILEKEYFILEKEYF");
        let missing = ApiKey::from_file(&dir.join("none.txt")).unwrap_err();
        assert!(format!("{missing:#}").contains("none.txt"));
        std::fs::write(&p, "\n").unwrap();
        assert!(ApiKey::from_file(&p).is_err(), "an empty file is no key");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
