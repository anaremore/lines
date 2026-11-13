//! Applying and undoing an update on disk (decision 0031 §4, §5): the saved-file backup, the
//! rename-aside swap, the rollback, and the start count that offers the rollback after two
//! failed starts. Plain file operations on paths the caller gives, so tests use temp folders.
//!
//! The swap: the running executable is renamed aside to `<stem>.previous<ext>` (Windows lets
//! a running file be renamed, not overwritten) and the verified download is copied to its
//! name. A failed copy renames the previous one back. Rolling back renames the new one aside
//! (`<stem>.failed<ext>`) and the previous one back, and restores the saved files backed up
//! before the update, so an older version never reads a newer format.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::versioned::{self, Format};

/// Where the saved files and their backups are: settings and workspaces in the config folder,
/// paper accounts and backups where `crate::data_dir` puts them (the data folder, or the config
/// folder on an install from before it).
#[derive(Clone, Debug, PartialEq)]
pub struct Dirs {
    pub config: PathBuf,
    pub paper: PathBuf,
    pub backups: PathBuf,
}

impl Dirs {
    /// Everything in the config folder (installs from before the data folder; tests).
    pub fn in_config(config: PathBuf) -> Dirs {
        Dirs {
            paper: config.join("paper"),
            backups: config.join("backups"),
            config,
        }
    }

    /// This run's places (`crate::data_dir::locations`).
    pub fn for_run(config: PathBuf) -> Dirs {
        let l = crate::data_dir::locations();
        let mut d = Dirs::in_config(config);
        if let Some(p) = l.paper {
            d.paper = p;
        }
        if let Some(b) = l.backups {
            d.backups = b;
        }
        d
    }

    /// A backed-up file's place: `paper/…` in the paper folder, the rest in the config folder.
    fn file(&self, rel: &Path) -> PathBuf {
        match rel.strip_prefix("paper") {
            Ok(rest) => self.paper.join(rest),
            Err(_) => self.config.join(rel),
        }
    }
}

/// The saved files backed up before an update, as kept in a backup: settings, workspaces and
/// drawings (config folder) and the trading files (paper accounts, guardrails) as `paper/…`.
/// Not the store, recordings, journal or logs (their formats are versioned per file and only
/// grow; decision 0031 §5).
fn backed_up(dirs: &Dirs) -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from("settings.json"),
        PathBuf::from("workspace.json"),
    ];
    for dir in ["workspaces", "paper", "drawings"] {
        if let Ok(rd) = std::fs::read_dir(dirs.file(Path::new(dir))) {
            let mut names: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "json") && p.is_file())
                .filter_map(|p| p.file_name().map(|n| Path::new(dir).join(n)))
                .collect();
            names.sort();
            v.extend(names);
        }
    }
    v.retain(|p| dirs.file(p).is_file());
    v
}

/// `<backups>/<version>`.
pub fn backup_dir(dirs: &Dirs, version: &str) -> PathBuf {
    dirs.backups.join(version)
}

/// Copy the saved files to `backups/<version>/` (replacing an earlier backup of that
/// version). Returns how many were copied.
pub fn backup(dirs: &Dirs, version: &str) -> std::io::Result<usize> {
    let to = backup_dir(dirs, version);
    if to.exists() {
        std::fs::remove_dir_all(&to)?;
    }
    let files = backed_up(dirs);
    for rel in &files {
        let dst = to.join(rel);
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(dirs.file(rel), &dst)?;
    }
    Ok(files.len())
}

/// Copy `backups/<version>/` back over the saved files. Returns how many were restored.
pub fn restore(dirs: &Dirs, version: &str) -> std::io::Result<usize> {
    let from = backup_dir(dirs, version);
    let mut n = 0;
    let mut stack = vec![from.clone()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)? {
            let p = e?.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(&from) {
                let dst = dirs.file(rel);
                if let Some(d) = dst.parent() {
                    std::fs::create_dir_all(d)?;
                }
                std::fs::copy(&p, &dst)?;
                n += 1;
            }
        }
    }
    Ok(n)
}

fn sibling(exe: &Path, tag: &str) -> PathBuf {
    let stem = exe
        .file_stem()
        .map_or_else(Default::default, |s| s.to_os_string());
    let mut name = stem;
    name.push(format!(".{tag}"));
    if let Some(ext) = exe.extension() {
        name.push(".");
        name.push(ext);
    }
    exe.with_file_name(name)
}

/// The previous version's executable kept beside `exe`.
pub fn previous_exe(exe: &Path) -> PathBuf {
    sibling(exe, "previous")
}

