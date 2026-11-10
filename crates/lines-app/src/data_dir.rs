//! The data folder (Settings › Data › Data folder): where the local store (`store-v1/`),
//! `recordings/`, `logs/`, the paper accounts (`paper/`), the journal (`journal/`, with
//! `journal/shots/`) and update backups (`backups/`) live. Settings and workspaces stay in the
//! config folder (`crate::persist::config_dir`).
//!
//! Which folder, in order: `LINES_DATA_DIR`, else the folder named by the pointer file
//! `<OS default>/location.json` (it can't live inside the folder it moves), else the OS default
//! (`lines_feed::disk::os_default_root`). The pointer is read leniently: missing, unreadable or
//! naming a folder that doesn't exist means the OS default, with a notice; nothing is created.
//!
//! `paper/`, `journal/` and `backups/` used to live in the config folder. Existing installs keep
//! working with no automatic move: each is `<data>/<name>` if that exists, else
//! `<config>/<name>` if that exists, else `<data>/<name>`.
//!
//! Changing the folder applies at the next start. "Start empty" only writes the pointer.
//! "Move existing data" (never while a position or a working order is open) writes the pointer
//! with `pending_move` and copies the big items (`store-v1`, `logs`, `recordings`) in the
//! background while this run keeps the old folder, checking each file byte for byte right after
//! copying it and recording what it checked (`location.verified.json` beside the pointer: path,
//! size, modification time). The next start, before anything opens a file there, trusts a
//! recorded file still the same size and time in both places, copies and checks every other one
//! (missing, grown, never checked), always copies and checks `paper`/`journal`/`backups` (from
//! wherever they live), switches the pointer, and only then removes
//! the old copies (only the files it moved; a folder is removed only once empty). Any failure
//! keeps the old folder and both copies, says so, and the move is tried again at the next
//! start until it is canceled.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::versioned::{self, Format};

pub const POINTER_FILE: &str = "location.json";
pub const STORE: &str = "store-v1";
pub const LOGS: &str = "logs";
pub const RECORDINGS: &str = "recordings";
pub const PAPER: &str = "paper";
pub const JOURNAL: &str = "journal";
pub const BACKUPS: &str = "backups";
/// Big and append-only: copied in the background before the restart.
pub const BIG: [&str; 3] = [STORE, LOGS, RECORDINGS];
/// Small; may still live in the config folder (installs from before the data folder).
pub const SMALL: [&str; 3] = [PAPER, JOURNAL, BACKUPS];
/// Every item: a target holding any of them with something in it already has Lines data.
pub const ITEMS: [&str; 6] = [STORE, LOGS, RECORDINGS, PAPER, JOURNAL, BACKUPS];

// ---- the pointer file -------------------------------------------------------------------

/// `location.json`: the folder to use, and a move waiting to finish at the next start.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pointer {
    pub data_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_move: Option<PendingMove>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingMove {
    /// The folder the data is moving from (the one in use when the move was chosen).
    pub from: PathBuf,
    /// This move's id, also in `<from>/.lines-move-source` ([`MARKER_FILE`]): a folder at
    /// `from` without it (recreated empty, another disk with the same letter, an unmounted
    /// mount point) is never moved from.
    #[serde(default)]
    pub id: String,
}

/// In the folder being moved from: the pending move's id.
pub const MARKER_FILE: &str = ".lines-move-source";

/// A new move's id: unique enough to tell this move's source from any other folder.
pub fn new_move_id() -> String {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{ns:x}-{:x}", std::process::id())
}

/// Mark `from` as the source of move `id`.
pub fn write_marker(from: &Path, id: &str) -> io::Result<()> {
    let path = from.join(MARKER_FILE);
    let tmp = from.join(format!("{MARKER_FILE}.tmp"));
    std::fs::write(&tmp, id.as_bytes())?;
    std::fs::rename(&tmp, path)
}

/// Whether `from` holds move `id`'s marker.
fn has_marker(from: &Path, id: &str) -> bool {
    std::fs::read_to_string(from.join(MARKER_FILE)).is_ok_and(|m| m.trim() == id)
}

/// Remove `from`'s marker (best effort).
fn remove_marker(from: &Path) {
    let _ = std::fs::remove_file(from.join(MARKER_FILE));
}

/// A file without `version` has version 1's fields.
fn pointer_v0_to_v1(v: serde_json::Value) -> Result<serde_json::Value, String> {
    Ok(v)
}

pub const POINTER_FORMAT: Format = Format {
    what: "data folder pointer",
    current: 1,
    migrations: &[pointer_v0_to_v1],
};

pub fn pointer_path(os_default: &Path) -> PathBuf {
    os_default.join(POINTER_FILE)
}

/// The pointer, leniently: missing is `None`; unreadable, newer or damaged is `None` and a
/// notice. Never writes, never sets a file aside.
pub fn read_pointer(path: &Path) -> (Option<Pointer>, Option<String>) {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (None, None),
        Err(e) => {
            return (
                None,
                Some(format!(
                    "couldn't read {} ({e}); using the default data folder",
                    path.display()
                )),
            );
        }
    };
    match versioned::decode::<Pointer>(&POINTER_FORMAT, &bytes) {
        versioned::Decoded::Ok { value, .. } => (Some(value), None),
        versioned::Decoded::Newer { found } => (
            None,
            Some(format!(
                "{} is version {found}, newer than this build reads; using the default data folder",
                path.display()
            )),
        ),
        versioned::Decoded::Corrupt { reason } => (
            None,
            Some(format!(
                "{} couldn't be read ({reason}); using the default data folder",
                path.display()
            )),
        ),
    }
}

pub fn write_pointer(os_default: &Path, p: &Pointer) -> io::Result<()> {
    let bytes = versioned::encode(&POINTER_FORMAT, p).map_err(io::Error::other)?;
    versioned::save(&pointer_path(os_default), &bytes)
}

pub const VERIFIED_FILE: &str = "location.verified.json";

/// What a move's background copy checked byte for byte, for one move (`from` → `to`): the
/// next start trusts these files while they keep this size and time in both folders.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Verified {
    pub from: PathBuf,
    pub to: PathBuf,
    pub files: Vec<VerifiedFile>,
    /// Every file this move created or replaced in `to` (relative): only these may be
    /// replaced again; any other file already there must be the same or the move fails.
    #[serde(default)]
    pub written: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifiedFile {
    /// Relative to the data folder: `store-v1/coinbase/BTC-USD/2026-11-05.lseg`.
    pub path: PathBuf,
    /// The source's size and modification time (nanoseconds since the Unix epoch).
    pub len: u64,
    pub mtime_ns: u64,
    /// The copy's own size and time as the target drive stored them (exFAT and FAT keep
    /// times in 10 ms or 2 s steps, so they needn't equal the source's). Absent in records
    /// from before: the copy must then match the source's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst_len: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst_mtime_ns: Option<u64>,
}

fn verified_v0_to_v1(v: serde_json::Value) -> Result<serde_json::Value, String> {
    Ok(v)
}

pub const VERIFIED_FORMAT: Format = Format {
    what: "data move record",
    current: 1,
    migrations: &[verified_v0_to_v1],
};

pub fn verified_path(os_default: &Path) -> PathBuf {
    os_default.join(VERIFIED_FILE)
}

