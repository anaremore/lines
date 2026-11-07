//! The journal file: `<config dir>/journal/<source>.jsonl`.
//!
//! JSON Lines, append-only. The first line is a versioned header
//! (`{"journal":2,"source":"mock"}`); every later line is one [`Record`]: a closed trade, or
//! an edit of one (tags, note; the last one wins). Nothing is ever rewritten in place, with one
//! exception: the version migration below.
//!
//! - **v1 → v2** (decision 0028): v2 trades may name equity and option contracts, so an older
//!   build must refuse the file instead of reading an option as spot. Every v1 record is a
//!   valid v2 record. Opening a v1 file for writing first copies it aside
//!   (`<stem>.v1.jsonl`, never overwritten), then replaces it atomically with the same bytes
//!   under a v2 header; reading a v1 file needs nothing.
//!
//! - **Torn tail**: a crash can leave a last line without its newline, or cut mid-record.
//!   [`parse`] drops it (and reports it); opening the file for writing cuts it off first, so
//!   the next record starts on a fresh line. A bad line elsewhere is skipped and counted.
//! - **Newer versions** are refused (the file is left as it is and nothing is written).
//! - **Writes** happen on the writer's own thread ([`JournalWriter`]), never the UI's or the
//!   trading core's: append, flush, and `sync_data` at most once a second and on close.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::Record;

pub const VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub journal: u32,
    pub source: String,
}

/// What a journal file held.
#[derive(Debug, Default, PartialEq)]
pub struct Loaded {
    pub records: Vec<Record>,
    /// Bad lines skipped (not counting a torn tail).
    pub skipped: usize,
    /// The last line was incomplete and was dropped.
    pub torn: bool,
    /// Bytes up to the end of the last complete line (where appending resumes).
    pub valid_len: u64,
    /// The header's version (0: no header yet).
    pub version: u32,
}

/// Parse a journal file's bytes. `Err` for a missing/garbled header or a newer version.
pub fn parse(bytes: &[u8]) -> Result<Loaded> {
    let mut out = Loaded::default();
    let mut pos = 0usize;
    let mut first = true;
    while pos < bytes.len() {
        let Some(nl) = bytes[pos..].iter().position(|&b| b == b'\n') else {
            // No newline: a torn last line (never complete, whatever it parses as).
            out.torn = true;
            break;
        };
        let line = &bytes[pos..pos + nl];
        let end = pos + nl + 1;
        if first {
            let h: Header = serde_json::from_slice(line).context("journal header")?;
            if h.journal > VERSION {
                bail!(
                    "journal version {} is newer than this build reads ({VERSION})",
                    h.journal
                );
            }
            out.version = h.journal;
            first = false;
        } else if !line.iter().all(u8::is_ascii_whitespace) {
            match serde_json::from_slice::<Record>(line) {
                Ok(r) => out.records.push(r),
                Err(e) => {
                    tracing::warn!("journal: skipped a bad line: {e}");
                    out.skipped += 1;
                }
            }
        }
        out.valid_len = end as u64;
        pos = end;
    }
    // No complete line at all (an empty file, or one torn while writing its header): an empty
    // journal; the writer starts it again from byte 0.
    Ok(out)
}

/// Read a journal file; `Ok(None)` if there is none yet.
pub fn read(path: &Path) -> Result<Option<Loaded>> {
    match fs::read(path) {
        Ok(b) => parse(&b).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Appends records on its own thread. Cheap to clone; dropping the last clone flushes,
/// syncs and stops the thread.
#[derive(Clone)]
pub struct JournalWriter {
    tx: Sender<Job>,
}

enum Job {
    Write(Record),
    /// Sync, then reply.
    Flush(Sender<()>),
}

impl JournalWriter {
    /// Open (or create) `path` for appending: a new file gets its header; a torn tail is cut
    /// off first. The file is read again here (not trusted from `_loaded`, which may be older
    /// than the file), and every write appends, so nothing complete is ever cut or overwritten.
    pub fn open(path: PathBuf, source: &str, _loaded: Option<&Loaded>) -> Result<JournalWriter> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let parsed = parse(&bytes)?;
        let valid = parsed.valid_len;
        let len = bytes.len() as u64;
        if valid < len {
            tracing::warn!(
                "journal {}: cut a torn tail of {} bytes",
                path.display(),
                len - valid
            );
            // An append-only handle can't truncate (Windows): cut with a plain write handle.
            OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_len(valid))
                .with_context(|| format!("cut the torn tail of {}", path.display()))?;
        }
        if parsed.version != 0 && parsed.version < VERSION {
            migrate(&path, parsed.version, &bytes[..valid as usize], source)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        if file.metadata()?.len() == 0 {
            let h = Header {
                journal: VERSION,
                source: source.to_owned(),
            };
            let mut line = serde_json::to_vec(&h)?;
            line.push(b'\n');
            file.write_all(&line)?;
            file.sync_data()?;
        }
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("lines-journal".into())
            .spawn(move || write_loop(file, path, rx))?;
        Ok(JournalWriter { tx })
    }

    /// Queue a record (returns at once).
    pub fn append(&self, r: Record) {
        let _ = self.tx.send(Job::Write(r));
    }

    /// Wait (up to `timeout`) until everything queued so far is written and synced.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Job::Flush(tx)).is_ok() && rx.recv_timeout(timeout).is_ok()
    }
}

/// Bring an older journal to [`VERSION`]: keep a copy as it was, then swap in the same records
/// under the current header (written whole beside it, then renamed: a crash leaves either
/// file, never half of one).
fn migrate(path: &Path, from: u32, valid: &[u8], source: &str) -> Result<()> {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let aside = path.with_file_name(format!("{stem}.v{from}.jsonl"));
    if !aside.exists() {
        fs::write(&aside, valid).with_context(|| format!("keep {}", aside.display()))?;
    }
    let body = valid
        .iter()
        .position(|&b| b == b'\n')
        .map_or(&[][..], |nl| &valid[nl + 1..]);
    let mut out = serde_json::to_vec(&Header {
        journal: VERSION,
        source: source.to_owned(),
    })?;
    out.push(b'\n');
    out.extend_from_slice(body);
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut f = File::create(&tmp).with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(&out)?;
        f.sync_data()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    tracing::info!(
        "journal {}: migrated v{from} → v{VERSION} (the original is kept as {})",
        path.display(),
        aside.display()
    );
    Ok(())
}

fn write_loop(mut file: File, path: PathBuf, rx: Receiver<Job>) {
    let mut last_sync = Instant::now();
    let mut unsynced = false;
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Job::Flush(tx)) => {
                let _ = file.sync_data();
                unsynced = false;
                let _ = tx.send(());
            }
            Ok(Job::Write(r)) => match serde_json::to_vec(&r) {
                Ok(mut line) => {
                    line.push(b'\n');
                    if let Err(e) = file.write_all(&line).and_then(|_| file.flush()) {
                        tracing::error!("journal {}: write failed: {e}", path.display());
                    }
                    unsynced = true;
                }
                Err(e) => tracing::error!("journal: can't encode a record: {e}"),
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if unsynced && last_sync.elapsed() >= Duration::from_secs(1) {
            let _ = file.sync_data();
            unsynced = false;
            last_sync = Instant::now();
        }
    }
    let _ = file.sync_data();
}
