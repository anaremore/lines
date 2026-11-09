//! The option chart (decision 0029), without GPUI: where a chain's rows go when they share the
//! underlying chart's price scale. A strike's row is centered on that strike's price, by the
//! chart's own `Viewport::price_to_y`, so zoom and pan can't pull them apart. Rows that wouldn't
//! fit side by side give way, nearest the money last; the selected strike and strikes the account
//! holds or works come first. A strike without room has no row, so nothing is half drawn.

use lines_core::{Price, Viewport};

/// The chart's scale as the chain reads it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LockedScale {
    pub vp: Viewport,
    /// The chart plot's top, in window coordinates.
    pub plot_top: f32,
}

impl LockedScale {
    /// The window y of `price` on the chart.
    pub fn y(&self, price: Price) -> f32 {
        self.plot_top + self.vp.price_to_y(price.0 as f64) as f32
    }

    /// The scale as a value: when it changes, the rows are placed again.
    pub fn key(&self) -> (f64, f64, f32, f64) {
        (
            self.vp.price_lo,
            self.vp.price_hi,
            self.plot_top,
            self.vp.height,
        )
    }

    /// Where the plot is: when this changes every row moves, though the prices don't. Leaves out
    /// the price range, which only the trader's own zoom, pan and resets move under a hold.
    pub fn geometry(&self) -> (f32, f64) {
        (self.plot_top, self.vp.height)
    }

    /// The plot's bottom, in window coordinates.
    pub fn bottom(&self) -> f32 {
        self.plot_top + self.vp.height as f32
    }
}

/// A strike that could have a row.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Candidate {
    /// The row's top, in window coordinates.
    pub top: f32,
    /// Its index in the chain's rows.
    pub index: u32,
    /// The selected strike, or one the account holds or works.
    pub priority: bool,
    /// How far from the underlying's price, in px.
    pub distance: f32,
}

/// Choose the rows that fit from `candidates` (reordered, no allocation): priority first, then
/// nearest the money, each kept only if clear of those kept, into `out`, top first. `out` keeps
/// its capacity and stops at it.
pub fn pick_rows(candidates: &mut [Candidate], row_h: f32, out: &mut Vec<Candidate>) {
    out.clear();
    candidates.sort_unstable_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(a.distance.total_cmp(&b.distance))
    });
    for c in candidates.iter() {
        if out.len() == out.capacity() {
            break;
        }
        if out.iter().all(|k| (k.top - c.top).abs() >= row_h) {
            out.push(*c);
        }
    }
    out.sort_unstable_by(|a, b| a.top.total_cmp(&b.top));
}

/// What of the underlying's price mark can show at `y` without covering a row: the line (not on
/// a row: that row is the money, and the chart's own line and axis tag still say where), and its
/// tag (clear of every row). `row_tops` are the rows on screen.
pub fn price_mark(y: f32, row_tops: &[f32], row_h: f32, tag_h: f32) -> (bool, bool) {
    let line = row_tops.iter().all(|t| y < *t || y >= *t + row_h);
    let tag = row_tops
        .iter()
        .all(|t| y + tag_h / 2.0 <= *t || y - tag_h / 2.0 >= *t + row_h);
    (line, tag)
}

/// What the account has on strikes past one edge of the rows.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Off {
    /// Series with a position.
    pub held: u32,
    /// Working orders.
    pub working: u32,
    /// The strike nearest the rows.
    pub nearest: Option<Price>,
}

impl Off {
    pub fn any(&self) -> bool {
        self.held > 0 || self.working > 0
    }
}

/// What the account has that no row shows.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct OffRows {
    pub above: Off,
    pub below: Off,
    /// Nothing to press to reach: crowded out of the rows' range, new while the rows hold, or
    /// on a row the strip, ticket or a long message covers.
    pub between: Off,
}

/// Which of those a strike's mark counts toward, by the same test that places a row, so a strike
/// is either a row's or on this list. `area` is the rows' band.
pub fn tally(
    off: &mut OffRows,
    strike: Price,
    y: f32,
    row_h: f32,
    area: (f32, f32),
    held: bool,
    working: u32,
) {
    let top = y - row_h / 2.0;
    let side = if top < area.0 {
        off.above.nearest = Some(off.above.nearest.map_or(strike, |n| n.min(strike)));
        &mut off.above
    } else if top + row_h > area.1 {
        off.below.nearest = Some(off.below.nearest.map_or(strike, |n| n.max(strike)));
        &mut off.below
    } else {
        &mut off.between
    };
    side.held += u32::from(held);
    side.working += working;
}

