//! Where Lines keeps its files, and a background writer for them.
//!
//! `<config dir>` is `%APPDATA%\lines` on Windows, `~/Library/Application Support/lines` on
//! macOS, `$XDG_CONFIG_HOME/lines` or `~/.config/lines` elsewhere; `LINES_CONFIG_DIR` replaces
//! it (tests, portable installs). It holds:
//! - `settings.json`: recents and favourites (`crate::picker::Settings`);
//! - `workspace.json`: the autosaved workspace, restored on start;
//! - `workspaces/<name>.json`: named workspaces.
//!
//! Every file is versioned and saved with a backup of the previous one (`crate::versioned`).
//! Writes happen on one background thread ([`Writer`]), never on the UI thread; a burst of
//! saves to one path writes only the newest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use crate::versioned;

pub fn config_dir() -> Option<PathBuf> {
    let var = |k| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(d) = var("LINES_CONFIG_DIR") {
        return Some(d);
    }
    let base = if cfg!(windows) {
        var("APPDATA")?
    } else if cfg!(target_os = "macos") {
        var("HOME")?.join("Library/Application Support")
    } else {
        var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|h| h.join(".config")))?
    };
    Some(base.join("lines"))
}

pub fn workspace_path(dir: &Path) -> PathBuf {
    dir.join("workspace.json")
}

pub fn named_dir(dir: &Path) -> PathBuf {
    dir.join("workspaces")
}

/// `name` must have passed `wsdoc::valid_name`.
pub fn named_path(dir: &Path, name: &str) -> PathBuf {
    named_dir(dir).join(format!("{name}.json"))
}

/// Saved workspace names, sorted case-insensitively. Backups and temp files are skipped.
pub fn list_named(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(named_dir(dir)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".json")?;
            crate::wsdoc::valid_name(stem)
                .ok()
                .filter(|s| s == stem)
                .map(|_| stem.to_owned())
        })
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

enum Job {
    Write(PathBuf, Vec<u8>),
    Flush(Sender<()>),
}

/// The background file writer. Cloning shares the thread.
#[derive(Clone)]
pub struct Writer {
    tx: Sender<Job>,
}

impl Writer {
    pub fn spawn() -> Writer {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("lines-save".into())
            .spawn(move || run(rx));
        if let Err(e) = spawned {
            tracing::warn!("couldn't start the save thread: {e}; nothing will be saved");
        }
        Writer { tx }
    }

    /// Queue `bytes` for `path` (atomic replace with a `.bak` of the previous file).
    pub fn write(&self, path: PathBuf, bytes: Vec<u8>) {
        let _ = self.tx.send(Job::Write(path, bytes));
    }

    /// Wait (up to `timeout`) until everything queued so far is on disk. For quitting.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Job::Flush(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(timeout).is_ok()
    }
}

fn run(rx: Receiver<Job>) {
    while let Ok(first) = rx.recv() {
        // Take everything queued, keep the newest write per path, preserving first-seen order.
        let mut order: Vec<PathBuf> = Vec::new();
        let mut latest: HashMap<PathBuf, Vec<u8>> = HashMap::new();
        let mut flushes = Vec::new();
        let mut take = |job| match job {
            Job::Write(p, b) => {
                if latest.insert(p.clone(), b).is_none() {
                    order.push(p);
                }
            }
            Job::Flush(ack) => flushes.push(ack),
        };
        take(first);
        while let Ok(job) = rx.try_recv() {
            take(job);
        }
        for p in order {
            if let Some(bytes) = latest.remove(&p)
                && let Err(e) = versioned::save(&p, &bytes)
            {
                tracing::warn!("can't save {}: {e}", p.display());
            }
        }
        for ack in flushes {
            let _ = ack.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_land_in_order_with_backups_and_flush_waits() {
        let dir = lines_core::scratch::Scratch::new("persist");
        let w = Writer::spawn();
        let p = workspace_path(&dir);
        for i in 0..50 {
            w.write(p.clone(), format!("{{\"n\":{i}}}").into_bytes());
        }
        w.write(named_path(&dir, "desk"), b"{}".to_vec());
        assert!(w.flush(Duration::from_secs(10)));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"n\":49}");
        // Not a saved workspace: a backup, and a name `valid_name` refuses.
        std::fs::write(named_dir(&dir).join("x.y.json"), b"{}").unwrap();
        std::fs::write(named_dir(&dir).join("desk.json.bak"), b"{}").unwrap();
        std::fs::write(named_dir(&dir).join("alpha.json"), b"{}").unwrap();
        assert_eq!(list_named(&dir), vec!["alpha", "desk"]);
    }
}
