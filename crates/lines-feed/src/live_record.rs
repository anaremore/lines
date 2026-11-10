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
    /// The feed is live (not a replay) and running: it can be recorded.
    recordable: bool,
    /// A start asked for, into this directory.
    pending: Option<PathBuf>,
    /// The feed thread took the start and is creating the file; a stop meanwhile cancels it.
    creating: bool,
    cancelled: bool,
    active: Option<Active>,
    error: Option<String>,
}

/// What a stop did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// Not recording.
    Nothing,
    /// A start that hadn't begun yet was called off.
    Cancelled,
    /// This file is finished.
    Saved(PathBuf),
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
        if !s.recordable || s.pending.is_some() || s.creating || s.active.is_some() {
            return false;
        }
        s.pending = Some(dir);
        s.error = None;
        drop(s);
        // Wakes the running connection (or the next one: the permit is kept).
        self.restart.notify_one();
        true
    }

    /// Stop recording: the file is finished now; the feed keeps running. A start not begun
    /// yet (the feed is reconnecting) is called off, also while its file is being created.
    pub fn stop(&self) -> Stopped {
        let mut s = self.state.lock();
        if let Some(a) = s.active.take() {
            a.recorder.stop();
            return Stopped::Saved(a.path);
        }
        if s.pending.take().is_some() {
            return Stopped::Cancelled;
        }
        if s.creating {
            s.cancelled = true;
            return Stopped::Cancelled;
        }
        Stopped::Nothing
    }

    /// The last start's failure, once (the app shows it where the recording was started).
    pub fn take_error(&self) -> Option<String> {
        self.state.lock().error.take()
    }

    pub fn state(&self) -> RecState {
        let s = self.state.lock();
        match (&s.active, s.pending.is_some() || s.creating, &s.error) {
            (Some(a), _, _) => RecState::On {
                path: a.path.clone(),
                since_ms: a.since_ms,
            },
            (None, true, _) => RecState::Starting,
            (None, false, Some(e)) => RecState::Failed(e.clone()),
            (None, false, None) => RecState::Off,
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

    /// The feed thread, at a connect: a start asked for (it is creating the file until
    /// [`RecordCtl::set_active`] or [`RecordCtl::fail`]).
    pub(crate) fn take_pending(&self) -> Option<PathBuf> {
        let mut s = self.state.lock();
        let p = s.pending.take();
        s.creating = p.is_some();
        s.cancelled = false;
        p
    }

    /// The file is open. `false` when a stop came while it was being created: the recorder
    /// is stopped and not used.
    pub(crate) fn set_active(&self, path: PathBuf, since_ms: i64, recorder: Arc<Recorder>) -> bool {
        let mut s = self.state.lock();
        s.creating = false;
        if std::mem::take(&mut s.cancelled) {
            recorder.stop();
            return false;
        }
        s.active = Some(Active {
            path,
            since_ms,
            recorder,
        });
        s.error = None;
        true
    }

    pub(crate) fn fail(&self, e: String) {
        let mut s = self.state.lock();
        s.creating = false;
        s.cancelled = false;
        s.error = Some(e);
    }

    /// The feed has stopped: nothing more can be recorded; a recording ends.
    pub(crate) fn close(&self) {
        let mut s = self.state.lock();
        s.recordable = false;
        s.pending = None;
        s.creating = false;
        let a = s.active.take();
        drop(s);
        if let Some(a) = a {
            a.recorder.stop();
        }
    }

    /// The feed thread, at a roll: the recording ends (a file holds one contract).
    pub(crate) fn end(&self) {
        // Taken under the lock, stopped and dropped outside it (a drop may join the writer).
        let a = self.state.lock().active.take();
        if let Some(a) = a {
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

/// Create `dir` and claim a fresh file in it (a numbered suffix if the name is taken). The
/// file is created here (`create_new`), so two feeds starting in the same second with the same
/// name never share one file.
pub(crate) fn fresh_path(dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stem = name.trim_end_matches(".lfr");
    for n in 1..1000 {
        let p = if n == 1 {
            dir.join(name)
        } else {
            dir.join(format!("{stem}-{n}.lfr"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(_) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
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
        // Claimed at once: a second claim in the same instant gets the next name.
        let a = fresh_path(&dir, "x.lfr").unwrap();
        let b = fresh_path(&dir, "x.lfr").unwrap();
        assert_eq!(a.file_name().unwrap(), "x.lfr");
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
        assert_eq!(c.state(), RecState::Starting, "creating the file");
        c.fail("disk full".into());
        assert_eq!(c.state(), RecState::Failed("disk full".into()));
        assert_eq!(c.take_error().as_deref(), Some("disk full"), "shown once");
        assert_eq!(c.take_error(), None);
        assert_eq!(c.stop(), Stopped::Nothing);
        // A stop before the feed took the start calls it off.
        assert!(c.start("d".into()));
        assert_eq!(c.stop(), Stopped::Cancelled);
        assert_eq!(c.take_pending(), None);
        // A feed that has stopped can't be recorded.
        c.close();
        assert!(!c.start("d".into()));
    }

    /// A stop while the feed thread creates the file: the recording never becomes active.
    #[test]
    fn a_stop_during_creation_wins() {
        let dir = std::env::temp_dir().join(format!("lines-rec-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let c = RecordCtl::default();
        c.set_recordable(true);
        assert!(c.start(dir.clone()));
        let d = c.take_pending().unwrap();
        assert_eq!(c.stop(), Stopped::Cancelled);
        let path = d.join("x.lfr");
        let r = Arc::new(Recorder::create(&path, "cqg", "p", "ES", None).unwrap());
        assert!(!c.set_active(path, 0, r.clone()));
        assert!(r.is_stopped());
        assert_eq!(c.state(), RecState::Off);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