/// The record for the move `from` → `to`; anything else (missing, damaged, another move's)
/// is no record: every file is then checked.
pub fn read_verified(os_default: &Path, from: &Path, to: &Path) -> Option<Verified> {
    let bytes = std::fs::read(verified_path(os_default)).ok()?;
    match versioned::decode::<Verified>(&VERIFIED_FORMAT, &bytes) {
        versioned::Decoded::Ok { value, .. }
            if same_place(&value.from, from) && same_place(&value.to, to) =>
        {
            Some(value)
        }
        _ => None,
    }
}

/// Replace the record atomically (temporary file + rename; no backup: it is only a shortcut).
fn write_verified(os_default: &Path, v: &Verified) -> io::Result<()> {
    let bytes = versioned::encode(&VERIFIED_FORMAT, v).map_err(io::Error::other)?;
    let path = verified_path(os_default);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)
}

fn remove_verified(os_default: &Path) {
    let _ = std::fs::remove_file(verified_path(os_default));
}

/// The newest choice (Move, Start empty, Cancel) made this session.
static MOVE_SEQ: AtomicU64 = AtomicU64::new(0);
/// Held while the pointer or the record is written: writes happen one at a time, and one for
/// an older choice than the newest is skipped (the newest writes its own).
static WRITE: Mutex<()> = Mutex::new(());

fn next_seq() -> u64 {
    MOVE_SEQ.fetch_add(1, Ordering::SeqCst) + 1
}

/// Run `f` under the write lock if `seq` is still the newest choice (`None`: always; tests).
fn ordered<R>(seq: Option<u64>, f: impl FnOnce() -> R) -> Option<R> {
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    match seq {
        Some(n) if MOVE_SEQ.load(Ordering::SeqCst) != n => None,
        _ => Some(f()),
    }
}

// ---- resolution -------------------------------------------------------------------------

/// What chose the data folder this run.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum By {
    /// `LINES_DATA_DIR`: Settings can't change it.
    Env,
    /// The pointer file.
    Pointer,
    #[default]
    Default,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    pub root: Option<PathBuf>,
    pub by: By,
    /// A move waiting to finish into `root`.
    pub pending: Option<PendingMove>,
    pub notice: Option<String>,
}

/// `LINES_DATA_DIR` > the pointer's folder (if it exists) > the OS default. `pointer` is what
/// [`read_pointer`] found. A pending move whose target is gone stays with the folder it was
/// moving from.
pub fn resolve(
    env: Option<PathBuf>,
    os_default: Option<PathBuf>,
    pointer: (Option<Pointer>, Option<String>),
) -> Resolved {
    if let Some(e) = env {
        return Resolved {
            root: Some(e),
            by: By::Env,
            pending: None,
            notice: None,
        };
    }
    let default = |notice| Resolved {
        root: os_default.clone(),
        by: By::Default,
        pending: None,
        notice,
    };
    let (p, notice) = pointer;
    let Some(p) = p else {
        return default(notice);
    };
    if p.data_dir.is_dir() {
        return Resolved {
            root: Some(p.data_dir),
            by: By::Pointer,
            pending: p.pending_move,
            notice: None,
        };
    }
    match p.pending_move {
        Some(m) if m.from.is_dir() => Resolved {
            notice: Some(format!(
                "Failed: couldn't move data ({} wasn't found); still using {}",
                p.data_dir.display(),
                m.from.display()
            )),
            root: Some(m.from),
            by: By::Pointer,
            pending: None,
        },
        _ => default(Some(format!(
            "Failed: the data folder {} wasn't found; using {}",
            p.data_dir.display(),
            os_default
                .as_deref()
                .map_or_else(|| "none".into(), |d| d.display().to_string())
        ))),
    }
}

/// Where this run's files go (`None`: nowhere, nothing is saved).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Locations {
    pub root: Option<PathBuf>,
    pub paper: Option<PathBuf>,
    pub journal: Option<PathBuf>,
    pub backups: Option<PathBuf>,
}

impl Locations {
    /// Each of `paper`, `journal`, `backups`: in the data folder if there, else in the config
    /// folder if there (installs from before the data folder), else in the data folder.
    pub fn resolve(root: Option<&Path>, config: Option<&Path>) -> Locations {
        let pick = |name: &str| match (root, config) {
            (Some(r), c) => {
                let d = r.join(name);
                if d.exists() {
                    return Some(d);
                }
                match c.map(|c| c.join(name)) {
                    Some(legacy) if legacy.exists() => Some(legacy),
                    _ => Some(d),
                }
            }
            (None, Some(c)) => Some(c.join(name)),
            (None, None) => None,
        };
        Locations {
            root: root.map(Path::to_path_buf),
            paper: pick(PAPER),
            journal: pick(JOURNAL),
            backups: pick(BACKUPS),
        }
    }
}

// ---- copying, checking, removing --------------------------------------------------------

/// Files a move never copies: the store's writer locks (held open by a running writer, made
/// again by the next one) and temporary files.
fn skipped(name: &OsStr) -> bool {
    let n = name.to_string_lossy();
    n == "writer.lock" || n.ends_with(".tmp")
}

/// Every regular file under `dir` (relative, sorted), symlinks and [`skipped`] files left out.
/// A missing `dir` has none.
fn walk(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for e in std::fs::read_dir(dir.join(&rel))? {
            let e = e?;
            let ft = e.file_type()?;
            let r = rel.join(e.file_name());
            if ft.is_dir() {
                stack.push(r);
            } else if ft.is_file() && !skipped(&e.file_name()) {
                out.push(r);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Bytes under `dir` a move would copy.
pub fn tree_size(dir: &Path) -> u64 {
    walk(dir)
        .unwrap_or_default()
        .iter()
        .filter_map(|r| std::fs::metadata(dir.join(r)).ok())
        .map(|m| m.len())
        .sum()
}

/// `dst` is already `src`'s copy: same size and modification time.
fn same(src: &std::fs::Metadata, dst: &Path) -> bool {
    let Ok(d) = std::fs::metadata(dst) else {
        return false;
    };
    d.len() == src.len() && matches!((d.modified(), src.modified()), (Ok(a), Ok(b)) if a == b)
}

fn canceled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "canceled")
}

/// Copy one file through a temporary beside it, keeping its modification time.
fn copy_file(src: &Path, dst: &Path, done: &AtomicU64, cancel: &AtomicBool) -> io::Result<()> {
    let meta = std::fs::metadata(src)?;
    if let Some(d) = dst.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".copy.tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut from = std::fs::File::open(src)?;
        let mut to = std::fs::File::create(&tmp)?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            if cancel.load(Ordering::Relaxed) {
                drop(to);
                let _ = std::fs::remove_file(&tmp);
                return Err(canceled());
            }
            let n = from.read(&mut buf)?;
            if n == 0 {
                break;
            }
            to.write_all(&buf[..n])?;
            done.fetch_add(n as u64, Ordering::Relaxed);
            bump();
        }
        to.sync_all()?;
        if let Ok(t) = meta.modified() {
            to.set_modified(t)?;
        }
    }
    std::fs::rename(&tmp, dst)
}

/// Copy every file under `src` to the same place under `dst` unless it is there already (same
/// size and time). Adds the bytes it passes (copied or already there) to `done`. Tests: a
/// partial copy, as an interrupted background copy leaves.
#[cfg(test)]
fn copy_tree(src: &Path, dst: &Path, done: &AtomicU64, cancel: &AtomicBool) -> io::Result<()> {
    for rel in walk(src)? {
        if cancel.load(Ordering::Relaxed) {
            return Err(canceled());
        }
        let (s, d) = (src.join(&rel), dst.join(&rel));
        let meta = std::fs::metadata(&s)?;
        if same(&meta, &d) {
            done.fetch_add(meta.len(), Ordering::Relaxed);
            continue;
        }
        copy_file(&s, &d, done, cancel)?;
    }
    Ok(())
}

