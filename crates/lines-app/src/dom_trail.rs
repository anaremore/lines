//! The DOM price trail: a "motion blur" in its own narrow column beside the price, showing
//! where price just traded, with volume as intensity (model: `lines_core::trail`, pure).
//!
//! - **Input**: only the prints the store counted as new (its time & sales ring, the same
//!   trades as `tape`/`vap`), taken by sequence number at paint, so a re-delivered trade
//!   never counts twice. A pane that wasn't painting (hidden, minimized) catches up from the
//!   ring when shown; prints older than ten half-lives are skipped (they would be invisible).
//! - **Clock**: the feed's own clock (`ChartStore::feed_clock`: frame or market time, running
//!   on only while the feed is live), never the wall clock, so replays and screenshots are
//!   deterministic and a finished replay is a still picture.
//! - **Frames**: while some visible cell is above the visibility threshold and the feed clock
//!   moves, the pane asks for a frame when it hasn't painted for [`FRAME_EVERY`] (about
//!   30 Hz, counting paints for new data too, so the trail only fills gaps); then it stops. Hidden and minimized panes never get here (their poll doesn't run).
//! - **Paint**: fill only, no text. Net bought rows use the up color, net sold rows the down
//!   color, mixed rows blend toward a neutral gray; a chevron on the newest trade's row points
//!   the way the last price moved (direction by shape too, docs/design.md §4). No allocation.
//!
//! Off by default; saved per DOM as the optional `trail` field (enabled, half-life).

use std::time::{Duration, Instant};

use gpui::{Bounds, Window, fill, point, px, size};
use lines_core::UtcMillis;
use lines_core::ladder::LadderRow;
use lines_core::trail::{self, DEFAULT_HALF_LIFE_MS, Move, Trail};
use lines_feed::ChartData;
use lines_feed::store::ChartStore;
use serde::{Deserialize, Serialize};

use crate::theme::Theme;

/// Fastest the trail asks for frames while it fades.
pub const FRAME_EVERY: Duration = Duration::from_millis(33);
/// Prints older than this many half-lives are invisible: never replayed into the trail.
const KEEP_HALF_LIVES: i64 = 12;

/// What is saved for a DOM's trail (only when it differs from off at the default half-life).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrailDoc {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_half_life")]
    pub half_life_ms: u32,
}

fn default_half_life() -> u32 {
    DEFAULT_HALF_LIFE_MS
}

impl Default for TrailDoc {
    fn default() -> Self {
        TrailDoc {
            enabled: false,
            half_life_ms: DEFAULT_HALF_LIFE_MS,
        }
    }
}

pub struct DomTrail {
    enabled: bool,
    model: Trail,
    /// The store's print counter already taken; `None`: catch up from the ring.
    seen: Option<u64>,
    /// The feed clock at the last paint.
    painted: Option<UtcMillis>,
    /// When the DOM last painted or the trail last asked for a frame (rate cap only; never
    /// affects values): the trail only fills gaps between data frames.
    asked: Option<Instant>,
}

