//! Versioned JSON files with forward migrations (settings, workspaces).
//!
//! Every file Lines writes carries `"version": N`. Reading goes through [`decode`]:
//! - the current version deserializes directly;
//! - an older version runs the [`Format`]'s migrations in order (`version` missing counts as 0,
//!   i.e. files written before versioning existed) and then deserializes;
//! - a newer version (written by a later Lines) or anything unreadable is *not* interpreted.
//!
//! [`load`] adds the file system rules: user data is never silently discarded. A file that
//! can't be used is copied aside first (`<stem>.unreadable-<ms>.json`, `<stem>.v<N>-<ms>.json`)
//! and the caller gets a report to show; a migrated file keeps its old version as
//! `<stem>.v<old>.json` before anything overwrites it. [`save`] writes atomically and keeps the
//! previous file as `<file>.bak`.
//!
//! Decoding is pure (bytes in, value out) and unit-tested; only `load`/`save` touch the disk.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Upgrades a document from version `from` to `from + 1`.
pub type Migration = fn(Value) -> Result<Value, String>;

/// A file format: its current version and how to get there from every older one.
pub struct Format {
    /// What the file holds, for messages ("workspace", "settings").
    pub what: &'static str,
    pub current: u32,
    /// `migrations[v]` upgrades version `v` to `v + 1`; `len() == current`.
    pub migrations: &'static [Migration],
}

/// The outcome of reading a document's bytes.
#[derive(Debug, PartialEq)]
pub enum Decoded<T> {
    Ok {
        value: T,
        /// The version on disk, if it was older than current (the value was migrated).
        migrated_from: Option<u32>,
    },
    /// Written by a newer Lines; left uninterpreted.
    Newer { found: u32 },
    /// Not JSON, not an object, a bad version field, a failed migration, or a shape this
    /// version can't read.
    Corrupt { reason: String },
}

/// The version a document says it has (missing = 0, i.e. before versioning).
fn version_of(v: &Value) -> Result<u32, String> {
    let obj = v.as_object().ok_or("not a JSON object")?;
    match obj.get("version") {
        None => Ok(0),
        Some(n) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| format!("bad version field {n}")),
    }
}

pub fn decode<T: DeserializeOwned>(format: &Format, bytes: &[u8]) -> Decoded<T> {
    debug_assert_eq!(format.migrations.len(), format.current as usize);
    let mut doc: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            return Decoded::Corrupt {
                reason: format!("not valid JSON: {e}"),
            };
        }
    };
    let found = match version_of(&doc) {
        Ok(v) => v,
        Err(reason) => return Decoded::Corrupt { reason },
    };
    if found > format.current {
        return Decoded::Newer { found };
    }
    for v in found..format.current {
        doc = match (format.migrations[v as usize])(doc) {
            Ok(d) => d,
            Err(e) => {
                return Decoded::Corrupt {
                    reason: format!("migrating version {v} to {}: {e}", v + 1),
                };
            }
        };
        if let Some(o) = doc.as_object_mut() {
            o.insert("version".into(), Value::from(v + 1));
        }
    }
    match serde_json::from_value(doc) {
        Ok(value) => Decoded::Ok {
            value,
            migrated_from: (found < format.current).then_some(found),
        },
        Err(e) => Decoded::Corrupt {
            reason: e.to_string(),
        },
    }
}

/// `value` as pretty JSON with `"version"` set to the format's current version.
pub fn encode<T: Serialize>(format: &Format, value: &T) -> Result<Vec<u8>, String> {
    let mut v = serde_json::to_value(value).map_err(|e| e.to_string())?;
    let obj = v.as_object_mut().ok_or("not a JSON object")?;
    obj.insert("version".into(), Value::from(format.current));
    serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())
}

/// What [`load`] found.
#[derive(Debug)]
pub struct Loaded<T> {
    /// `None`: use defaults (missing file, or one that couldn't be used).
    pub value: Option<T>,
    /// Something the user should be told (shown inline, also logged). `None` when the file
    /// was missing or read cleanly.
    pub report: Option<String>,
    /// `false` if a file that couldn't be used also couldn't be backed up: saving over it
    /// would lose it, so the caller must not write this path this session.
    pub writable: bool,
}