fn mtime_ns(m: &std::fs::Metadata) -> Option<u64> {
    let d = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    u64::try_from(d.as_nanos()).ok()
}

/// The record says `dst` is `src`'s checked copy, and both are as recorded: the source
/// against the source's size and time, the copy against the copy's own.
fn trusted(rec: Option<&VerifiedFile>, src: &std::fs::Metadata, dst: &Path) -> bool {
    let (Some(r), Ok(d)) = (rec, std::fs::metadata(dst)) else {
        return false;
    };
    let dst_len = r.dst_len.unwrap_or(r.len);
    let dst_t = Some(r.dst_mtime_ns.unwrap_or(r.mtime_ns));
    r.len == src.len()
        && mtime_ns(src) == Some(r.mtime_ns)
        && d.len() == dst_len
        && mtime_ns(&d) == dst_t
}

/// Copy `src` to `dst` unless it is there already (same size and time), then check it byte
/// for byte.
fn copy_and_check(
    src: &Path,
    dst: &Path,
    done: &AtomicU64,
    cancel: &AtomicBool,
) -> io::Result<bool> {
    let meta = std::fs::metadata(src)?;
    if same(&meta, dst) {
        done.fetch_add(meta.len(), Ordering::Relaxed);
    } else {
        copy_file(src, dst, done, cancel)?;
    }
    same_bytes(src, dst)
}

/// One file of a move at the start: `d` made `s`'s checked copy. A file already at `d` that
/// this move didn't write (`written`) is never replaced: it must already be the same.
fn move_file(
    s: &Path,
    d: &Path,
    key: &Path,
    written: &mut Vec<PathBuf>,
    done: &AtomicU64,
) -> Result<(), String> {
    let ours = written.iter().any(|w| w == key);
    if d.exists() && !ours {
        return match same_bytes(s, d) {
            Ok(true) => Ok(()),
            Ok(false) => Err(format!(
                "{} already exists and differs; not overwritten",
                d.display()
            )),
            Err(e) => Err(format!("checking {}: {e}", d.display())),
        };
    }
    if !ours {
        written.push(key.to_path_buf());
    }
    match copy_and_check(s, d, done, &AtomicBool::new(false)) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("{} differs after copying", d.display())),
        Err(e) => Err(format!("copying {}: {e}", key.display())),
    }
}