/// How much the marker says: as much as fits whole.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Detail {
    Full,
    Totals,
    Bare,
}

/// Whether every row just left its place with nobody aiming at the new one: the rows went from a
/// list to the chart's scale or back, or the plot moved or changed height.
pub fn rows_jumped(was: Option<(f32, f64)>, now: Option<(f32, f64)>) -> bool {
    was != now
}

/// The marker's words.
pub fn write_marker(
    out: &mut impl std::fmt::Write,
    above: Off,
    below: Off,
    between: Off,
    detail: Detail,
) {
    let mut first = true;
    let mut sep = |out: &mut dyn std::fmt::Write| {
        let word = if first { "" } else { "  ·  " };
        first = false;
        out.write_str(word)
    };
    if detail == Detail::Totals {
        let _ = out.write_str("Held or working: ");
    }
    for (off, word) in [(above, "above"), (below, "below"), (between, "not shown")] {
        if !off.any() {
            continue;
        }
        let _ = sep(out);
        if detail != Detail::Full {
            let _ = write!(out, "{} {word}", off.held + off.working);
            continue;
        }
        if off.held > 0 {
            let _ = write!(out, "{} held", off.held);
        }
        if off.held > 0 && off.working > 0 {
            let _ = out.write_str(", ");
        }
        if off.working > 0 {
            let _ = write!(out, "{} working", off.working);
        }
        let _ = write!(out, " {word}");
    }
}

/// Where a marker can be pressed, and the strike a press brings into view.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct MarkerHit {
    pub x: (f32, f32),
    pub y: (f32, f32),
    pub target: Price,
    pub above: bool,
}

impl MarkerHit {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x.0 && x < self.x.1 && y >= self.y.0 && y < self.y.1
    }
}

