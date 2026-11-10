//! Runtime control of a replay (the replay bar): play, pause, speed and seek.
//!
//! One control is shared by the UI (which changes it), the replay transport (which paces
//! frames by it), the feed's store and the trading core (whose market clocks read it). So a
//! paused replay's clock stands still everywhere: no paper order, timeout or staleness moves
//! while nothing plays.
//!
//! Every seek ends paused at its target. A seek ahead delivers the frames up to the target
//! at once. A seek back is a rewind: the feed clears its data, restarts the file and
//! delivers up to the target the same way (`run_feed`).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use lines_core::UtcMillis;
use tokio::sync::Notify;
use tokio::time::Instant;

/// The speeds the bar offers, slowest first. `0.0` is Max: frames as fast as they decode.
pub const SPEEDS: [f64; 5] = [1.0, 2.0, 10.0, 60.0, 0.0];

/// What the transport does with the frame it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pace {
    /// Deliver it now.
    Deliver,
    /// Drop it and end the stream: the feed restarts the file (a seek back).
    Rewind,
}

/// The replay's state as the bar shows it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snapshot {
    pub paused: bool,
    /// One of [`SPEEDS`].
    pub speed: f64,
    /// The replay clock now (see [`ReplayControl::clock`]).
    pub clock: Option<UtcMillis>,
    /// The first and last recorded frame times, once known.
    pub first: Option<UtcMillis>,
    pub last: Option<UtcMillis>,
    /// A seek in progress, to this time.
    pub seeking: Option<UtcMillis>,
    /// The file played to its end.
    pub finished: bool,
    /// How many times the replay jumped back (the trading core lets its clock go back once
    /// per rewind).
    pub rewinds: u64,
}

#[derive(Debug)]
struct State {
    paused: bool,
    speed: f64,
    seek: Option<UtcMillis>,
    rewind: bool,
    /// The newest delivered frame's time.
    position: Option<UtcMillis>,
    first: Option<UtcMillis>,
    last: Option<UtcMillis>,
    /// The clock read `.1` at `.0`; it runs at `speed` from there while playing.
    anchor: Option<(Instant, i64)>,
    finished: bool,
    rewinds: u64,
    /// Screenshots: report playing while the clock holds.
    shown_playing: bool,
}

impl State {
    fn clock(&self, now: Instant) -> Option<UtcMillis> {
        // Jumping back: the clock reads the target until the restarted file's first frame.
        if self.rewind {
            return self.seek;
        }
        let Some((at, base)) = self.anchor else {
            return self.seek;
        };
        let ran = if self.paused || self.seek.is_some() || self.rewind {
            0
        } else {
            // Max runs on in real time between frames (and after the end), as a live clock.
            let rate = if self.speed > 0.0 { self.speed } else { 1.0 };
            let ms = now.saturating_duration_since(at).as_secs_f64() * 1000.0 * rate;
            ms.min(i64::MAX as f64 / 2.0) as i64
        };
        let t = base.saturating_add(ran);
        Some(UtcMillis(self.position.map_or(t, |p| t.max(p.0))))
    }

    /// Hold the clock where it reads now (before pausing or changing speed).
    fn rebase(&mut self, now: Instant) {
        if let Some(t) = self.clock(now) {
            self.anchor = Some((now, t.0));
        }
    }
}

/// The one of [`SPEEDS`] nearest `speed` (`0.0` or less: Max).
pub fn nearest_speed(speed: f64) -> f64 {
    if speed.is_nan() || speed <= 0.0 {
        return 0.0;
    }
    if speed.is_infinite() {
        return 60.0;
    }
    SPEEDS
        .iter()
        .copied()
        .filter(|s| *s > 0.0)
        .min_by(|a, b| (a - speed).abs().total_cmp(&(b - speed).abs()))
        .unwrap_or(1.0)
}

#[derive(Debug)]
pub struct ReplayControl {
    state: Mutex<State>,
    notify: Notify,
    /// Moves on every change the bar shows.
    revision: AtomicU64,
}