fn same_bytes(a: &Path, b: &Path) -> io::Result<bool> {
    let (mut fa, mut fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let n = read_full(&mut fa, &mut ba)?;
        let m = read_full(&mut fb, &mut bb)?;
        if n != m || ba[..n] != bb[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

fn read_full(f: &mut std::fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Remove `files` (relative to `src`), the store's writer locks, then every folder under `src`
/// (and `src` itself) left empty. Anything else stays. Returns how many entries couldn't go.
fn remove_moved(src: &Path, files: &[PathBuf]) -> usize {
    let mut left = 0;
    for rel in files {
        if std::fs::remove_file(src.join(rel)).is_err() {
            left += 1;
        }
    }
    // Folders deepest first; a folder with anything left in it stays.
    let mut dirs = vec![src.to_path_buf()];
    let mut i = 0;
    while i < dirs.len() {
        if let Ok(rd) = std::fs::read_dir(&dirs[i]) {
            for e in rd.flatten() {
                match e.file_type() {
                    Ok(t) if t.is_dir() => dirs.push(e.path()),
                    Ok(t) if t.is_file() && e.file_name() == "writer.lock" => {
                        let _ = std::fs::remove_file(e.path());
                    }
                    _ => {}
                }
            }
        }
        i += 1;
    }
    for d in dirs.iter().rev() {
        if std::fs::read_dir(d).is_ok_and(|mut r| r.next().is_none()) {
            let _ = std::fs::remove_dir(d);
        } else {
            left += 1;
        }
    }
    left
}

/// Whether another process is writing a contract of the store under `root` (its writer lock
/// is held).
fn store_in_use(root: &Path) -> bool {
    let store = root.join(STORE);
    let Ok(sources) = std::fs::read_dir(&store) else {
        return false;
    };
    for s in sources.flatten() {
        let Ok(contracts) = std::fs::read_dir(s.path()) else {
            continue;
        };
        for c in contracts.flatten() {
            let lock = c.path().join("writer.lock");
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&lock)
                && f.try_lock().is_err()
            {
                return true;
            }
        }
    }
    false
}

/// What a finished copy moved: each item's source and the files checked there.
#[derive(Debug, Default)]
pub struct Moved {
    items: Vec<(PathBuf, Vec<PathBuf>)>,
}

/// Where `name` is now, for a move out of `from`: `from/<name>`, else the config folder's.
fn source_of(from: &Path, config: Option<&Path>, name: &str) -> Option<PathBuf> {
    let d = from.join(name);
    if d.exists() {
        return Some(d);
    }
    config.map(|c| c.join(name)).filter(|c| c.exists())
}

/// Copy every item from `from` (and the config folder's `paper`/`journal`/`backups`) into `to`
/// and check every file, except big-item files the background copy checked (`record.files`)
/// still the same size and time in both folders. Never overwrites a file the move didn't write
/// (`record.written`); without a record, a `to` already holding Lines data is refused. An
/// item whose source and target are one folder (or one inside the other) is refused, and so
/// is a `from` without this move's marker (`id`, [`MARKER_FILE`]). Removes
/// nothing: see [`remove_old`]. `record` comes back with what this start wrote.
pub fn finish_move(
    from: &Path,
    id: &str,
    to: &Path,
    config: Option<&Path>,
    record: &mut Option<Verified>,
) -> Result<Moved, String> {
    if !from.is_dir() {
        return Err(format!("{} wasn't found", from.display()));
    }
    // Only the folder this move was chosen from: never one recreated empty at its path (a run
    // while it was missing), another disk given its letter, or a bare mount point.
    if !has_marker(from, id) {
        return Err(format!(
            "{} isn't the folder being moved from (its marker is missing)",
            from.display()
        ));
    }
    if same_place(from, to) {
        return Ok(Moved::default());
    }
    if inside(to, from) || inside(from, to) {
        return Err("one folder is inside the other".into());
    }
    if !to.is_dir() {
        return Err(format!("{} wasn't found", to.display()));
    }
    if store_in_use(from) {
        return Err(format!("another Lines is using {}", from.display()));
    }
    if record.is_none() {
        if let Some(n) = holds_data(to) {
            return Err(format!(
                "{} already holds Lines data ({}); nothing was overwritten",
                to.display(),
                item_words(n)
            ));
        }
        *record = Some(Verified {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
            ..Verified::default()
        });
    }
    let Some(rec) = record.as_mut() else {
        return Err("no record".into());
    };
    let trust: std::collections::HashMap<PathBuf, VerifiedFile> = rec
        .files
        .iter()
        .map(|f| (f.path.clone(), f.clone()))
        .collect();
    let mut items = Vec::new();
    for name in BIG {
        let s = from.join(name);
        if s.exists() {
            items.push((name, s, true));
        }
    }
    for name in SMALL {
        if let Some(s) = source_of(from, config, name) {
            items.push((name, s, false));
        }
    }
    let done = AtomicU64::new(0);
    let mut moved = Moved::default();
    for (name, src, big) in items {
        let dst = to.join(name);
        if overlaps(&src, &dst) {
            return Err(format!(
                "{} and {} are the same folder",
                src.display(),
                dst.display()
            ));
        }
        let files = walk(&src).map_err(|e| format!("reading {}: {e}", src.display()))?;
        for rel in &files {
            let (s, d) = (src.join(rel), dst.join(rel));
            let meta =
                std::fs::metadata(&s).map_err(|e| format!("reading {}: {e}", s.display()))?;
            let key = Path::new(name).join(rel);
            if big && trusted(trust.get(&key), &meta, &d) {
                continue;
            }
            move_file(&s, &d, &key, &mut rec.written, &done)?;
        }
        moved.items.push((src, files));
    }
    Ok(moved)
}

/// After the switch: remove what [`finish_move`] moved. A notice if anything stayed.
pub fn remove_old(m: &Moved) -> Option<String> {
    let mut left = Vec::new();
    for (src, files) in &m.items {
        if remove_moved(src, files) > 0 {
            left.push(src.display().to_string());
        }
    }
    (!left.is_empty()).then(|| {
        format!(
            "Data moved; some old files stayed in {} (not Lines' own, or in use)",
            left.join(", ")
        )
    })
}

// ---- startup ----------------------------------------------------------------------------

/// This run's data folder.
#[derive(Clone, Debug, Default)]
pub struct Run {
    pub by: By,
    pub os_default: Option<PathBuf>,
    pub locations: Locations,
    /// What the pointer says after startup (a new folder for the next start, a move waiting).
    pub pointer: Option<Pointer>,
    /// The row's problem from startup (a failed move, a folder not found).
    pub problem: Option<String>,
    /// With `LINES_DATA_DIR`: the folder Settings has saved (read only, for the row).
    pub saved: Option<PathBuf>,
}

/// Resolve this run's data folder and, with `finish_moves` (the interactive app), finish a
/// move chosen last run. Returns the run and what to tell the trader.
pub fn start(
    env: Option<PathBuf>,
    os_default: Option<PathBuf>,
    config: Option<&Path>,
    finish_moves: bool,
) -> (Run, Vec<String>) {
    let pointer = match (&env, &os_default) {
        (None, Some(d)) => read_pointer(&pointer_path(d)),
        _ => (None, None),
    };
    let mut saved = pointer.0.clone();
    // With LINES_DATA_DIR the pointer isn't used, only named on the row.
    let saved_folder = match (&env, &os_default) {
        (Some(_), Some(d)) => Some(
            read_pointer(&pointer_path(d))
                .0
                .map_or_else(|| d.clone(), |p| p.data_dir),
        ),
        _ => None,
    };
    let r = resolve(env, os_default.clone(), pointer);
    let mut notices: Vec<String> = r.notice.iter().cloned().collect();
    let mut problem = r.notice.clone();
    let mut root = r.root.clone();
    if let (Some(m), Some(to)) = (r.pending.clone(), r.root.clone()) {
        if !finish_moves {
            root = Some(m.from.clone());
        } else {
            // Before the logs open: the terminal is all there is.
            eprintln!(
                "Lines: finishing the data move from {} to {}",
                m.from.display(),
                to.display()
            );
            let mut record = os_default
                .as_deref()
                .and_then(|d| read_verified(d, &m.from, &to));
            let finished = finish_move(&m.from, &m.id, &to, config, &mut record);
            // What this start wrote stays recorded for the next try.
            if finished.is_err()
                && let (Some(d), Some(r)) = (os_default.as_deref(), &record)
            {
                let _ = write_verified(d, r);
            }
            match finished {
                Ok(moved) => {
                    let p = Pointer {
                        data_dir: to.clone(),
                        pending_move: None,
                    };
                    let written = os_default
                        .as_deref()
                        .ok_or_else(|| "no default data folder".to_owned())
                        .and_then(|d| write_pointer(d, &p).map_err(|e| e.to_string()));
                    match written {
                        Ok(()) => {
                            if let Some(d) = os_default.as_deref() {
                                remove_verified(d);
                            }
                            saved = Some(p);
                            notices.push(format!("Data moved to {}", to.display()));
                            remove_marker(&m.from);
                            if let Some(n) = remove_old(&moved) {
                                notices.push(n);
                            }
                        }
                        Err(e) => {
                            let n = format!(
                                "Failed: couldn't save the new data folder ({e}); still using {}",
                                m.from.display()
                            );
                            notices.push(n.clone());
                            problem = Some(n);
                            root = Some(m.from.clone());
                        }
                    }
                }
                Err(e) => {
                    let n = if m.from.is_dir() {
                        format!(
                            "Failed: couldn't move data ({e}) · still using {} · anything copied to {} is kept",
                            m.from.display(),
                            to.display()
                        )
                    } else {
                        // The folder being moved from is gone (a drive not plugged in). This run
                        // still points at it, so the move can finish when that folder, with its
                        // marker, is back; one this run recreates at the path has no marker and
                        // is never moved from. Nothing in it or in the copy is touched. Said
                        // plainly: what this run writes there starts empty.
                        let copy = if record.as_ref().is_some_and(|r| !r.files.is_empty()) {
                            format!("the copy checked so far is in {}", to.display())
                        } else {
                            format!("anything copied to {} is kept", to.display())
                        };
                        format!(
                            "Failed: couldn't move data ({} wasn't found) · this run uses an empty folder at {} · {copy}",
                            m.from.display(),
                            m.from.display()
                        )
                    };
                    notices.push(n.clone());
                    problem = Some(n);
                    root = Some(m.from.clone());
                }
            }
        }
    }
    let run = Run {
        by: r.by,
        os_default,
        locations: Locations::resolve(root.as_deref(), config),
        pointer: saved,
        problem,
        saved: saved_folder,
    };
    (run, notices)
}

static RUN: OnceLock<Run> = OnceLock::new();

/// Set this run's data folder (once, at startup, before anything opens a file there).
pub fn init(run: Run) {
    let _ = RUN.set(run);
}

fn env_dir() -> Option<PathBuf> {
    std::env::var_os("LINES_DATA_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
}

/// This run's data folder; before [`init`] (screenshots, tests, tools) `LINES_DATA_DIR`, else
/// the OS default, never the pointer (they read no user files).
pub fn run() -> Run {
    if let Some(r) = RUN.get() {
        return r.clone();
    }
    let env = env_dir();
    let by = if env.is_some() { By::Env } else { By::Default };
    let os_default = lines_feed::disk::os_default_root();
    let root = env.or_else(|| os_default.clone());
    Run {
        by,
        saved: os_default.clone().filter(|_| by == By::Env),
        os_default,
        locations: Locations::resolve(root.as_deref(), None),
        pointer: None,
        problem: None,
    }
}

/// [`init`], from the environment: what `run_with_args` calls.
pub fn init_from_env(config: Option<&Path>, finish_moves: bool) -> Vec<String> {
    let (r, notices) = start(
        env_dir(),
        lines_feed::disk::os_default_root(),
        config,
        finish_moves,
    );
    init(r);
    refresh_now();
    notices
}

pub fn root() -> Option<PathBuf> {
    run().locations.root
}

pub fn locations() -> Locations {
    run().locations
}

// ---- choosing a folder (Settings) -------------------------------------------------------

fn canon(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok()
}

fn parts(p: &Path) -> Vec<String> {
    p.components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy();
            // Windows and macOS (APFS, HFS+) folders are case-insensitive by default.
            if cfg!(any(windows, target_os = "macos")) {
                s.to_lowercase()
            } else {
                s.into_owned()
            }
        })
        .collect()
}

/// Both paths as they are on disk when both exist, else as given.
fn comparable(a: &Path, b: &Path) -> (Vec<String>, Vec<String>) {
    match (canon(a), canon(b)) {
        (Some(x), Some(y)) => (parts(&x), parts(&y)),
        _ => (parts(a), parts(b)),
    }
}

/// The same folder: the same file on Unix when both exist (whatever the names' case), else
/// the same canonical path (case ignored on Windows and macOS).
fn same_place(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(x), Ok(y)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            return (x.dev(), x.ino()) == (y.dev(), y.ino());
        }
    }
    let (x, y) = comparable(a, b);
    x == y
}