/// The price range that keeps everything now in view and adds `strike`, just inside the rows'
/// band. `None` when the plot has no room to.
pub fn range_showing(
    scale: &LockedScale,
    strike: Price,
    area: (f32, f32),
    row_h: f32,
    above: bool,
) -> Option<(f64, f64)> {
    let (lo, hi, k) = (scale.vp.price_lo, scale.vp.price_hi, strike.0 as f64);
    let height = scale.vp.height;
    // (A pixel's margin: rounding must not leave it just outside the band.)
    let margin = row_h / 2.0 + 1.0;
    let target = if above {
        area.0 + margin
    } else {
        area.1 - margin
    };
    let to = f64::from(target - scale.plot_top);
    if !(to > 0.0 && to < height) {
        return None;
    }
    let range = if above {
        let r = to / height;
        ((k - lo * r) / (1.0 - r), lo)
    } else {
        (hi, hi - (hi - k) * height / to)
    };
    let (hi, lo) = range;
    (lo.is_finite() && hi.is_finite() && hi > lo).then_some((lo, hi))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: f32 = 18.0;

    /// A strike is a row's, or counted past the edge it's beyond, by the test that places rows.
    #[test]
    fn a_mark_without_a_row_is_counted_on_its_side() {
        let area = (100.0, 500.0);
        let mut off = OffRows::default();
        tally(&mut off, Price(690), 50.0, ROW, area, true, 0);
        tally(&mut off, Price(695), 20.0, ROW, area, false, 2);
        tally(&mut off, Price(670), 600.0, ROW, area, true, 1);
        tally(&mut off, Price(660), 900.0, ROW, area, true, 0);
        tally(&mut off, Price(680), 300.0, ROW, area, true, 0);
        assert_eq!(
            off.above,
            Off {
                held: 1,
                working: 2,
                nearest: Some(Price(690))
            }
        );
        assert_eq!(
            off.below,
            Off {
                held: 2,
                working: 1,
                nearest: Some(Price(670))
            }
        );
        assert_eq!(
            off.between,
            Off {
                held: 1,
                working: 0,
                nearest: None
            }
        );
        // At the very edge of the band: a row fits, so this isn't past it.
        let mut edge = OffRows::default();
        tally(&mut edge, Price(1), 100.0 + ROW / 2.0, ROW, area, true, 0);
        tally(&mut edge, Price(2), 500.0 - ROW / 2.0, ROW, area, true, 0);
        assert_eq!(edge.between.held, 2);
    }

    #[test]
    fn the_marker_says_what_is_past_the_edge() {
        let say = |a: Off, b: Off, m: Off, detail: Detail| {
            let mut s = String::new();
            write_marker(&mut s, a, b, m, detail);
            s
        };
        let (none, held, both) = (
            Off::default(),
            Off {
                held: 2,
                working: 0,
                nearest: None,
            },
            Off {
                held: 1,
                working: 3,
                nearest: None,
            },
        );
        assert_eq!(say(held, none, none, Detail::Full), "2 held above");
        assert_eq!(
            say(none, both, none, Detail::Full),
            "1 held, 3 working below"
        );
        assert_eq!(
            say(
                none,
                Off {
                    held: 0,
                    working: 1,
                    nearest: None
                },
                held,
                Detail::Full
            ),
            "1 working below  ·  2 held not shown"
        );
        assert_eq!(
            say(held, both, none, Detail::Totals),
            "Held or working: 2 above  ·  4 below"
        );
        assert_eq!(
            say(held, both, held, Detail::Bare),
            "2 above  ·  4 below  ·  2 not shown"
        );
    }

    /// A press widens the range just enough: the strike gets a row's room in the band, and what
    /// was in view stays there.
    #[test]
    fn a_press_brings_the_strike_inside_the_rows() {
        let s = scale(68_000.0, 69_000.0, 600.0);
        let area = (80.0, 600.0);
        for (strike, above) in [
            (70_000, true),
            (65_000, false),
            (69_010, true),
            (67_990, false),
        ] {
            let (lo, hi) = range_showing(&s, Price(strike), area, ROW, above).unwrap();
            assert!(lo <= 68_000.0 && hi >= 69_000.0, "{lo} {hi}");
            let mut after = s;
            after.vp.price_lo = lo;
            after.vp.price_hi = hi;
            let y = after.y(Price(strike));
            assert!(
                y - ROW / 2.0 >= area.0 && y + ROW / 2.0 <= area.1,
                "{strike} {y}"
            );
            let mut off = OffRows::default();
            tally(&mut off, Price(strike), y, ROW, area, true, 0);
            assert_eq!(off.between.held, 1, "{strike}");
        }
        assert_eq!(
            range_showing(&s, Price(70_000), (0.0, 600.0), ROW, false),
            None
        );
    }

    #[test]
    fn rows_jump_when_the_scale_comes_or_goes_or_the_plot_moves() {
        let at = |lo, hi, height, plot_top| {
            let mut s = scale(lo, hi, height);
            s.plot_top = plot_top;
            Some(s.geometry())
        };
        let here = at(68_000.0, 69_000.0, 600.0, 40.0);
        assert!(rows_jumped(None, here), "the first fit");
        assert!(rows_jumped(here, None), "no scale any more");
        assert!(
            rows_jumped(here, at(68_000.0, 69_000.0, 500.0, 40.0)),
            "height"
        );
        assert!(
            rows_jumped(here, at(68_000.0, 69_000.0, 600.0, 60.0)),
            "moved"
        );
        assert!(!rows_jumped(None, None));
        // The trader's own zoom: the rows move, and it was aimed.
        assert!(!rows_jumped(here, at(68_100.0, 68_900.0, 600.0, 40.0)));
    }

    #[test]
    fn a_marker_takes_a_press_only_on_itself() {
        let hit = MarkerHit {
            x: (100.0, 200.0),
            y: (50.0, 70.0),
            target: Price(1),
            above: true,
        };
        assert!(hit.contains(150.0, 60.0));
        assert!(!hit.contains(99.0, 60.0) && !hit.contains(200.0, 60.0));
        assert!(!hit.contains(150.0, 49.0) && !hit.contains(150.0, 70.0));
    }

    /// The tag never covers a strike or a quote: near a row it goes, on a row the line does too.
    #[test]
    fn the_price_mark_stays_off_the_rows() {
        let rows = [100.0, 150.0];
        assert_eq!(price_mark(130.0, &rows, ROW, 15.0), (true, true), "between");
        assert_eq!(price_mark(120.0, &rows, ROW, 15.0), (true, false), "near");
        assert_eq!(price_mark(109.0, &rows, ROW, 15.0), (false, false), "on");
        assert_eq!(price_mark(145.0, &rows, ROW, 15.0), (true, false), "near");
        assert_eq!(price_mark(130.0, &[], ROW, 15.0), (true, true), "no rows");
    }

    fn scale(lo: f64, hi: f64, height: f64) -> LockedScale {
        let mut vp = Viewport::default();
        vp.set_size(800.0, height);
        vp.price_lo = lo;
        vp.price_hi = hi;
        LockedScale { vp, plot_top: 40.0 }
    }

    fn candidates(s: &LockedScale, strikes: &[i64], last: i64, priority: &[i64]) -> Vec<Candidate> {
        strikes
            .iter()
            .enumerate()
            .map(|(i, &k)| Candidate {
                top: s.y(Price(k)) - ROW / 2.0,
                index: i as u32,
                priority: priority.contains(&k),
                distance: (s.y(Price(k)) - s.y(Price(last))).abs(),
            })
            .collect()
    }

    /// A strike's row is centered on the chart's own y for that price, at any zoom.
    #[test]
    fn a_row_sits_at_its_strikes_price_on_the_chart() {
        for (lo, hi) in [(68_000.0, 69_000.0), (68_400.0, 68_500.0)] {
            let s = scale(lo, hi, 600.0);
            let strike = Price(68_450);
            let on_chart = 40.0 + s.vp.price_to_y(68_450.0) as f32;
            assert_eq!(s.y(strike), on_chart);
            let mut c = candidates(&s, &[68_450], 68_450, &[]);
            let mut out = Vec::with_capacity(8);
            pick_rows(&mut c, ROW, &mut out);
            assert_eq!(out[0].top + ROW / 2.0, on_chart);
        }
    }

    #[test]
    fn higher_strikes_are_higher_and_rows_never_overlap() {
        let s = scale(68_000.0, 69_000.0, 600.0);
        let strikes: Vec<i64> = (0..=20).map(|i| 68_000 + i * 50).collect();
        let mut c = candidates(&s, &strikes, 68_500, &[]);
        let mut out = Vec::with_capacity(64);
        pick_rows(&mut c, ROW, &mut out);
        assert!(out.len() > 1);
        for pair in out.windows(2) {
            assert!(pair[1].top - pair[0].top >= ROW, "{pair:?}");
            assert!(pair[0].index > pair[1].index, "top down is high to low");
        }
    }

    /// Too many strikes for the height: every kept row is still at its own price, the money
    /// stays, and a strike the account holds keeps its row over a closer one.
    #[test]
    fn strikes_give_way_but_the_money_and_held_strikes_stay() {
        let s = scale(68_000.0, 69_000.0, 200.0);
        let strikes: Vec<i64> = (0..=100).map(|i| 68_000 + i * 10).collect();
        let mut c = candidates(&s, &strikes, 68_500, &[68_510]);
        let mut out = Vec::with_capacity(64);
        pick_rows(&mut c, ROW, &mut out);
        assert!(out.len() < strikes.len() && out.len() >= 8, "{}", out.len());
        let has = |k: i64| out.iter().any(|r| strikes[r.index as usize] == k);
        assert!(has(68_510), "held strike");
        assert!(!has(68_500), "too close to the held one");
        for r in &out {
            let k = strikes[r.index as usize];
            assert_eq!(
                r.top + ROW / 2.0,
                s.y(Price(k)),
                "kept rows stay on their price"
            );
        }
    }

    #[test]
    fn it_stops_at_capacity_without_growing() {
        let s = scale(68_000.0, 69_000.0, 10_000.0);
        let strikes: Vec<i64> = (0..=100).map(|i| 68_000 + i * 10).collect();
        let mut c = candidates(&s, &strikes, 68_500, &[]);
        let mut out = Vec::with_capacity(4);
        pick_rows(&mut c, ROW, &mut out);
        assert_eq!((out.len(), out.capacity()), (4, 4));
    }

    #[test]
    fn a_new_scale_is_a_new_key() {
        let a = scale(68_000.0, 69_000.0, 600.0);
        let b = scale(68_000.0, 68_900.0, 600.0);
        assert_ne!(a.key(), b.key());
        assert_eq!(a.key(), scale(68_000.0, 69_000.0, 600.0).key());
    }
}