impl Default for ReplayControl {
    fn default() -> Self {
        ReplayControl::new(1.0)
    }
}

impl ReplayControl {
    /// A control playing at `speed` (`0.0` or less: Max): the nearest of [`SPEEDS`], so the
    /// bar always shows the speed in force and steps from it.
    pub fn new(speed: f64) -> ReplayControl {
        ReplayControl {
            state: Mutex::new(State {
                paused: false,
                speed: nearest_speed(speed),
                seek: None,
                rewind: false,
                position: None,
                first: None,
                last: None,
                anchor: None,
                finished: false,
                rewinds: 0,
                shown_playing: false,
            }),
            notify: Notify::new(),
            revision: AtomicU64::new(0),
        }
    }

    fn change<R>(&self, f: impl FnOnce(&mut State, Instant) -> R) -> R {
        let r = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut st, Instant::now())
        };
        self.revision.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_waiters();
        r
    }

    fn read<R>(&self, f: impl FnOnce(&State) -> R) -> R {
        f(&self.state.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Lock-free change detector for the bar.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> Snapshot {
        let now = Instant::now();
        self.read(|st| Snapshot {
            paused: st.paused && !st.shown_playing,
            speed: st.speed,
            clock: st.clock(now),
            first: st.first,
            last: st.last,
            seeking: st.seek,
            finished: st.finished,
            rewinds: st.rewinds,
        })
    }

    /// The replay clock: the market time it has played to, running at the replay's speed
    /// while it plays and standing still while it is paused or seeking. While a seek back
    /// waits for the file to restart, the seek's target. `None` before the first frame.
    pub fn clock(&self) -> Option<UtcMillis> {
        let now = Instant::now();
        self.read(|st| st.clock(now))
    }

    /// How many times the replay jumped back.
    pub fn rewinds(&self) -> u64 {
        self.read(|st| st.rewinds)
    }

    pub fn set_paused(&self, paused: bool) {
        self.change(|st, now| {
            if st.paused != paused {
                st.rebase(now);
                st.paused = paused;
            }
        });
    }

    pub fn toggle(&self) {
        self.change(|st, now| {
            st.rebase(now);
            st.paused = !st.paused;
        });
    }

    /// Set the speed (`0.0`: Max); the clock keeps its time.
    pub fn set_speed(&self, speed: f64) {
        self.change(|st, now| {
            st.rebase(now);
            st.speed = speed.max(0.0);
        });
    }

    /// The next speed in [`SPEEDS`] (`up`: faster), stopping at either end.
    pub fn step_speed(&self, up: bool) {
        let cur = self.read(|st| st.speed);
        let i = SPEEDS.iter().position(|s| *s == cur).unwrap_or(0);
        let j = if up {
            (i + 1).min(SPEEDS.len() - 1)
        } else {
            i.saturating_sub(1)
        };
        self.set_speed(SPEEDS[j]);
    }

    /// Go to `t` and pause there. Before the current clock this is a rewind; at or after the
    /// file's end it only moves the clock. `true` if it is a rewind.
    pub fn seek(&self, t: UtcMillis) -> bool {
        self.change(|st, now| {
            st.rebase(now);
            st.paused = true;
            // A rewind still waiting for the file to restart: its target is now `t`.
            if st.rewind {
                st.seek = Some(t);
                return true;
            }
            let at = st.clock(now);
            let back = at.is_some_and(|c| t.0 < c.0);
            if back {
                st.rewind = true;
                st.rewinds += 1;
                st.seek = Some(t);
            } else if st.finished {
                // Nothing left to deliver: the clock moves to `t` and holds.
                st.anchor = Some((now, t.0));
            } else {
                st.seek = Some(t);
            }
            back
        })
    }

    /// Screenshots: show the playing state with the clock held where it is (a picture of the
    /// bar playing, deterministic).
    pub fn show_playing(&self) {
        self.change(|st, _| st.shown_playing = true);
    }

    /// Transport: the replay's first recorded frame time.
    pub fn set_first(&self, t: UtcMillis) {
        self.change(|st, _| {
            if st.first.is_none_or(|f| t.0 < f.0) {
                st.first = Some(t);
            }
        });
    }

    /// The replay's last recorded frame time (a scan of the file).
    pub fn set_last(&self, t: UtcMillis) {
        self.change(|st, _| st.last = Some(t));
    }

    /// Transport: what to do with the frame recorded at `wall` (ms), waiting as long as the
    /// replay's pace (or a pause) says. Cancel-safe: the transport keeps the frame until
    /// this returns [`Pace::Deliver`].
    pub async fn pace(&self, wall: i64) -> Pace {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wait = {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                if st.rewind {
                    return Pace::Rewind;
                }
                let due = match (st.seek, st.anchor) {
                    // A seek delivers everything up to its target at once, then pauses there.
                    (Some(t), _) if wall <= t.0 => Some(now),
                    (Some(t), _) => {
                        st.seek = None;
                        st.paused = true;
                        let at = st.position.map_or(t.0, |p| t.0.max(p.0));
                        st.anchor = Some((now, at));
                        drop(st);
                        self.revision.fetch_add(1, Ordering::AcqRel);
                        continue;
                    }
                    _ if st.paused => None,
                    // The first frame (and the first after a rewind) sets the clock.
                    (None, None) => Some(now),
                    _ if st.speed <= 0.0 => Some(now),
                    (None, Some((at, base))) => {
                        let ahead = wall.saturating_sub(base).max(0) as f64 / st.speed;
                        // At most a year ahead (a corrupt file can put frames anywhere).
                        Some(at + Duration::from_secs_f64((ahead / 1000.0).min(31_536_000.0)))
                    }
                };
                match due {
                    Some(d) if d <= now => {
                        let paced = st.seek.is_none() && st.speed > 0.0 && st.anchor.is_some();
                        if !paced {
                            st.anchor = Some((now, wall));
                        }
                        st.position = Some(UtcMillis(st.position.map_or(wall, |p| p.0.max(wall))));
                        // The timeline starts at the first frame delivered (a live recording
                        // starts with outbound frames, which are never delivered).
                        if st.first.is_none_or(|f| wall < f.0) {
                            st.first = Some(UtcMillis(wall));
                        }
                        drop(st);
                        self.revision.fetch_add(1, Ordering::AcqRel);
                        return Pace::Deliver;
                    }
                    other => other,
                }
            };
            match wait {
                None => notified.await,
                Some(due) => {
                    tokio::select! {
                        _ = tokio::time::sleep_until(due) => {}
                        _ = notified => {}
                    }
                }
            }
        }
    }

    /// Transport: the file ended.
    pub fn set_finished(&self) {
        self.change(|st, now| {
            // A jump back that came as the file ended keeps its target for the restart.
            if st.rewind {
                return;
            }
            if let Some(t) = st.seek.take() {
                // A seek past the end: the clock goes to the target and holds there.
                st.anchor = Some((now, st.position.map_or(t.0, |p| t.0.max(p.0))));
            }
            st.finished = true;
        });
    }

    /// Feed: whether a rewind is waiting (the transport ended for it).
    pub fn rewind_requested(&self) -> bool {
        self.read(|st| st.rewind)
    }

    /// Feed: wait until a rewind is requested (a finished replay waits here for the bar).
    pub async fn wait_for_rewind(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.rewind_requested() {
                return;
            }
            notified.await;
        }
    }

    /// Feed: the file restarts from its start for a rewind (its seek target stays).
    pub fn restarted(&self) {
        self.change(|st, _| {
            st.rewind = false;
            st.position = None;
            st.anchor = None;
            st.finished = false;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
    }

    /// Frames come at the recorded pace times the speed, and the clock runs at that speed.
    #[test]
    fn frames_are_paced_by_the_speed() {
        rt().block_on(async {
            let c = ReplayControl::new(10.0);
            let t0 = Instant::now();
            assert_eq!(c.pace(1_000_000).await, Pace::Deliver);
            assert_eq!(c.clock(), Some(UtcMillis(1_000_000)));
            assert_eq!(c.pace(1_010_000).await, Pace::Deliver);
            // 10 s of market at 10x: 1 s of wall time.
            assert_eq!(t0.elapsed(), Duration::from_secs(1));
            tokio::time::advance(Duration::from_millis(500)).await;
            assert_eq!(c.clock(), Some(UtcMillis(1_015_000)));
        });
    }

    /// A paused replay delivers nothing and its clock stands still; playing again picks up
    /// from there.
    #[test]
    fn a_pause_stops_the_frames_and_the_clock() {
        rt().block_on(async {
            let c = std::sync::Arc::new(ReplayControl::new(1.0));
            assert_eq!(c.pace(1_000_000).await, Pace::Deliver);
            tokio::time::advance(Duration::from_millis(400)).await;
            c.set_paused(true);
            let held = c.clock();
            assert_eq!(held, Some(UtcMillis(1_000_400)));
            let c2 = c.clone();
            let next = tokio::spawn(async move { c2.pace(1_001_000).await });
            tokio::time::advance(Duration::from_secs(60)).await;
            assert!(!next.is_finished(), "nothing plays while paused");
            assert_eq!(c.clock(), held, "the clock stands still");
            c.set_paused(false);
            tokio::time::advance(Duration::from_millis(600)).await;
            assert_eq!(next.await.unwrap(), Pace::Deliver);
            assert_eq!(c.clock(), Some(UtcMillis(1_001_000)));
        });
    }

    /// A seek ahead delivers up to its target at once, then pauses at the target with the
    /// next frame held back.
    #[test]
    fn a_seek_ahead_runs_to_its_target_and_pauses() {
        rt().block_on(async {
            let c = std::sync::Arc::new(ReplayControl::new(1.0));
            assert_eq!(c.pace(1_000_000).await, Pace::Deliver);
            assert!(!c.seek(UtcMillis(1_060_000)));
            let t0 = Instant::now();
            assert_eq!(c.pace(1_030_000).await, Pace::Deliver);
            assert_eq!(c.pace(1_060_000).await, Pace::Deliver);
            assert_eq!(t0.elapsed(), Duration::ZERO, "no pacing while seeking");
            let c2 = c.clone();
            let next = tokio::spawn(async move { c2.pace(1_060_001).await });
            tokio::time::advance(Duration::from_secs(5)).await;
            assert!(!next.is_finished(), "paused at the target");
            let s = c.snapshot();
            assert!(s.paused && s.seeking.is_none());
            assert_eq!(s.clock, Some(UtcMillis(1_060_000)));
            next.abort();
        });
    }

    /// A seek back is a rewind: the held frame is dropped, the feed restarts the file, and
    /// the restarted file runs up to the target.
    #[test]
    fn a_seek_back_is_a_rewind() {
        rt().block_on(async {
            let c = ReplayControl::new(1.0);
            assert_eq!(c.pace(1_000_000).await, Pace::Deliver);
            assert_eq!(c.pace(1_090_000).await, Pace::Deliver);
            assert!(c.seek(UtcMillis(1_050_000)));
            assert_eq!(c.pace(1_090_001).await, Pace::Rewind);
            assert!(c.rewind_requested());
            assert_eq!(c.rewinds(), 1);
            assert_eq!(
                c.clock(),
                Some(UtcMillis(1_050_000)),
                "the target, while rewinding"
            );
            c.restarted();
            assert_eq!(
                c.clock(),
                Some(UtcMillis(1_050_000)),
                "until the first frame"
            );
            assert_eq!(c.pace(1_000_000).await, Pace::Deliver);
            assert_eq!(c.pace(1_050_000).await, Pace::Deliver);
            assert_eq!(c.clock(), Some(UtcMillis(1_050_000)));
        });
    }

    /// The clock never runs behind the newest delivered frame, and Max delivers at once.
    #[test]
    fn max_delivers_at_once() {
        rt().block_on(async {
            let c = ReplayControl::new(0.0);
            let t0 = Instant::now();
            for i in 0..5 {
                assert_eq!(c.pace(1_000_000 + i * 60_000).await, Pace::Deliver);
            }
            assert_eq!(t0.elapsed(), Duration::ZERO);
            assert_eq!(c.clock(), Some(UtcMillis(1_240_000)));
        });
    }

    /// The first frame delivered starts the timeline; a speed off the list is the nearest.
    #[test]
    fn the_first_frame_and_odd_speeds() {
        rt().block_on(async {
            let c = ReplayControl::new(5.0);
            assert_eq!(c.snapshot().speed, 2.0, "5x shows as the nearest, 2x");
            assert_eq!(c.pace(1_000_000).await, Pace::Deliver);
            assert_eq!(c.snapshot().first, Some(UtcMillis(1_000_000)));
        });
        assert_eq!(nearest_speed(5.0), 2.0);
        assert_eq!(nearest_speed(7.0), 10.0);
        assert_eq!(nearest_speed(0.0), 0.0);
        assert_eq!(nearest_speed(-1.0), 0.0);
        assert_eq!(nearest_speed(1000.0), 60.0);
        assert_eq!(nearest_speed(f64::INFINITY), 60.0);
    }

    /// A jump back as the file ends keeps its target; a second jump while it waits retargets.
    #[test]
    fn a_rewind_waiting_keeps_its_latest_target() {
        rt().block_on(async {
            let c = ReplayControl::new(1.0);
            assert_eq!(c.pace(1_000_000).await, Pace::Deliver);
            assert_eq!(c.pace(1_090_000).await, Pace::Deliver);
            assert!(c.seek(UtcMillis(1_050_000)));
            c.set_finished();
            assert!(c.rewind_requested());
            assert!(c.seek(UtcMillis(1_060_000)));
            assert_eq!(c.snapshot().seeking, Some(UtcMillis(1_060_000)));
            c.restarted();
            assert_eq!(c.pace(1_000_000).await, Pace::Deliver);
            assert_eq!(c.pace(1_060_000).await, Pace::Deliver);
            assert_eq!(c.clock(), Some(UtcMillis(1_060_000)));
        });
    }

    /// A frame stamped at the far end of the time range (a corrupt file) neither panics nor
    /// comes early: the wait is capped, never overflowed.
    #[test]
    fn a_frame_far_ahead_waits_without_overflow() {
        rt().block_on(async {
            let c = std::sync::Arc::new(ReplayControl::new(1.0));
            assert_eq!(c.pace(i64::MIN / 2).await, Pace::Deliver);
            let c2 = c.clone();
            let far = tokio::spawn(async move { c2.pace(i64::MAX).await });
            tokio::time::advance(Duration::from_secs(3600)).await;
            assert!(!far.is_finished(), "an hour in, it still waits");
            far.abort();
        });
    }

    /// Speeds step through the list and stop at either end.
    #[test]
    fn speeds_step_and_stop_at_the_ends() {
        let c = ReplayControl::new(1.0);
        c.step_speed(false);
        assert_eq!(c.snapshot().speed, 1.0);
        for want in [2.0, 10.0, 60.0, 0.0, 0.0] {
            c.step_speed(true);
            assert_eq!(c.snapshot().speed, want);
        }
    }

    /// After the end, a seek ahead only moves the clock; a seek back still rewinds.
    #[test]
    fn a_finished_replay_still_seeks() {
        rt().block_on(async {
            let c = ReplayControl::new(1.0);
            assert_eq!(c.pace(1_000_000).await, Pace::Deliver);
            c.set_finished();
            assert!(!c.seek(UtcMillis(2_000_000)));
            assert_eq!(c.clock(), Some(UtcMillis(2_000_000)));
            assert!(c.seek(UtcMillis(1_000_000)));
            assert!(c.rewind_requested());
        });
    }
}
