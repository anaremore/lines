//! Start and stop recording a running feed (the app's "Start recording" / "Stop recording").
//!
//! A recording is the same `.lfr` file `--record` writes: every raw inbound frame, its header
//! naming the provider (the source: `cqg`, `coinbase`, an extension's name; decisions 0010
//! and 0030: a recording of a licensed source is stamped with it), product and contract.
//!
//! - **Start** ([`RecordCtl::start`]) asks the feed to reconnect once with a recorder: the file
//!   begins with a whole session (logon, subscriptions, snapshots), so it replays exactly like
//!   one recorded from launch. The UI never waits: the request is a flag and a wake-up, and the
//!   feed thread creates the file at its next connect.
//! - **Stop** ([`RecordCtl::stop`]) finishes the file at once (its writer thread closes it);
//!   the connection goes on.
//! - A roll to the next contract stops the recording (a file holds one contract).
//!
//! Replays are never recorded again ([`RecordCtl::start`] refuses: nothing live).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::recording::Recorder;

/// What the UI shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecState {
    Off,
    /// Asked for; the feed creates the file at its next connect.
    Starting,
    /// Recording to `path` since `since_ms` (wall clock, UTC ms).
    On {
        path: PathBuf,
        since_ms: i64,
    },
    /// The last start failed (the file couldn't be created).
    Failed(String),
}

#[derive(Default)]
struct State {
    /// The feed is live (not a replay): it can be recorded.
    recordable: bool,
    /// A start asked for, into this directory.
    pending: Option<PathBuf>,
    active: Option<Active>,
    error: Option<String>,
}

struct Active {
    path: PathBuf,
    since_ms: i64,
    recorder: Arc<Recorder>,
}

/// A feed's recording switch (held by its `ChartStore`).
#[derive(Default)]
pub struct RecordCtl {
    state: Mutex<State>,
    restart: Notify,
}

impl std::fmt::Debug for RecordCtl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RecordCtl").field(&self.state()).finish()
    }
}

impl RecordCtl {
    /// Ask to record into `dir` (created if missing). `false` when there is nothing live to
    /// record (a replay) or it is already recording or starting.
    pub fn start(&self, dir: PathBuf) -> bool {
        let mut s = self.state.lock();
        if !s.recordable || s.pending.is_some() || s.active.is_some() {
            return false;
        }
        s.pending = Some(dir);
        s.error = None;
        drop(s);
        // Wakes the running connection (or the next one: the permit is kept).
        self.restart.notify_one();
        true
    }

    /// Stop recording: the file is finished now; the feed keeps running. Returns its path.
    pub fn stop(&self) -> Option<PathBuf> {
        let mut s = self.state.lock();
        s.pending = None;
        let a = s.active.take()?;
        a.recorder.stop();
        Some(a.path)
    }

    pub fn state(&self) -> RecState {
        let s = self.state.lock();
        match (&s.active, &s.pending, &s.error) {
            (Some(a), _, _) => RecState::On {
                path: a.path.clone(),
                since_ms: a.since_ms,
            },
            (None, Some(_), _) => RecState::Starting,
            (None, None, Some(e)) => RecState::Failed(e.clone()),
            (None, None, None) => RecState::Off,
        }
    }

    /// Whether this feed can be recorded (live, not a replay).
    pub fn recordable(&self) -> bool {
        self.state.lock().recordable
    }

    pub(crate) fn set_recordable(&self, on: bool) {
        self.state.lock().recordable = on;
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.state.lock().pending.is_some()
    }

    /// The feed thread, at a connect: a start asked for.
    pub(crate) fn take_pending(&self) -> Option<PathBuf> {
        self.state.lock().pending.take()
    }

    pub(crate) fn set_active(&self, path: PathBuf, since_ms: i64, recorder: Arc<Recorder>) {
        let mut s = self.state.lock();
        s.active = Some(Active {
            path,
            since_ms,
            recorder,
        });
        s.error = None;
    }

    pub(crate) fn fail(&self, e: String) {
        self.state.lock().error = Some(e);
    }

    /// The feed thread, at a roll: the recording ends (a file holds one contract).
    pub(crate) fn end(&self) {
        if let Some(a) = self.state.lock().active.take() {
            a.recorder.stop();
        }
    }

    /// Resolves when a start asks the connection to reconnect with a recorder.
    pub(crate) async fn restarted(&self) {
        self.restart.notified().await
    }
}

/// A recording's file name: `<source>-<symbol>-<UTC time>.lfr`, e.g.
/// `coinbase-BTC-USD-20261109T143005Z.lfr` (characters a file name can't hold become `-`).
pub fn file_name(source: &str, symbol: &str, now_ms: i64) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    };
    let t = lines_core::UtcMillis(now_ms).to_string();
    // "2026-11-09T14:30:05.000Z" (or without millis) -> "20261109T143005Z"
    let stamp: String = t
        .chars()
        .take_while(|c| *c != '.' && *c != 'Z')
        .filter(|c| c.is_ascii_digit() || *c == 'T')
        .collect();
    format!("{}-{}-{stamp}Z.lfr", clean(source), clean(symbol))
}

/// Create `dir` and a fresh path in it (a numbered suffix if the name is taken).
pub(crate) fn fresh_path(dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let p = dir.join(name);
    if !p.exists() {
        return Ok(p);
    }
    let stem = name.trim_end_matches(".lfr");
    for n in 2..1000 {
        let p = dir.join(format!("{stem}-{n}.lfr"));
        if !p.exists() {
            return Ok(p);
        }
    }
    Err(std::io::Error::other("no free recording name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_carry_source_symbol_and_utc_time() {
        let t = lines_core::UtcMillis::parse_iso("2026-11-09T14:30:05Z")
            .unwrap()
            .0;
        assert_eq!(
            file_name("cqg", "ESZ26", t),
            "cqg-ESZ26-20261109T143005Z.lfr"
        );
        assert_eq!(
            file_name("coinbase", "BTC/USD", t + 250),
            "coinbase-BTC-USD-20261109T143005Z.lfr"
        );
        assert_eq!(
            file_name("acme feed", "NQ H7", t),
            "acme-feed-NQ-H7-20261109T143005Z.lfr"
        );
    }

    #[test]
    fn a_taken_name_gets_a_number() {
        let dir = std::env::temp_dir().join(format!("lines-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = fresh_path(&dir, "x.lfr").unwrap();
        std::fs::write(&a, b"").unwrap();
        let b = fresh_path(&dir, "x.lfr").unwrap();
        assert_eq!(b.file_name().unwrap(), "x-2.lfr");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nothing live (a replay): no start. A second start while one is pending: refused.
    #[test]
    fn start_needs_a_live_feed_and_happens_once() {
        let c = RecordCtl::default();
        assert!(!c.start("d".into()), "a replay is never recorded again");
        c.set_recordable(true);
        assert!(c.start("d".into()));
        assert_eq!(c.state(), RecState::Starting);
        assert!(!c.start("d".into()));
        assert_eq!(c.take_pending(), Some(PathBuf::from("d")));
        c.fail("disk full".into());
        assert_eq!(c.state(), RecState::Failed("disk full".into()));
        assert_eq!(c.stop(), None);
    }
}