/// `a` is strictly inside `b`.
fn inside(a: &Path, b: &Path) -> bool {
    let a = canon(a).unwrap_or_else(|| a.to_path_buf());
    a.ancestors().skip(1).any(|p| same_place(p, b))
}

/// The same folder, or one inside the other.
fn overlaps(a: &Path, b: &Path) -> bool {
    same_place(a, b) || inside(a, b) || inside(b, a)
}

/// An item as the trader reads it: "its store", "paper accounts".
pub fn item_words(item: &str) -> &'static str {
    match item {
        STORE => "its store",
        LOGS => "logs",
        RECORDINGS => "recordings",
        PAPER => "paper accounts",
        JOURNAL => "the journal",
        _ => "backups",
    }
}

/// The first item `dir` holds with something in it.
pub fn holds_data(dir: &Path) -> Option<&'static str> {
    ITEMS
        .into_iter()
        .find(|n| std::fs::read_dir(dir.join(n)).is_ok_and(|mut r| r.next().is_some()))
}

/// Whether `chosen` may become the data folder: not the folder in use, not inside it, not
/// one containing it; nor the settings folder (`config`), inside it or containing it, unless
/// it is the OS default (on macOS the default and the settings folder are one folder).
pub fn check_target(
    in_use: &Path,
    chosen: &Path,
    config: Option<&Path>,
    os_default: Option<&Path>,
) -> Result<(), String> {
    if same_place(chosen, in_use) {
        return Err("That is the data folder already".into());
    }
    if inside(chosen, in_use) {
        return Err("That folder is inside the data folder".into());
    }
    if inside(in_use, chosen) {
        return Err("That folder contains the data folder".into());
    }
    if let Some(c) = config
        && !os_default.is_some_and(|d| same_place(d, chosen))
    {
        if same_place(chosen, c) {
            return Err("That is the settings folder".into());
        }
        if inside(chosen, c) {
            return Err("That folder is inside the settings folder".into());
        }
        if inside(c, chosen) {
            return Err("That folder contains the settings folder".into());
        }
    }
    Ok(())
}

/// A folder picked in Settings, checked (off the UI thread): `Err` refuses it; `Ok(Some)` is
/// why its data can't be moved there (it already holds Lines data; Start empty uses it).
pub fn check_pick(
    in_use: &Path,
    chosen: &Path,
    config: Option<&Path>,
    os_default: Option<&Path>,
) -> Result<Option<String>, String> {
    check_target(in_use, chosen, config, os_default)?;
    probe_writable(chosen)?;
    Ok(holds_data(chosen).map(|n| {
        format!(
            "Refused: {} already holds Lines data ({}) · moving never overwrites it",
            chosen.display(),
            item_words(n)
        )
    }))
}

/// Create a file in `dir` and remove it.
pub fn probe_writable(dir: &Path) -> Result<(), String> {
    if !dir.is_dir() {
        return Err(format!("{} isn't a folder", dir.display()));
    }
    let probe = dir.join(format!(".lines-probe-{}", std::process::id()));
    let wrote = std::fs::write(&probe, b"lines").map_err(|e| format!("can't write there: {e}"));
    let _ = std::fs::remove_file(&probe);
    wrote
}

/// Why data can't move now (a position or a working order is open), or `None`.
pub fn move_refusal(positions: usize, orders: usize) -> Option<String> {
    let count = |n: usize, one: &str, many: &str| match n {
        0 => None,
        1 => Some(format!("1 {one}")),
        n => Some(format!("{n} {many}")),
    };
    let parts: Vec<String> = [
        count(positions, "position open", "positions open"),
        count(orders, "working order", "working orders"),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| {
        format!(
            "Refused: not while a position or order is open ({})",
            parts.join(", ")
        )
    })
}

/// `text` split where it may wrap: at spaces, but never inside one of `paths` (a path with
/// spaces stays one piece, with what is stuck to it, such as "(" or ";").
pub fn wrap_tokens(text: &str, paths: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if text[i..].starts_with(' ') {
            i += 1;
            continue;
        }
        let word_end = text[i..].find(' ').map_or(text.len(), |k| i + k);
        let mut end = word_end;
        for p in paths.iter().filter(|p| p.contains(' ')) {
            if let Some(k) = text[i..].find(p.as_str())
                && i + k < word_end
            {
                let pe = i + k + p.len();
                end = end.max(text[pe..].find(' ').map_or(text.len(), |k| pe + k));
            }
        }
        out.push(text[i..end].to_owned());
        i = end;
    }
    out
}

/// A Data row line that is a failure or a refusal (its words say so; shown in the error color
/// too), not a pending or progress note.
pub fn is_problem(line: &str) -> bool {
    line.starts_with("Failed:") || line.starts_with("Refused:")
}

/// A background copy into the new folder.
#[derive(Debug, Default)]
pub struct Copy {
    pub to: PathBuf,
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub finished: AtomicBool,
    pub cancel: AtomicBool,
    pub error: Mutex<Option<String>>,
}

/// What this session changed: the pointer as written, a copy, a problem or a note.
#[derive(Default)]
struct Session {
    /// `None`: nothing written this session (the startup pointer stands).
    pointer: Option<Option<Pointer>>,
    copy: Option<Arc<Copy>>,
    problem: Option<String>,
    note: Option<String>,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
static REVISION: AtomicU64 = AtomicU64::new(0);

/// Moves on every change the Data folder row shows (the Settings window repaints on it).
pub fn revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

fn bump() {
    REVISION.fetch_add(1, Ordering::Relaxed);
}

fn with_session<R>(f: impl FnOnce(&mut Session) -> R) -> R {
    let r = {
        let mut g = SESSION.lock().unwrap_or_else(|e| e.into_inner());
        f(g.get_or_insert_with(Session::default))
    };
    bump();
    r
}

/// What the Data folder row shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FolderFacts {
    pub in_use: Option<PathBuf>,
    pub by: By,
    pub os_default: Option<PathBuf>,
    /// The folder the next start uses, when it isn't the one in use.
    pub next: Option<PathBuf>,
    /// `next` comes with the data (a move), else it starts empty.
    pub moving: bool,
    /// A background copy: bytes done, of total, finished.
    pub copy: Option<(u64, u64, bool)>,
    pub problem: Option<String>,
    pub note: Option<String>,
    /// `next` already holds Lines data: the next start uses it as it is.
    pub next_holds: bool,
    /// The folder in use is the OS default.
    pub on_default: bool,
    /// With `LINES_DATA_DIR`: the folder Settings has saved (the pointer's, else the default).
    pub saved: Option<PathBuf>,
}