/// Milliseconds since the Unix epoch, for backup names.
fn stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// `dir/<stem><suffix>.json` beside `path`.
pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let stem = path
        .file_stem()
        .map_or_else(|| "file".into(), |s| s.to_string_lossy().into_owned());
    path.with_file_name(format!("{stem}{suffix}.json"))
}

/// Read and decode `path`. Never deletes or overwrites a file it couldn't use without copying
/// it aside first; see the module docs.
pub fn load<T: DeserializeOwned>(format: &Format, path: &Path) -> Loaded<T> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Loaded {
                value: None,
                report: None,
                writable: true,
            };
        }
        Err(e) => {
            // Unreadable (permissions, locked): we can't copy it either, so don't replace it.
            return Loaded {
                value: None,
                report: Some(format!(
                    "can't read {} {}: {e}; using defaults and not saving over it",
                    format.what,
                    path.display()
                )),
                writable: false,
            };
        }
    };
    let set_aside = |suffix: String, why: String| -> Loaded<T> {
        let backup = sibling(path, &suffix);
        match std::fs::copy(path, &backup) {
            Ok(_) => Loaded {
                value: None,
                report: Some(format!(
                    "{} file {} {why}; kept it as {} and loaded defaults",
                    format.what,
                    path.display(),
                    backup.display()
                )),
                writable: true,
            },
            Err(e) => Loaded {
                value: None,
                report: Some(format!(
                    "{} file {} {why}, and backing it up failed ({e}); using defaults and not \
                     saving over it",
                    format.what,
                    path.display()
                )),
                writable: false,
            },
        }
    };
    match decode::<T>(format, &bytes) {
        Decoded::Ok {
            value,
            migrated_from: None,
        } => Loaded {
            value: Some(value),
            report: None,
            writable: true,
        },
        Decoded::Ok {
            value,
            migrated_from: Some(old),
        } => {
            // Keep the pre-migration file for downgrades; never replace an earlier copy.
            let keep = sibling(path, &format!(".v{old}"));
            let kept = keep.exists() || std::fs::copy(path, &keep).is_ok();
            if kept {
                tracing::info!(
                    "{} {} upgraded from version {old}; the old file is kept as {}",
                    format.what,
                    path.display(),
                    keep.display()
                );
            }
            Loaded {
                value: Some(value),
                report: (!kept).then(|| {
                    format!(
                        "{} {} upgraded from version {old}, but the old file couldn't be kept; \
                         not saving over it",
                        format.what,
                        path.display()
                    )
                }),
                writable: kept,
            }
        }
        Decoded::Newer { found } => set_aside(
            format!(".v{found}-{}", stamp()),
            format!(
                "is version {found}, newer than this Lines reads ({})",
                format.current
            ),
        ),
        Decoded::Corrupt { reason } => set_aside(
            format!(".unreadable-{}", stamp()),
            format!("couldn't be read ({reason})"),
        ),
    }
}