impl DomTrail {
    pub fn new(units: i64) -> DomTrail {
        DomTrail {
            enabled: false,
            model: Trail::new(units, DEFAULT_HALF_LIFE_MS),
            seen: None,
            painted: None,
            asked: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn half_life_ms(&self) -> u32 {
        self.model.half_life_ms()
    }

    pub fn doc(&self) -> Option<TrailDoc> {
        let d = TrailDoc {
            enabled: self.enabled,
            half_life_ms: self.model.half_life_ms(),
        };
        (d != TrailDoc::default()).then_some(d)
    }

    pub fn restore(&mut self, doc: Option<TrailDoc>) {
        let d = doc.unwrap_or_default();
        self.enabled = d.enabled;
        self.model.set_half_life(d.half_life_ms);
        self.reset();
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        self.reset();
    }

    /// The next half-life preset (wrapping). Values decayed so far stay as they are.
    pub fn cycle_half_life(&mut self) -> u32 {
        let next = trail::next_half_life(self.model.half_life_ms());
        self.model.set_half_life(next);
        next
    }

    /// Start over from the store's ring (a new feed, grouping or setting).
    pub fn reset(&mut self) {
        let units = self.model.units();
        self.model.clear(units);
        self.seen = None;
        self.painted = None;
    }

    /// Under the store lock in paint: take the prints counted since the last frame (oldest
    /// first) and move to the feed clock. `units`: this frame's row height in price units.
    pub fn prepare(&mut self, d: &ChartData, units: i64, clock: Option<UtcMillis>) {
        if !self.enabled {
            return;
        }
        if units != self.model.units() {
            self.model.clear(units);
            self.seen = None;
        }
        let tape = &d.tape;
        let pushed = tape.pushed();
        let mut n = match self.seen {
            Some(s) => pushed.saturating_sub(s).min(tape.len() as u64) as usize,
            None => {
                // Catch up: only prints recent enough to still show.
                let horizon = i64::from(self.model.half_life_ms()) * KEEP_HALF_LIVES;
                let now = clock.or_else(|| tape.newest(0).map(|p| p.time));
                match now {
                    Some(now) => (0..tape.len())
                        .take_while(|&i| {
                            tape.newest(i).is_some_and(|p| p.time.0 >= now.0 - horizon)
                        })
                        .count(),
                    None => 0,
                }
            }
        };
        self.seen = Some(pushed);
        while n > 0 {
            n -= 1;
            if let Some(p) = tape.newest(n) {
                self.model.trade(p.time, p.price, p.qty, p.aggressor);
            }
        }
        if let Some(c) = clock {
            self.model.advance(c);
        }
        self.painted = clock;
        // A paint for new data counts against the frame cap too.
        self.asked = Some(Instant::now());
    }

    /// The pane's poll (every 8 ms, visible panes only): ask for a frame while the trail is
    /// fading, at most every [`FRAME_EVERY`]; never while the feed clock stands still.
    pub fn wants_frame(&mut self, store: &ChartStore, column_shown: bool) -> bool {
        if !self.enabled || !column_shown {
            return false;
        }
        let Some(now) = store.feed_clock() else {
            return false;
        };
        if self.painted.is_some_and(|p| p.0 >= now.0) {
            return false; // nothing moved since the last frame
        }
        if self.asked.is_some_and(|a| a.elapsed() < FRAME_EVERY) {
            return false;
        }
        if !self.model.needs_animation(now) {
            return false;
        }
        self.asked = Some(Instant::now());
        true
    }

    /// Paint the trail column for the visible rows (`cell`: the column's x and width; rows
    /// start at `oy`). Fill only; no allocation.
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &self,
        rows: &[LadderRow],
        cell: Option<(f32, f32)>,
        oy: f32,
        row_h: f32,
        bottom: f32,
        t: &Theme,
        window: &mut Window,
    ) {
        let (true, Some((x, w))) = (self.enabled, cell) else {
            return;
        };
        let rect =
            |x: f32, y: f32, w: f32, h: f32| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
        let newest = self.model.newest();
        for (i, r) in rows.iter().enumerate() {
            let y = oy + i as f32 * row_h;
            if y + row_h > bottom + 0.5 {
                break;
            }
            let row = self.model.row_of(r.price);
            let c = self.model.cell(row);
            if c.alpha > 0.0 {
                let (dir, share) = cell_colors(c.net, t);
                let b = rect(x, y, w, row_h);
                // Mixed rows lean toward neutral: the neutral share first, then the side.
                if share < 1.0 {
                    window.paint_quad(fill(b, t.text_dim.opacity(c.alpha * (1.0 - share))));
                }
                if share > 0.0 {
                    window.paint_quad(fill(b, dir.opacity(c.alpha * share)));
                }
            }
            if let Some((nr, Some(m))) = newest
                && nr == row
            {
                paint_chevron(x, y, w, row_h, m, t, window);
            }
        }
    }
}

/// The side color of a row with balance `net` (`-1..=1`) and how much of it to use (0 =
/// neutral, 1 = all one side).
fn cell_colors(net: f32, t: &Theme) -> (gpui::Hsla, f32) {
    let dir = if net >= 0.0 { t.up } else { t.down };
    (dir, net.abs().min(1.0))
}

/// A small chevron centered in the cell, pointing the way the last price moved: stacked
/// one-pixel bars, widest at the base (quads only, no path: no allocation).
fn paint_chevron(x: f32, y: f32, w: f32, h: f32, m: Move, t: &Theme, window: &mut Window) {
    const BARS: usize = 4;
    let scale = window.scale_factor();
    let bar = (1.0 / scale).max(1.0);
    let max_w = (w - 4.0).clamp(2.0, 9.0);
    let (cx, cy) = (x + w * 0.5, y + h * 0.5);
    let top = cy - bar * BARS as f32 * 0.5;
    for k in 0..BARS {
        // Up: the tip (narrowest bar) on top; down: at the bottom.
        let from_tip = match m {
            Move::Up => k,
            Move::Down => BARS - 1 - k,
        };
        let bw = max_w * (from_tip + 1) as f32 / BARS as f32;
        let by = (top + k as f32 * bar) * scale;
        window.paint_quad(fill(
            Bounds::new(
                point(px(cx - bw * 0.5), px(by.round() / scale)),
                size(px(bw), px(bar)),
            ),
            t.text_main,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::tape::Aggressor;
    use lines_core::{Capabilities, Price, Print, Qty};
    use lines_feed::catalog;
    use lines_feed::store::FeedStatus;

    fn store() -> ChartStore {
        let es = catalog::get(catalog::ES).unwrap();
        ChartStore::new(es, None, Capabilities::default(), "test", 60_000, 10)
    }

    fn print(t: i64, ticks: i64, q: i64) -> Print {
        Print {
            time: UtcMillis(t),
            price: Price(600_000 + ticks * 25),
            qty: Qty(q),
            aggressor: Some(Aggressor::Buy),
        }
    }

    #[test]
    fn saved_only_when_set() {
        let mut t = DomTrail::new(25);
        assert_eq!(t.doc(), None);
        t.set_enabled(true);
        assert_eq!(
            t.doc(),
            Some(TrailDoc {
                enabled: true,
                half_life_ms: 1_500
            })
        );
        assert_eq!(t.cycle_half_life(), 3_000);
        let json = serde_json::to_string(&t.doc().unwrap()).unwrap();
        assert_eq!(json, r#"{"enabled":true,"half_life_ms":3000}"#);
        let mut u = DomTrail::new(25);
        u.restore(Some(serde_json::from_str(r#"{"enabled":true}"#).unwrap()));
        assert!(
            u.enabled() && u.half_life_ms() == 1_500,
            "half-life defaults"
        );
        u.restore(Some(
            serde_json::from_str(r#"{"half_life_ms":99999}"#).unwrap(),
        ));
        assert!(!u.enabled() && u.half_life_ms() == trail::MAX_HALF_LIFE_MS);
    }

    /// Each counted print is taken once; prints older than the trail could show are not
    /// replayed when catching up.
    #[test]
    fn takes_each_counted_print_once() {
        let s = store();
        let mut t = DomTrail::new(25);
        t.set_enabled(true);
        {
            let mut d = s.read();
            d.tape.push(print(0, 0, 50)); // far older than 12 half-lives
            d.tape.push(print(60_000, 1, 2));
            d.tape.push(print(60_100, 2, 2));
        }
        t.prepare(&s.read(), 25, Some(UtcMillis(60_100)));
        assert_eq!(
            t.model.cell(t.model.row_of(Price(600_000))).alpha,
            0.0,
            "stale print skipped"
        );
        let a = t.model.cell(t.model.row_of(Price(600_050))).alpha;
        assert!(a > 0.0);
        // Nothing new: preparing again adds nothing.
        t.prepare(&s.read(), 25, Some(UtcMillis(60_100)));
        assert_eq!(t.model.cell(t.model.row_of(Price(600_050))).alpha, a);
    }

    /// The poll asks for frames while the trail fades on a live clock, at most every 33 ms,
    /// and stops: once nothing is visible, or when the feed clock stands still (a finished
    /// replay), or when the column or the trail is off.
    #[test]
    fn asks_for_frames_only_while_fading() {
        let s = store();
        let mut t = DomTrail::new(25);
        t.set_enabled(true);
        s.set_frame_time(UtcMillis(10_000));
        {
            let mut d = s.read();
            d.tape.push(print(10_000, 0, 5));
        }
        s.update_frame(|_| ());
        t.prepare(&s.read(), 25, s.feed_clock());
        // Not live: the clock is frozen at the frame time, so nothing moves.
        assert!(!t.wants_frame(&s, true));
        s.set_status(FeedStatus::Live);
        std::thread::sleep(Duration::from_millis(5));
        t.asked = None; // as if the last paint were more than 33 ms ago
        assert!(t.wants_frame(&s, true));
        assert!(!t.wants_frame(&s, true), "rate capped");
        // A data paint counts against the cap as well.
        t.asked = None;
        t.prepare(&s.read(), 25, Some(UtcMillis(10_001)));
        assert!(!t.wants_frame(&s, true), "just painted for new data");
        assert!(!t.wants_frame(&s, false), "column hidden");
        // About ten half-lives later on the feed's clock: faded, no more frames.
        s.set_frame_time(UtcMillis(10_000 + 11 * 1_500));
        s.update_frame(|_| ());
        t.asked = None;
        assert!(!t.wants_frame(&s, true));
        t.set_enabled(false);
        assert!(!t.wants_frame(&s, true));
    }
}