impl FolderFacts {
    /// Settings may change the folder (not with `LINES_DATA_DIR`, not without a default).
    pub fn changeable(&self) -> bool {
        self.by != By::Env && self.os_default.is_some() && self.in_use.is_some()
    }

    pub fn pending(&self) -> bool {
        self.next.is_some()
    }

    /// "Use the default folder" (its button, Alt+D) is offered: changeable, nothing waiting,
    /// and the default isn't the folder in use.
    pub fn can_use_default(&self) -> bool {
        self.changeable() && !self.pending() && !self.on_default
    }

    /// The row's value: the folder in use.
    pub fn text(&self) -> String {
        match &self.in_use {
            None => "None (no data folder)".to_owned(),
            Some(d) => d.display().to_string(),
        }
    }

    /// Every path the row's lines can name (kept whole when the lines wrap).
    pub fn paths(&self) -> Vec<String> {
        let mut v: Vec<String> = [&self.in_use, &self.next, &self.os_default]
            .into_iter()
            .flatten()
            .map(|p| p.display().to_string())
            .collect();
        v.sort_by_key(|p| std::cmp::Reverse(p.len()));
        v
    }

    /// The row's status line: what waits for the restart, a copy's progress, a problem.
    pub fn status(&self) -> Option<String> {
        if self.by == By::Env {
            return Some(match &self.saved {
                Some(d) => format!(
                    "Set by LINES_DATA_DIR for this run · saved: {}",
                    d.display()
                ),
                None => "Set by LINES_DATA_DIR for this run".to_owned(),
            });
        }
        if let Some(p) = &self.problem {
            return Some(p.clone());
        }
        let next = self.next.as_ref().map(|n| n.display().to_string());
        let line = match (&next, self.moving, self.copy) {
            (Some(n), true, Some((done, total, false))) => {
                format!(
                    "Moving to {n} after restart · Copying {}",
                    amount(done, total)
                )
            }
            (Some(n), true, Some((_, total, true))) => format!(
                "Moving to {n} after restart · {} copied and checked",
                size(total)
            ),
            (Some(n), true, None) => format!("Moving to {n} after restart"),
            (Some(n), false, _) if self.next_holds => {
                format!("After restart: uses the data already in {n}")
            }
            (Some(n), false, _) => format!("{n} applies after restart (starts empty)"),
            (None, ..) => return self.note.clone(),
        };
        Some(line)
    }
}

const GIB: f64 = (1u64 << 30) as f64;
const MIB: f64 = (1u64 << 20) as f64;

/// "8.4 GiB", "450 MiB".
pub fn size(bytes: u64) -> String {
    if bytes as f64 >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB)
    } else {
        format!("{:.0} MiB", (bytes as f64 / MIB).ceil())
    }
}

/// "1.2 of 8.4 GiB", "120 of 450 MiB".
pub fn amount(done: u64, total: u64) -> String {
    if total as f64 >= GIB {
        format!("{:.1} of {:.1} GiB", done as f64 / GIB, total as f64 / GIB)
    } else {
        format!(
            "{:.0} of {:.0} MiB",
            (done as f64 / MIB).floor(),
            (total as f64 / MIB).ceil()
        )
    }
}

/// What the Data folder row shows about a folder that needs the disk to know: worked out off
/// the UI thread when the pointer's folder changes, read from here by every render.
#[derive(Clone, Debug, Default, PartialEq)]
struct Derived {
    /// The pointer's folder these are for.
    key: Option<PathBuf>,
    next_differs: bool,
    next_holds: bool,
    on_default: bool,
}

static DERIVED: Mutex<Option<Derived>> = Mutex::new(None);
static DERIVING: Mutex<Option<Option<PathBuf>>> = Mutex::new(None);

fn derive(run: &Run, key: Option<PathBuf>) -> Derived {
    let in_use = run.locations.root.as_deref();
    let next_differs = match (&key, in_use) {
        (Some(d), Some(u)) => !same_place(d, u),
        (Some(_), None) => true,
        _ => false,
    };
    Derived {
        next_holds: next_differs && key.as_deref().is_some_and(|d| holds_data(d).is_some()),
        on_default: match (in_use, run.os_default.as_deref()) {
            (Some(a), Some(b)) => same_place(a, b),
            _ => true,
        },
        next_differs,
        key,
    }
}

/// The pointer as this session last wrote it, else as the start found it.
fn current_pointer(run: &Run) -> Option<Pointer> {
    let g = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    match g.as_ref().and_then(|s| s.pointer.clone()) {
        Some(p) => p,
        None => run.pointer.clone(),
    }
}

/// Work out the disk facts for the current pointer now (startup and screenshot threads, before
/// any window: never the UI thread).
pub fn refresh_now() {
    let run = run();
    let key = current_pointer(&run).map(|p| p.data_dir);
    let d = derive(&run, key);
    *DERIVED.lock().unwrap_or_else(|e| e.into_inner()) = Some(d);
    bump();
}

/// Keep facts a worker worked out, only if they are for the key still being worked out (a
/// newer pointer's facts are never replaced by an older one's); that key is then done.
fn store_derived(d: Derived) -> bool {
    let mut busy = DERIVING.lock().unwrap_or_else(|e| e.into_inner());
    if busy.as_ref() != Some(&d.key) {
        return false;
    }
    *busy = None;
    *DERIVED.lock().unwrap_or_else(|e| e.into_inner()) = Some(d);
    drop(busy);
    bump();
    true
}

/// The cached disk facts for `key`, or (while a thread works them out) a guess from the paths
/// alone, never a disk call.
fn derived_for(run: &Run, key: Option<PathBuf>) -> Derived {
    if let Some(d) = DERIVED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|d| d.key == key)
    {
        return d.clone();
    }
    let mut busy = DERIVING.lock().unwrap_or_else(|e| e.into_inner());
    if busy.as_ref() != Some(&key) {
        *busy = Some(key.clone());
        let (run2, key2) = (run.clone(), key.clone());
        let _ = std::thread::Builder::new()
            .name("lines-data-facts".into())
            .spawn(move || {
                store_derived(derive(&run2, key2));
            });
    }
    let in_use = run.locations.root.clone();
    Derived {
        next_differs: key.is_some() && key != in_use,
        next_holds: false,
        on_default: in_use == run.os_default,
        key,
    }
}

