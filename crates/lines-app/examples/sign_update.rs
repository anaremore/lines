//! Sign a release's executable for in-app updates (decision 0031) and write its `update.json`.
//! Run by the release workflow (CI), never by hand with a real key on a shared machine.
//!
//! ```text
//! UPDATE_SIGNING_KEY=<minisign secret key file contents> UPDATE_SIGNING_PASSWORD=<password> \
//!   cargo run --release -p lines-app --example sign_update -- \
//!     <exe> <version> <target> <url> <out dir>
//! ```
//!
//! Writes `<out dir>/update.json` and `<exe>.minisig`. The signature's trusted comment is
//! exactly what the app requires (`update::manifest::trusted_comment`):
//! `lines <version> public pre-release <target>`. The secret key is read from the environment
//! only and never written anywhere.

use std::io::Cursor;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

fn main() {
    if let Err(e) = run() {
        eprintln!("sign_update: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [exe, version, target, url, out] = args.as_slice() else {
        return Err("usage: sign_update <exe> <version> <target> <url> <out dir>".into());
    };
    let version = version.trim_start_matches('v');
    if version.split('.').count() != 3 || version.split('.').any(|p| p.parse::<u32>().is_err()) {
        return Err(format!("{version}: expected MAJOR.MINOR.PATCH"));
    }
    let key = std::env::var("UPDATE_SIGNING_KEY").map_err(|_| "UPDATE_SIGNING_KEY is not set")?;
    let password = std::env::var("UPDATE_SIGNING_PASSWORD").ok();
    let sk = minisign::SecretKeyBox::from_string(&key)
        .and_then(|b| b.into_secret_key(password))
        .map_err(|e| format!("the signing key couldn't be read: {e}"))?;
    let data = std::fs::read(exe).map_err(|e| format!("{exe}: {e}"))?;
    let comment = format!("lines {version} public pre-release {target}");
    let sig = minisign::sign(None, &sk, Cursor::new(&data), Some(&comment), None)
        .map_err(|e| format!("signing: {e}"))?
        .to_string();
    let sha: String = Sha256::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let sig_path = PathBuf::from(format!("{exe}.minisig"));
    std::fs::write(&sig_path, &sig).map_err(|e| format!("{}: {e}", sig_path.display()))?;
    let manifest = serde_json::json!({
        "version": version,
        "channel": "pre-release",
        "edition": "public",
        "files": [{
            "target": target,
            "url": url,
            "sha256": sha,
            "signature": sig,
        }],
    });
    let out = PathBuf::from(out).join("update.json");
    let text = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    std::fs::write(&out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    println!(
        "signed {exe} ({sha}) as \"{comment}\"; wrote {}",
        out.display()
    );
    Ok(())
}
