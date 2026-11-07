//! The journal file: `<config dir>/journal/<source>.jsonl`.
//!
//! JSON Lines, append-only. The first line is a versioned header
//! (`{"journal":1,"source":"mock"}`); every later line is one [`Record`]: a closed trade, or
//! an edit of one (tags, note; the last one wins). Nothing is ever rewritten in place.
//!
//! - **Torn tail**: a crash can leave a last line without its newline, or cut mid-record.
//!   [`parse`] drops it (and reports it); opening the file for writing cuts it off first, so
//!   the next record starts on a fresh line. A bad line elsewhere is skipped and counted.
//! - **Newer versions** are refused (the file is left as it is and nothing is written).
//! - **Writes** happen on the writer's own thread ([`JournalWriter`]), never the UI's or the
//!   trading core's: append, flush, and `sync_data` at most once a second and on close.

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::Record;

pub const VERSION: u32 = 1;

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
    tx: Sender<Record>,
}

impl JournalWriter {
    /// Open (or create) `path` for appending: a new file gets its header; a torn tail
    /// (`loaded.valid_len` short of the file's length) is cut off first.
    pub fn open(path: PathBuf, source: &str, loaded: Option<&Loaded>) -> Result<JournalWriter> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(false)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let len = file.metadata()?.len();
        match loaded {
            Some(l) if l.valid_len < len => {
                tracing::warn!(
                    "journal {}: cut a torn tail of {} bytes",
                    path.display(),
                    len - l.valid_len
                );
                file.set_len(l.valid_len)?;
            }
            _ => {}
        }
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
        file.seek(SeekFrom::End(0))?;
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("lines-journal".into())
            .spawn(move || write_loop(file, path, rx))?;
        Ok(JournalWriter { tx })
    }

    /// Queue a record (returns at once).
    pub fn append(&self, r: Record) {
        let _ = self.tx.send(r);
    }
}

fn write_loop(mut file: File, path: PathBuf, rx: Receiver<Record>) {
    let mut last_sync = Instant::now();
    let mut unsynced = false;
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(r) => match serde_json::to_vec(&r) {
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