/// What the Data folder row shows. No disk calls: the folders' facts come from a cache filled
/// off the UI thread.
pub fn facts() -> FolderFacts {
    let run = run();
    let pointer = current_pointer(&run);
    let key = pointer.as_ref().map(|p| p.data_dir.clone());
    let derived = derived_for(&run, key);
    let g = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let s = g.as_ref();
    let in_use = run.locations.root.clone();
    let next = pointer
        .as_ref()
        .filter(|_| run.by != By::Env && derived.next_differs)
        .map(|p| p.data_dir.clone());
    let moving = pointer.as_ref().is_some_and(|p| p.pending_move.is_some());
    let copy = s.and_then(|s| s.copy.as_ref()).map(|c| {
        (
            c.done.load(Ordering::Relaxed),
            c.total.load(Ordering::Relaxed),
            c.finished.load(Ordering::Relaxed),
        )
    });
    let copy_error = s
        .and_then(|s| s.copy.as_ref())
        .and_then(|c| c.error.lock().ok().and_then(|e| e.clone()));
    let session_problem = s.and_then(|s| s.problem.clone());
    // A startup problem stands until this session changes the folder.
    let startup_problem = run
        .problem
        .clone()
        .filter(|_| s.is_none_or(|s| s.pointer.is_none()));
    FolderFacts {
        in_use,
        by: run.by,
        os_default: run.os_default.clone(),
        next: next.clone(),
        moving: moving && next.is_some(),
        copy,
        problem: session_problem.or(copy_error).or(startup_problem),
        note: s.and_then(|s| s.note.clone()),
        next_holds: derived.next_holds && !moving,
        on_default: derived.on_default,
        saved: run.saved.clone(),
    }
}

/// Use `to` from the next start: with the data (`with_data`, copied now in the background and
/// finished at the next start) or empty. The pointer is written off the calling thread.
pub fn choose(to: PathBuf, with_data: bool) -> Result<(), String> {
    let run = run();
    if run.by == By::Env {
        return Err("Set by LINES_DATA_DIR for this run".into());
    }
    let os_default = run.os_default.clone().ok_or("No default data folder")?;
    let in_use = run.locations.root.clone().ok_or("No data folder")?;
    let pointer = Pointer {
        data_dir: to.clone(),
        pending_move: with_data.then(|| PendingMove {
            from: in_use.clone(),
            id: new_move_id(),
        }),
    };
    let copy = with_data.then(|| {
        Arc::new(Copy {
            to: to.clone(),
            ..Copy::default()
        })
    });
    let seq = next_seq();
    let before = with_session(|s| {
        if let Some(c) = &s.copy {
            c.cancel.store(true, Ordering::Relaxed);
        }
        let before = s.pointer.clone();
        s.pointer = Some(Some(pointer.clone()));
        s.copy = copy.clone();
        s.problem = None;
        s.note = None;
        before
    });
    let spawned = std::thread::Builder::new()
        .name("lines-data-move".into())
        .spawn(move || {
            let written = ordered(Some(seq), || {
                remove_verified(&os_default);
                // The source's marker first: a pointer naming a move always has one.
                match &pointer.pending_move {
                    Some(m) => write_marker(&m.from, &m.id)?,
                    None => remove_marker(&in_use),
                }
                write_pointer(&os_default, &pointer)
            });
            match written {
                // A newer choice was made meanwhile: it writes its own.
                None => return,
                Some(Err(e)) => {
                    with_session(|s| {
                        s.pointer = before;
                        s.copy = None;
                        s.problem =
                            Some(format!("Failed: couldn't save the new data folder ({e})"));
                    });
                    return;
                }
                Some(Ok(())) => {}
            }
            if let Some(c) = copy {
                background_copy(&in_use, &c, Some(&os_default), Some(seq));
            }
        });
    spawned
        .map(|_| ())
        .map_err(|e| format!("Failed: couldn't start ({e})"))
}

/// The big items, copied now and checked file by file, so the restart has little left to do.
/// What was written and checked (and didn't change meanwhile) is recorded beside the pointer
/// (`record`: the OS default folder) before the first file, every few seconds and when the copy
/// ends, unless a newer choice was made (`seq`; `None`: always, tests).
fn background_copy(from: &Path, c: &Copy, record: Option<&Path>, seq: Option<u64>) {
    let items: Vec<&str> = BIG.into_iter().filter(|n| from.join(n).exists()).collect();
    c.total.store(
        items.iter().map(|n| tree_size(&from.join(n))).sum(),
        Ordering::Relaxed,
    );
    bump();
    let mut v = Verified {
        from: from.to_path_buf(),
        to: c.to.clone(),
        ..Verified::default()
    };
    let save = |v: &Verified| {
        if let Some(d) = record {
            let _ = ordered(seq, || write_verified(d, v));
        }
    };
    let fail = |v: &Verified, e: String| {
        save(v);
        if let Ok(mut err) = c.error.lock() {
            *err = Some(format!(
                "Failed: copying to {} ({e}); tried again at restart",
                c.to.display()
            ));
        }
        bump();
    };
    save(&v);
    let mut saved_at = std::time::Instant::now();
    for name in items {
        let (src, dst) = (from.join(name), c.to.join(name));
        if overlaps(&src, &dst) {
            return fail(&v, format!("{} is the same folder", dst.display()));
        }
        let files = match walk(&src) {
            Ok(f) => f,
            Err(e) => return fail(&v, e.to_string()),
        };
        for rel in files {
            let (s, d) = (src.join(&rel), dst.join(&rel));
            let key = Path::new(name).join(&rel);
            match background_file(&s, &d, key, &mut v, c) {
                Ok(()) => {}
                Err(None) => {
                    save(&v);
                    bump();
                    return;
                }
                Err(Some(e)) => return fail(&v, e),
            }
            if saved_at.elapsed() >= std::time::Duration::from_secs(2) {
                save(&v);
                saved_at = std::time::Instant::now();
            }
        }
        save(&v);
    }
    c.finished.store(true, Ordering::Relaxed);
    bump();
}

/// One file of the background copy: copied (unless this move's copy is there already),
/// checked, and recorded as checked with both sides' size and time unless the source changed
/// meanwhile. A source gone since the listing (a rotated log, a date the store's budget
/// removed) is skipped. `Err(None)`: canceled; `Err(Some)`: the copy fails.
fn background_file(
    s: &Path,
    d: &Path,
    key: PathBuf,
    v: &mut Verified,
    c: &Copy,
) -> Result<(), Option<String>> {
    let gone = |e: &io::Error| e.kind() == io::ErrorKind::NotFound && !s.exists();
    let before = match std::fs::metadata(s) {
        Ok(m) => m,
        Err(e) if gone(&e) => return Ok(()),
        Err(e) => return Err(Some(e.to_string())),
    };
    let ours = v.written.contains(&key);
    let checked = if d.exists() && !ours {
        // Not this move's: never replaced (the folder was empty when picked).
        match same_bytes(s, d) {
            Ok(true) => Ok(true),
            Ok(false) => {
                return Err(Some(format!(
                    "{} already exists and differs; not overwritten",
                    d.display()
                )));
            }
            Err(e) if gone(&e) => return Ok(()),
            Err(e) => return Err(Some(e.to_string())),
        }
    } else {
        if !ours {
            v.written.push(key.clone());
        }
        copy_and_check(s, d, &c.done, &c.cancel)
    };
    let after = std::fs::metadata(s);
    // A file written to meanwhile (a growing segment, the log) isn't recorded: the
    // restart copies and checks it.
    let unchanged = match &after {
        Ok(b) => before.len() == b.len() && mtime_ns(&before) == mtime_ns(b),
        Err(_) => false,
    };
    match checked {
        Ok(true) if unchanged => {
            if let (Some(t), Ok(dm)) = (mtime_ns(&before), std::fs::metadata(d)) {
                v.files.push(VerifiedFile {
                    path: key,
                    len: before.len(),
                    mtime_ns: t,
                    dst_len: Some(dm.len()),
                    dst_mtime_ns: mtime_ns(&dm),
                });
            }
            Ok(())
        }
        Ok(true) => Ok(()),
        Ok(false) if !unchanged => Ok(()),
        Ok(false) => Err(Some(format!("{} differs after copying", d.display()))),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Err(None),
        Err(e) if gone(&e) => Ok(()),
        Err(e) => Err(Some(e.to_string())),
    }
}

