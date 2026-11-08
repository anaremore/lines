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

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: f32 = 18.0;

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
