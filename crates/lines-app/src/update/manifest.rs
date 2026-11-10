//! The release feed and update manifest (decision 0031), pure.
//!
//! The feed is the GitHub releases list of the public repository (one HTTPS request, no
//! identifiers). A release that can update the app carries two assets the release workflow
//! publishes: `update.json` (this [`Manifest`]) and the executable it names. The manifest
//! states the version, channel and edition, and per target the file's name, SHA-256 and
//! minisign signature. Nothing here touches the network or the disk.

use serde::Deserialize;

/// The edition this build updates within (decision 0031 §7): public builds update public.
pub const EDITION: &str = "public";
/// The channel this build follows: every release is a pre-release for now.
pub const CHANNEL: &str = "pre-release";
/// The manifest's asset name on a release.
pub const MANIFEST_ASSET: &str = "update.json";

/// This build's target, as the manifest names it.
pub fn target() -> &'static str {
    if cfg!(all(windows, target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else {
        "unsupported"
    }
}

/// A version as `MAJOR.MINOR.PATCH` (a leading `v` allowed; pre-release suffixes refused, as
/// the project doesn't use them).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut it = s.split('.');
        let v = Version(
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
            it.next()?.parse().ok()?,
        );
        it.next().is_none().then_some(v)
    }

    /// This build's version.
    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is MAJOR.MINOR.PATCH")
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// One release in the feed, as far as the updater reads it.
#[derive(Clone, Debug, Deserialize)]
pub struct FeedRelease {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<FeedAsset>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FeedAsset {
    pub name: String,
    pub browser_download_url: String,
}

/// The newest release in `feed` newer than `current` that has a manifest: its version and
/// the manifest's URL. Drafts and tags that aren't versions are skipped.
pub fn newest(feed: &[FeedRelease], current: Version) -> Option<(Version, String)> {
    feed.iter()
        .filter(|r| !r.draft)
        .filter_map(|r| {
            let v = Version::parse(&r.tag_name)?;
            let m = r.assets.iter().find(|a| a.name == MANIFEST_ASSET)?;
            Some((v, m.browser_download_url.clone()))
        })
        .filter(|(v, _)| *v > current)
        .max_by_key(|(v, _)| *v)
}

/// A release's update manifest (`update.json`).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Manifest {
    pub version: String,
    pub channel: String,
    pub edition: String,
    pub files: Vec<ManifestFile>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ManifestFile {
    pub target: String,
    /// The executable's download URL.
    pub url: String,
    /// Lower-case hex SHA-256 of the file.
    pub sha256: String,
    /// The file's minisign signature (the `.minisig` file's text).
    pub signature: String,
}

/// Why a manifest can't update this build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    Malformed(String),
    /// Another channel or edition (decision 0031 §7).
    NotOurs(String),
    /// Not newer than this build (or not the version the feed said).
    NotNewer,
    NoFileForTarget,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Malformed(e) => write!(f, "the update manifest couldn't be read ({e})"),
            Refused::NotOurs(what) => write!(f, "the update is for another {what}"),
            Refused::NotNewer => f.write_str("the update isn't newer than this version"),
            Refused::NoFileForTarget => f.write_str("the update has no file for this system"),
        }
    }
}

/// The file this build should download from `manifest`, which the feed listed as `expected`.
pub fn pick(
    manifest: &Manifest,
    expected: Version,
    current: Version,
    target: &str,
) -> Result<(Version, ManifestFile), Refused> {
    let v = Version::parse(&manifest.version)
        .ok_or_else(|| Refused::Malformed(format!("version {:?}", manifest.version)))?;
    if manifest.edition != EDITION {
        return Err(Refused::NotOurs("edition".into()));
    }
    if manifest.channel != CHANNEL {
        return Err(Refused::NotOurs("channel".into()));
    }
    if v <= current || v != expected {
        return Err(Refused::NotNewer);
    }
    let f = manifest
        .files
        .iter()
        .find(|f| f.target == target)
        .ok_or(Refused::NoFileForTarget)?;
    Ok((v, f.clone()))
}