/// Keep the folder in use: the pointer goes back to it, a copy stops (what it copied stays).
pub fn cancel() -> Result<(), String> {
    let run = run();
    let os_default = run.os_default.clone().ok_or("No default data folder")?;
    let in_use = run.locations.root.clone().ok_or("No data folder")?;
    let next = facts().next;
    let pointer = Pointer {
        data_dir: in_use.clone(),
        pending_move: None,
    };
    let marker_dir = in_use.clone();
    let seq = next_seq();
    let before = with_session(|s| {
        let had_copy = s
            .copy
            .take()
            .inspect(|c| c.cancel.store(true, Ordering::Relaxed));
        let before = s.pointer.clone();
        s.pointer = Some(Some(pointer.clone()));
        s.problem = None;
        s.note = Some(match (had_copy, next) {
            (Some(c), _) if c.done.load(Ordering::Relaxed) > 0 => format!(
                "Canceled · staying in {} · what was copied stays in {}",
                in_use.display(),
                c.to.display()
            ),
            _ => format!("Canceled; staying in {}", in_use.display()),
        });
        before
    });
    let spawned = std::thread::Builder::new()
        .name("lines-data-move".into())
        .spawn(move || {
            let written = ordered(Some(seq), || {
                remove_marker(&marker_dir);
                remove_verified(&os_default);
                write_pointer(&os_default, &pointer)
            });
            if let Some(Err(e)) = written {
                with_session(|s| {
                    s.pointer = before;
                    s.note = None;
                    s.problem = Some(format!("Failed: couldn't cancel ({e})"));
                });
            }
        });
    spawned
        .map(|_| ())
        .map_err(|e| format!("Failed: couldn't start ({e})"))
}

/// Screenshots: show a state without writing anything. `pending:PATH` (starts empty after
/// restart), `moving:PATH:DONE_GIB:TOTAL_GIB` (a copy in progress), `copied:PATH:TOTAL_GIB`,
/// `failed:PATH` (last start's move failed).
#[cfg(feature = "screenshot")]
pub fn show_for_screenshot(state: &str) -> Result<(), String> {
    let (kind, rest) = state.split_once(':').ok_or("expected KIND:PATH")?;
    let gb = |s: &str| -> Result<u64, String> {
        s.parse::<f64>()
            .map(|g| (g * GIB) as u64)
            .map_err(|_| format!("{s:?} isn't a number of GiB"))
    };
    let run = run();
    let from = run.locations.root.clone().unwrap_or_default();
    // A Windows path has a drive colon: the numbers come after the last colons.
    let mut fields: Vec<&str> = rest.rsplitn(3, ':').collect();
    fields.reverse();
    let (path, nums): (String, Vec<&str>) = match kind {
        "moving" if fields.len() == 3 => (fields[0].to_owned(), fields[1..].to_vec()),
        "copied" => {
            let (p, n) = rest
                .rsplit_once(':')
                .ok_or("expected copied:PATH:TOTAL_GB")?;
            (p.to_owned(), vec![n])
        }
        "pending" | "failed" => (rest.to_owned(), Vec::new()),
        _ => return Err(format!("unknown state {state:?}")),
    };
    let to = PathBuf::from(&path);
    let moving = kind != "pending";
    let copy = match kind {
        "moving" => Some(Arc::new(Copy {
            to: to.clone(),
            done: AtomicU64::new(gb(nums[0])?),
            total: AtomicU64::new(gb(nums[1])?),
            ..Copy::default()
        })),
        "copied" => Some(Arc::new(Copy {
            to: to.clone(),
            done: AtomicU64::new(gb(nums[0])?),
            total: AtomicU64::new(gb(nums[0])?),
            finished: AtomicBool::new(true),
            ..Copy::default()
        })),
        _ => None,
    };
    with_session(|s| {
        s.pointer = Some(Some(Pointer {
            data_dir: to.clone(),
            pending_move: moving.then(|| PendingMove {
                from: from.clone(),
                id: "screenshot".into(),
            }),
        }));
        s.copy = copy;
        s.problem = (kind == "failed").then(|| {
            format!(
                "Failed: couldn't move data ({} differs after copying) · still using {} · anything copied to {} is kept",
                to.join("logs").join("lines.log").display(),
                from.display(),
                to.display()
            )
        });
    });
    refresh_now();
    Ok(())
}

// ---- the store's settings ---------------------------------------------------------------

/// Settings › Data's store fields as saved (absent: the defaults).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StorePrefs {
    pub store: Option<bool>,
    pub budget_gb: Option<f64>,
    pub depth: Option<Vec<String>>,
}

impl StorePrefs {
    pub fn of(s: &crate::picker::Settings) -> StorePrefs {
        StorePrefs {
            store: s.store,
            budget_gb: s.store_budget_gb,
            depth: s.store_depth.clone(),
        }
    }

    /// From `settings.json`, read only (a file that can't be read gives the defaults; the app
    /// reports it when it loads the file properly).
    pub fn peek(path: &Path) -> StorePrefs {
        let Ok(bytes) = std::fs::read(path) else {
            return StorePrefs::default();
        };
        match versioned::decode::<crate::picker::Settings>(&crate::picker::SETTINGS_FORMAT, &bytes)
        {
            versioned::Decoded::Ok { value, .. } => StorePrefs::of(&value),
            _ => StorePrefs::default(),
        }
    }
}

/// The store flags given this run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoreFlags {
    pub no_store: bool,
    pub budget_gb: Option<f64>,
    pub depth: Option<Vec<String>>,
    /// `--store-dir`: a store for this run wherever the source is.
    pub store_dir: bool,
    /// Where `--store-dir` put it.
    pub store_dir_path: Option<PathBuf>,
}

/// The store this run keeps.
#[derive(Clone, Debug, PartialEq)]
pub struct StoreRun {
    pub on: bool,
    pub budget_gb: f64,
    pub depth: Vec<String>,
}

pub const DEFAULT_BUDGET_GB: f64 = 4.0;

/// A budget from `settings.json` that can be used (a hand-edited 0 or less is ignored).
fn usable(g: f64) -> Option<f64> {
    (g.is_finite() && g > 0.0).then_some(g)
}

/// Flags win over the saved settings for their run.
pub fn store_run(flags: &StoreFlags, prefs: &StorePrefs) -> StoreRun {
    StoreRun {
        on: !flags.no_store && (flags.store_dir || prefs.store != Some(false)),
        budget_gb: flags
            .budget_gb
            .or(prefs.budget_gb.and_then(usable))
            .unwrap_or(DEFAULT_BUDGET_GB),
        depth: flags
            .depth
            .clone()
            .or_else(|| prefs.depth.clone())
            .unwrap_or_default(),
    }
}

static STORE_RUN: OnceLock<(StorePrefs, StoreFlags)> = OnceLock::new();

/// What the store settings were at startup and which flags were given (the rows say "Applies
/// after restart" and "set by --… for this run" from these).
pub fn set_store_run(prefs: StorePrefs, flags: StoreFlags) {
    let _ = STORE_RUN.set((prefs, flags));
}

pub fn store_at_start() -> (StorePrefs, StoreFlags) {
    STORE_RUN.get().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests;
