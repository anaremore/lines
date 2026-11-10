//! "Start recording" / "Stop recording" (palette, the pane menu's Pane › Recording, the REC
//! indicator's Stop): every running live feed is recorded to the data directory's
//! `recordings/` as `<source>-<symbol>-<UTC time>.lfr`, the format `--record` writes, its
//! source in the file's header and name (decisions 0010, 0030); level sources are recorded
//! beside them (`levels-<UTC time>/`, as `--record-levels` writes). GPUI-free.
//!
//! Nothing here blocks: a start is a request each feed thread takes at its next connect (it
//! reconnects once, so its file holds a whole session), a stop closes files on their writer
//! threads (`lines_feed::live_record`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lines_core::FixedStr;
use lines_feed::live_record::RecState;
use lines_feed::store::ChartStore;

/// Screenshots (`--show-recording SECS`): the header as if recording for that long. Never
/// starts a recording (screenshots write no user files). -1: off.
static FIXTURE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

pub fn set_fixture(secs: Option<u64>) {
    FIXTURE.store(
        secs.map_or(-1, |s| s.min(i64::MAX as u64) as i64),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// The fixture's status and "now" (the elapsed time it shows), if set.
pub fn fixture() -> Option<(Status, i64)> {
    let secs = FIXTURE.load(std::sync::atomic::Ordering::Relaxed);
    (secs >= 0).then(|| {
        (
            Status {
                on: 1,
                since_ms: Some(0),
                ..Status::default()
            },
            secs.saturating_mul(1000),
        )
    })
}

/// What the window header shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Files being written.
    pub on: usize,
    /// Feeds about to start (they reconnect first).
    pub starting: usize,
    /// The earliest recording's start (wall clock, UTC ms): the elapsed time counts from it.
    pub since_ms: Option<i64>,
    /// A start that failed (e.g. the folder couldn't be written).
    pub failed: Option<String>,
}

impl Status {
    pub fn of<'a>(states: impl IntoIterator<Item = &'a RecState>) -> Status {
        let mut s = Status::default();
        for st in states {
            match st {
                RecState::On { since_ms, .. } => {
                    s.on += 1;
                    s.since_ms = Some(s.since_ms.map_or(*since_ms, |t| t.min(*since_ms)));
                }
                RecState::Starting => s.starting += 1,
                RecState::Failed(e) => s.failed = Some(e.clone()),
                RecState::Off => {}
            }
        }
        s
    }

    /// Recording or about to: the header shows REC and Stop, the menu's switch is on.
    pub fn active(&self) -> bool {
        self.on + self.starting > 0
    }
}

/// The recording status of `stores`.
pub fn status(stores: &[Arc<ChartStore>]) -> Status {
    let states: Vec<RecState> = stores.iter().map(|s| s.recording.state()).collect();
    Status::of(&states)
}

/// Where recordings go: `<data dir>/recordings`.
pub fn dir(data_root: &Path) -> PathBuf {
    data_root.join("recordings")
}

/// The levels folder for a recording started at `now_ms`: `levels-<UTC time>`.
pub fn levels_dir(recordings: &Path, now_ms: i64) -> PathBuf {
    let name = lines_feed::live_record::file_name("levels", "x", now_ms);
    // "levels-x-20261109T143005Z.lfr" -> "levels-20261109T143005Z"
    let stamp = name
        .trim_start_matches("levels-x-")
        .trim_end_matches(".lfr");
    recordings.join(format!("levels-{stamp}"))
}

/// Start recording every live feed in `stores` into `dir` (and live levels into a levels
/// folder beside them). Returns how many feeds will record (0: nothing live to record).
pub fn start(stores: &[Arc<ChartStore>], dir: &Path, now_ms: i64) -> usize {
    let n = stores
        .iter()
        .filter(|s| s.recording.start(dir.to_path_buf()))
        .count();
    if n > 0 {
        lines_feed::levels::runtime::record_into(Some(levels_dir(dir, now_ms)));
    }
    n
}

/// Stop every recording. Returns the files finished.
pub fn stop(stores: &[Arc<ChartStore>]) -> Vec<PathBuf> {
    lines_feed::levels::runtime::record_into(None);
    stores.iter().filter_map(|s| s.recording.stop()).collect()
}

/// The header's text: `REC 0:42`, `REC 12:05`, `REC 1:02:03` (elapsed since `since_ms`);
/// `REC` alone while starting.
pub fn label(since_ms: Option<i64>, now_ms: i64) -> FixedStr<16> {
    use std::fmt::Write;
    let mut s = FixedStr::<16>::new();
    let _ = s.write_str("REC");
    if let Some(t) = since_ms {
        let secs = ((now_ms - t).max(0) / 1000) as u64;
        let (h, m, sec) = (secs / 3600, secs / 60 % 60, secs % 60);
        let _ = if h > 0 {
            write!(s, " {h}:{m:02}:{sec:02}")
        } else {
            write!(s, " {m}:{sec:02}")
        };
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_counts_from_the_first_recording() {
        assert_eq!(label(None, 0).as_str(), "REC");
        assert_eq!(label(Some(0), 42_900).as_str(), "REC 0:42");
        assert_eq!(label(Some(0), 725_000).as_str(), "REC 12:05");
        assert_eq!(label(Some(0), 3_723_000).as_str(), "REC 1:02:03");
        assert_eq!(
            label(Some(10_000), 0).as_str(),
            "REC 0:00",
            "never negative"
        );
    }

    #[test]
    fn status_sums_the_feeds() {
        let on = |t| RecState::On {
            path: "x".into(),
            since_ms: t,
        };
        let s = Status::of(&[on(500), RecState::Off, on(200), RecState::Starting]);
        assert_eq!((s.on, s.starting, s.since_ms), (2, 1, Some(200)));
        assert!(s.active());
        let s = Status::of(&[RecState::Off, RecState::Failed("disk full".into())]);
        assert!(!s.active());
        assert_eq!(s.failed.as_deref(), Some("disk full"));
    }

    #[test]
    fn paths_live_in_the_data_dir() {
        let d = dir(Path::new("/data"));
        assert_eq!(d, Path::new("/data/recordings"));
        let t = lines_core::UtcMillis::parse_iso("2026-11-09T14:30:05Z")
            .unwrap()
            .0;
        assert_eq!(
            levels_dir(&d, t),
            Path::new("/data/recordings/levels-20261109T143005Z")
        );
    }
}