pub fn parse_feed(bytes: &[u8]) -> Result<Vec<FeedRelease>, String> {
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, Refused> {
    serde_json::from_slice(bytes).map_err(|e| Refused::Malformed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "x86_64-pc-windows-msvc";

    fn feed(tags: &[(&str, bool, bool)]) -> Vec<FeedRelease> {
        tags.iter()
            .map(|(tag, draft, manifest)| FeedRelease {
                tag_name: (*tag).into(),
                draft: *draft,
                assets: if *manifest {
                    vec![FeedAsset {
                        name: MANIFEST_ASSET.into(),
                        browser_download_url: format!("https://example.test/{tag}/update.json"),
                    }]
                } else {
                    vec![]
                },
            })
            .collect()
    }

    fn manifest(version: &str) -> Manifest {
        Manifest {
            version: version.into(),
            channel: CHANNEL.into(),
            edition: EDITION.into(),
            files: vec![ManifestFile {
                target: T.into(),
                url: "https://example.test/lines.exe".into(),
                sha256: "00".into(),
                signature: "sig".into(),
            }],
        }
    }

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(Version::parse("v0.3.12"), Some(Version(0, 3, 12)));
        assert_eq!(Version::parse("1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("0.3"), None);
        assert_eq!(Version::parse("0.3.1-rc1"), None);
        assert_eq!(Version::parse("0.3.1.4"), None);
        assert!(Version(0, 3, 10) > Version(0, 3, 9), "numeric, not text");
        assert_eq!(Version(0, 3, 12).to_string(), "0.3.12");
    }

    /// A newer version is found; the same or an older one, a draft, a tag without a
    /// manifest or that isn't a version, is not.
    #[test]
    fn the_feed_offers_only_a_newer_release_with_a_manifest() {
        let cur = Version(0, 3, 11);
        let f = feed(&[
            ("v0.3.11", false, true),
            ("v0.3.12", false, true),
            ("v0.3.14", true, true),
            ("v0.3.13", false, false),
            ("nightly", false, true),
        ]);
        let (v, url) = newest(&f, cur).unwrap();
        assert_eq!(v, Version(0, 3, 12));
        assert!(url.ends_with("v0.3.12/update.json"));
        assert_eq!(newest(&feed(&[("v0.3.11", false, true)]), cur), None, "same");
        assert_eq!(newest(&feed(&[("v0.3.9", false, true)]), cur), None, "older");
    }

    #[test]
    fn a_manifest_must_be_ours_newer_and_have_our_file() {
        let cur = Version(0, 3, 11);
        let want = Version(0, 3, 12);
        let (v, f) = pick(&manifest("0.3.12"), want, cur, T).unwrap();
        assert_eq!((v, f.target.as_str()), (want, T));
        assert_eq!(pick(&manifest("0.3.11"), want, cur, T).unwrap_err(), Refused::NotNewer);
        assert_eq!(
            pick(&manifest("0.3.13"), want, cur, T).unwrap_err(),
            Refused::NotNewer,
            "not the version the feed listed"
        );
        let mut m = manifest("0.3.12");
        m.edition = "official".into();
        assert!(matches!(pick(&m, want, cur, T), Err(Refused::NotOurs(_))));
        let mut m = manifest("0.3.12");
        m.channel = "stable".into();
        assert!(matches!(pick(&m, want, cur, T), Err(Refused::NotOurs(_))));
        assert_eq!(
            pick(&manifest("0.3.12"), want, cur, "aarch64-apple-darwin").unwrap_err(),
            Refused::NoFileForTarget
        );
        assert!(matches!(parse_manifest(b"{"), Err(Refused::Malformed(_))));
    }

    #[test]
    fn the_github_feed_shape_parses() {
        let json = br#"[{"tag_name":"v0.3.12","draft":false,"prerelease":true,
            "assets":[{"name":"update.json","browser_download_url":"https://x/update.json","size":3}]}]"#;
        let f = parse_feed(json).unwrap();
        assert_eq!(f[0].assets[0].name, "update.json");
    }
}