/// Write `bytes` to `path` atomically (temp file + rename), keeping the previous file as
/// `<file>.bak`. Creates the directory. Blocking: call it off the UI thread.
pub fn save(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    if path.exists() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        std::fs::copy(path, PathBuf::from(bak))?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Doc {
        name: String,
        #[serde(default)]
        size: u32,
    }

    /// v0 called the field `title`; v1 renamed it to `name`; v2 added `size` (defaults to 0,
    /// but v1 files had an implied size of 7).
    fn v0_to_v1(mut v: Value) -> Result<Value, String> {
        let o = v.as_object_mut().ok_or("not an object")?;
        let title = o.remove("title").ok_or("no title")?;
        o.insert("name".into(), title);
        Ok(v)
    }
    fn v1_to_v2(mut v: Value) -> Result<Value, String> {
        v.as_object_mut()
            .ok_or("not an object")?
            .insert("size".into(), Value::from(7));
        Ok(v)
    }
    const FORMAT: Format = Format {
        what: "test",
        current: 2,
        migrations: &[v0_to_v1, v1_to_v2],
    };

    fn doc(name: &str, size: u32) -> Doc {
        Doc {
            name: name.into(),
            size,
        }
    }

    #[test]
    fn current_version_reads_directly() {
        let d: Decoded<Doc> = decode(&FORMAT, br#"{"version":2,"name":"a","size":3}"#);
        assert_eq!(
            d,
            Decoded::Ok {
                value: doc("a", 3),
                migrated_from: None
            }
        );
    }

    #[test]
    fn older_versions_migrate_forward_in_order() {
        let d: Decoded<Doc> = decode(&FORMAT, br#"{"version":1,"name":"a"}"#);
        assert_eq!(
            d,
            Decoded::Ok {
                value: doc("a", 7),
                migrated_from: Some(1)
            }
        );
        // No version field: written before versioning existed.
        let d: Decoded<Doc> = decode(&FORMAT, br#"{"title":"b"}"#);
        assert_eq!(
            d,
            Decoded::Ok {
                value: doc("b", 7),
                migrated_from: Some(0)
            }
        );
    }

    #[test]
    fn newer_and_corrupt_are_not_interpreted() {
        assert_eq!(
            decode::<Doc>(&FORMAT, br#"{"version":3,"whatever":1}"#),
            Decoded::Newer { found: 3 }
        );
        for bad in [
            &b"{"[..],
            b"[1,2]",
            br#"{"version":"two"}"#,
            br#"{"version":-1}"#,
            br#"{"version":2,"name":5}"#,
            br#"{"version":0}"#, // migration fails: no title
        ] {
            assert!(
                matches!(decode::<Doc>(&FORMAT, bad), Decoded::Corrupt { .. }),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn encode_stamps_the_current_version_and_round_trips() {
        let bytes = encode(&FORMAT, &doc("x", 1)).unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["version"], 2);
        assert_eq!(
            decode::<Doc>(&FORMAT, &bytes),
            Decoded::Ok {
                value: doc("x", 1),
                migrated_from: None
            }
        );
    }

    fn temp(name: &str) -> lines_core::scratch::Scratch {
        lines_core::scratch::Scratch::new(&format!("versioned-{name}"))
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn missing_file_is_defaults_without_a_report() {
        let dir = temp("missing");
        let l: Loaded<Doc> = load(&FORMAT, &dir.join("w.json"));
        assert!(l.value.is_none() && l.report.is_none() && l.writable);
    }

    #[test]
    fn corrupt_and_newer_files_are_backed_up_and_reported() {
        let dir = temp("bad");
        let path = dir.join("w.json");
        std::fs::write(&path, b"{ truncated").unwrap();
        let l: Loaded<Doc> = load(&FORMAT, &path);
        assert!(l.value.is_none() && l.writable);
        let report = l.report.unwrap();
        assert!(report.contains("couldn't be read"), "{report}");
        let names = files(&dir);
        let backup = names
            .iter()
            .find(|n| n.starts_with("w.unreadable-"))
            .unwrap();
        assert_eq!(std::fs::read(dir.join(backup)).unwrap(), b"{ truncated");

        std::fs::write(&path, br#"{"version":9}"#).unwrap();
        let l: Loaded<Doc> = load(&FORMAT, &path);
        assert!(l.value.is_none());
        assert!(l.report.unwrap().contains("version 9, newer"));
        assert!(files(&dir).iter().any(|n| n.starts_with("w.v9-")));
    }

    #[test]
    fn migrated_files_keep_their_old_version() {
        let dir = temp("migrate");
        let path = dir.join("w.json");
        std::fs::write(&path, br#"{"version":1,"name":"old"}"#).unwrap();
        let l: Loaded<Doc> = load(&FORMAT, &path);
        assert_eq!(l.value, Some(doc("old", 7)));
        assert!(l.report.is_none() && l.writable);
        assert_eq!(
            std::fs::read(dir.join("w.v1.json")).unwrap(),
            br#"{"version":1,"name":"old"}"#
        );
    }

    #[test]
    fn save_is_atomic_and_keeps_the_previous_file() {
        let dir = temp("save");
        let path = dir.join("sub").join("w.json");
        save(&path, b"one").unwrap();
        assert_eq!(files(&dir.join("sub")), vec!["w.json"]);
        save(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(
            std::fs::read(dir.join("sub").join("w.json.bak")).unwrap(),
            b"one"
        );
        assert_eq!(files(&dir.join("sub")), vec!["w.json", "w.json.bak"]);
    }
}