/// Put the file `new` in place of `exe`, keeping the old one as [`previous_exe`] (tests; the
/// app swaps the bytes it verified: [`swap_bytes`]).
#[cfg(test)]
pub fn swap(exe: &Path, new: &Path) -> std::io::Result<()> {
    swap_bytes(exe, &std::fs::read(new)?)
}

/// Put `data` (the bytes just verified, never re-read from disk) in place of `exe`, keeping
/// the old one as [`previous_exe`].
pub fn swap_bytes(exe: &Path, data: &[u8]) -> std::io::Result<()> {
    let prev = previous_exe(exe);
    if prev.exists() {
        std::fs::remove_file(&prev)?;
    }
    std::fs::rename(exe, &prev)?;
    if let Err(e) = std::fs::write(exe, data) {
        // Put the running version back where it was.
        let _ = std::fs::remove_file(exe);
        let _ = std::fs::rename(&prev, exe);
        return Err(e);
    }
    Ok(())
}

/// Put the previous executable back in place of `exe` (the failed one kept aside).
pub fn roll_back_exe(exe: &Path) -> std::io::Result<()> {
    let prev = previous_exe(exe);
    if !prev.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no previous version is kept",
        ));
    }
    let failed = sibling(exe, "failed");
    if failed.exists() {
        std::fs::remove_file(&failed)?;
    }
    std::fs::rename(exe, &failed)?;
    if let Err(e) = std::fs::rename(&prev, exe) {
        let _ = std::fs::rename(&failed, exe);
        return Err(e);
    }
    Ok(())
}

/// What the last update did, saved as `update-state.json` in the config folder: which
/// version it came from (its backups and the kept executable) and how many times the new one
/// has started without lasting.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateState {
    pub from: String,
    pub to: String,
    /// Starts of `to` since the update.
    #[serde(default)]
    pub starts: u32,
    /// `to` ran long enough once: it works, and the start count is done.
    #[serde(default)]
    pub confirmed: bool,
}

fn state_v0_to_v1(v: serde_json::Value) -> Result<serde_json::Value, String> {
    Ok(v)
}

pub const STATE_FORMAT: Format = Format {
    what: "update state",
    current: 1,
    migrations: &[state_v0_to_v1],
};

pub fn state_path(config: &Path) -> PathBuf {
    config.join("update-state.json")
}

pub fn load_state(config: &Path) -> Option<UpdateState> {
    versioned::load::<UpdateState>(&STATE_FORMAT, &state_path(config)).value
}

pub fn save_state(config: &Path, s: &UpdateState) -> std::io::Result<()> {
    let bytes = versioned::encode(&STATE_FORMAT, s).map_err(std::io::Error::other)?;
    versioned::save(&state_path(config), &bytes)
}

/// Remove staged downloads (`updates/<version>/`) of versions no newer than `current`.
pub fn clean_staged(updates: &Path, current: &str) {
    let Some(cur) = super::manifest::Version::parse(current) else {
        return;
    };
    let Ok(rd) = std::fs::read_dir(updates) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if super::manifest::Version::parse(&name).is_some_and(|v| v <= cur) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Failed starts after which the next launch offers the rollback (decision 0031 §4: two).
pub const FAILED_STARTS: u32 = 2;

/// A start of version `current`: the state to save, and whether to offer the rollback (the
/// update's version has already failed to start [`FAILED_STARTS`] times).
pub fn on_start(state: Option<UpdateState>, current: &str) -> (Option<UpdateState>, bool) {
    match state {
        Some(mut s) if s.to == current && !s.confirmed => {
            let offer = s.starts >= FAILED_STARTS;
            s.starts += 1;
            (Some(s), offer)
        }
        other => (other, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> lines_core::scratch::Scratch {
        lines_core::scratch::Scratch::new(&format!("update-{name}"))
    }

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    /// The saved files are copied before an update and a rollback restores them: an older
    /// version never reads what the newer one wrote.
    #[test]
    fn a_rollback_restores_the_saved_files() {
        let c = dir("backup");
        write(&c.join("settings.json"), "old settings");
        write(&c.join("workspace.json"), "old workspace");
        write(&c.join("workspaces/day.json"), "old day");
        write(&c.join("paper/replay.json"), "old paper");
        write(&c.join("journal/replay.jsonl"), "journal stays");
        let d = Dirs::in_config(c.to_path_buf());
        assert_eq!(backup(&d, "0.3.11").unwrap(), 4);
        // The new version migrates them forward.
        write(&c.join("settings.json"), "new settings");
        write(&c.join("paper/replay.json"), "new paper");
        assert_eq!(restore(&d, "0.3.11").unwrap(), 4);
        assert_eq!(read(&c.join("settings.json")), "old settings");
        assert_eq!(read(&c.join("paper/replay.json")), "old paper");
        assert_eq!(read(&c.join("workspaces/day.json")), "old day");
        assert!(
            !backup_dir(&d, "0.3.11").join("journal").exists(),
            "not the journal"
        );
    }

    /// With paper accounts and backups in the data folder, a backup holds the paper files and
    /// a rollback puts them back there, not in the config folder.
    #[test]
    fn paper_in_the_data_folder_is_backed_up_and_restored_there() {
        let root = dir("backup-data");
        let (c, data) = (root.join("config"), root.join("data"));
        write(&c.join("settings.json"), "old settings");
        write(&data.join("paper/coinbase.json"), "old paper");
        let d = Dirs {
            config: c.clone(),
            paper: data.join("paper"),
            backups: data.join("backups"),
        };
        assert_eq!(backup(&d, "0.3.11").unwrap(), 2);
        assert!(data.join("backups/0.3.11/paper/coinbase.json").is_file());
        assert!(!c.join("backups").exists(), "backups go to the data folder");
        write(&data.join("paper/coinbase.json"), "new paper");
        assert_eq!(restore(&d, "0.3.11").unwrap(), 2);
        assert_eq!(read(&data.join("paper/coinbase.json")), "old paper");
        assert!(!c.join("paper").exists(), "never into the config folder");
    }

    #[test]
    fn the_swap_keeps_the_previous_version_and_rolls_back() {
        let d = dir("swap");
        let exe = d.join("lines.exe");
        let new = d.join("download.exe");
        write(&exe, "v0.3.11");
        write(&new, "v0.3.12");
        swap(&exe, &new).unwrap();
        assert_eq!(read(&exe), "v0.3.12");
        assert_eq!(read(&previous_exe(&exe)), "v0.3.11");
        assert_eq!(
            previous_exe(&exe).file_name().unwrap(),
            "lines.previous.exe"
        );
        roll_back_exe(&exe).unwrap();
        assert_eq!(read(&exe), "v0.3.11");
        assert_eq!(read(&d.join("lines.failed.exe")), "v0.3.12");
        assert!(roll_back_exe(&exe).is_err(), "nothing kept to roll back to");
    }

    /// A swap whose copy fails leaves the running version in place.
    #[test]
    fn a_failed_swap_changes_nothing() {
        let d = dir("swapfail");
        let exe = d.join("lines.exe");
        write(&exe, "v0.3.11");
        assert!(swap(&exe, &d.join("missing.exe")).is_err());
        assert_eq!(read(&exe), "v0.3.11");
        assert!(!previous_exe(&exe).exists());
    }

    /// Two failed starts of the new version: the third offers the rollback; once it ran long
    /// enough (confirmed) nothing is offered.
    #[test]
    fn two_failed_starts_offer_the_rollback() {
        let s = Some(UpdateState {
            from: "0.3.11".into(),
            to: "0.3.12".into(),
            starts: 0,
            confirmed: false,
        });
        let (s, offer) = on_start(s, "0.3.12");
        assert!(!offer, "first start");
        let (s, offer) = on_start(s, "0.3.12");
        assert!(!offer, "second start (the first failed)");
        let (s, offer) = on_start(s, "0.3.12");
        assert!(offer, "two failed starts");
        let mut ok = s.clone().unwrap();
        ok.confirmed = true;
        assert!(!on_start(Some(ok), "0.3.12").1);
        assert!(
            !on_start(s, "0.3.11").1,
            "the old version (rolled back) isn't counted"
        );
        assert_eq!(on_start(None, "0.3.12"), (None, false));
    }

    #[test]
    fn staged_downloads_this_version_passed_are_removed() {
        let d = dir("clean");
        for v in ["0.3.10", "0.3.11", "0.3.12"] {
            write(&d.join(v).join("lines.exe"), v);
        }
        clean_staged(&d, "0.3.11");
        assert!(!d.join("0.3.10").exists() && !d.join("0.3.11").exists());
        assert!(d.join("0.3.12").exists(), "a newer one waits");
    }

    #[test]
    fn the_state_round_trips() {
        let d = dir("state");
        let s = UpdateState {
            from: "0.3.11".into(),
            to: "0.3.12".into(),
            starts: 2,
            confirmed: false,
        };
        save_state(&d, &s).unwrap();
        assert_eq!(load_state(&d), Some(s));
    }
}
