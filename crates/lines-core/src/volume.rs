//! Volume per bar (the chart's Volume study): each visible bar's traded quantity, from the
//! chart's own bars (the same counted trades as the candles, on every timeframe, tick bars
//! included), optionally split by aggressor and with a simple moving average.
//!
//! - **Total** is the bar's own `volume`, never a separate count, so a volume bar always
//!   agrees with its candle.
//! - **Split** sums the bar's per-price volume by side ([`crate::footprint::bar_levels`]: a tick
//!   bar's own levels, else the store's per-minute footprints over its bucket). Volume without a
//!   known side (and history before the counted trades) counts only toward the total
//!   ([`VolumeBar::no_side`]); it is never assigned to a side.
//! - **Average**: the mean of the last `n` totals, defined once `n` bars exist (exact `i128`
//!   sums; the value is `f64`, display only).
//!
//! Building is allocation-free within the buffer reserved up front (`tests/no_alloc.rs`).

use std::ops::Range;

use crate::footprint::bar_levels;
use crate::profile::Footprint;
use crate::{BarsView, Qty};

/// One bar's volume.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct VolumeBar {
    /// Its index in the chart's bars.
    pub bar: usize,
    pub total: Qty,
    /// Buyer- and seller-initiated volume (zero unless split).
    pub buy: Qty,
    pub sell: Qty,
    /// The candle rose or stayed level (`close >= open`), as the candles are colored.
    pub up: bool,
    /// The average of the last `n` totals ending at this bar, once `n` bars exist.
    pub avg: Option<f64>,
}

impl VolumeBar {
    /// Volume without a known aggressor side: the total less both sides (never negative).
    pub fn no_side(&self) -> Qty {
        Qty((self.total.0 - self.buy.0 - self.sell.0).max(0))
    }
}

/// What to compute beyond the totals.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct VolumeOptions {
    pub split: bool,
    /// The average's length in bars (`None`: no average).
    pub average: Option<usize>,
}

/// A frame's volume bars: the visible ones and the newest (for the axis tag and the readout
/// when the pointer is elsewhere).
#[derive(Clone, Debug, Default)]
pub struct VolumeFrame {
    pub bars: Vec<VolumeBar>,
    /// The tallest visible total or average (the pane's scale), at least 0.
    pub max: i64,
    pub newest: Option<VolumeBar>,
}

impl VolumeFrame {
    pub fn with_capacity(n: usize) -> VolumeFrame {
        VolumeFrame {
            bars: Vec::with_capacity(n),
            max: 0,
            newest: None,
        }
    }

    /// Room for `n` visible bars (only grows; called outside paint when the plot widens).
    pub fn reserve(&mut self, n: usize) {
        if self.bars.capacity() < n {
            self.bars.reserve(n - self.bars.len());
        }
    }

    pub fn clear(&mut self) {
        self.bars.clear();
        self.max = 0;
        self.newest = None;
    }

    /// The visible bar `bar`, if built.
    pub fn at(&self, bar: usize) -> Option<&VolumeBar> {
        let first = self.bars.first()?.bar;
        self.bars.get(bar.checked_sub(first)?)
    }

    /// Build the bars of `range` of `view` (at most the reserved capacity, newest kept).
    /// Never allocates.
    pub fn build(
        &mut self,
        fp: &Footprint,
        view: BarsView<'_>,
        range: Range<usize>,
        opts: VolumeOptions,
    ) {
        self.clear();
        let len = view.len();
        let end = range.end.min(len);
        let start = range
            .start
            .min(end)
            .max(end.saturating_sub(self.bars.capacity()));
        let n = opts.average.map(|n| n.max(1));
        // The average's window sum up to (not including) `start`, from `from` on.
        let from = n.map_or(start, |n| start.saturating_sub(n - 1));
        let mut sum: i128 = match n {
            Some(_) => view.bars[from..start]
                .iter()
                .map(|b| i128::from(b.volume.0))
                .sum(),
            None => 0,
        };
        for i in start..end {
            let mut v = one(fp, view, i, opts.split);
            if let Some(n) = n {
                sum += i128::from(view.bars[i].volume.0);
                if i >= n && i - n >= from {
                    sum -= i128::from(view.bars[i - n].volume.0);
                }
                v.avg = (i + 1 >= n).then(|| sum as f64 / n as f64);
            }
            self.max = self.max.max(v.total.0);
            if let Some(a) = v.avg {
                self.max = self.max.max(a.ceil() as i64);
            }
            self.bars.push(v);
        }
        self.newest = match (len.checked_sub(1), self.bars.last()) {
            (Some(last), Some(b)) if b.bar == last => Some(*b),
            (Some(last), _) => {
                let mut v = one(fp, view, last, opts.split);
                if let Some(n) = n.filter(|&n| len >= n) {
                    let s: i128 = view.bars[len - n..]
                        .iter()
                        .map(|b| i128::from(b.volume.0))
                        .sum();
                    v.avg = Some(s as f64 / n as f64);
                }
                Some(v)
            }
            (None, _) => None,
        };
    }
}

/// Bar `i`'s total, direction and (when `split`) sides.
fn one(fp: &Footprint, view: BarsView<'_>, i: usize, split: bool) -> VolumeBar {
    let b = &view.bars[i];
    let mut v = VolumeBar {
        bar: i,
        total: b.volume,
        up: b.close >= b.open,
        ..VolumeBar::default()
    };
    if split {
        for (_, s) in bar_levels(fp, view, i) {
            v.buy.0 = v.buy.0.saturating_add(s.buy.0);
            v.sell.0 = v.sell.0.saturating_add(s.sell.0);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Aggressor, Bar, BarSeries, Derived, Price, Timeframe, UtcMillis};

    /// Two hours of 1-minute bars, a trade a second, every fifth without a side.
    fn data() -> (BarSeries, Footprint) {
        let mut series = BarSeries::new(60_000, 1_000);
        let mut fp = Footprint::new(60_000, 1_000);
        for k in 0..7_200i64 {
            let t = UtcMillis(k * 1_000);
            let p = Price(10_000 + (k * 7 % 40) * 25);
            let q = Qty(k % 4 + 1);
            let side = match k % 5 {
                0 => None,
                1 | 2 => Some(Aggressor::Buy),
                _ => Some(Aggressor::Sell),
            };
            series.apply_trade(t, p, q);
            fp.add(t, p, q, side);
        }
        (series, fp)
    }

    #[test]
    fn totals_are_the_bars_own_volume_and_sides_sum_from_their_trades() {
        let (series, fp) = data();
        let view = BarsView::fixed(series.bars(), 60_000);
        let mut f = VolumeFrame::with_capacity(200);
        f.build(
            &fp,
            view,
            0..view.len(),
            VolumeOptions {
                split: true,
                average: None,
            },
        );
        assert_eq!(f.bars.len(), 120);
        for v in &f.bars {
            let b = view.bars[v.bar];
            assert_eq!(v.total, b.volume);
            assert_eq!(v.up, b.close >= b.open);
            // Every trade is counted once: both sides and no side add up to the total.
            assert_eq!(v.buy.0 + v.sell.0 + v.no_side().0, v.total.0);
            assert!(v.buy.0 > 0 && v.sell.0 > 0 && v.no_side().0 > 0);
        }
        assert_eq!(f.max, f.bars.iter().map(|v| v.total.0).max().unwrap());
        assert_eq!(f.newest, f.bars.last().copied());
        // Without the split no side is counted: all of it is "no side".
        f.build(&fp, view, 0..view.len(), VolumeOptions::default());
        assert!(f.bars.iter().all(|v| v.buy.0 == 0 && v.sell.0 == 0));
    }

    /// Every timeframe sums the same trades: a 15-minute bar's sides are its minutes' sides.
    #[test]
    fn a_higher_timeframe_sums_the_same_trades() {
        let (series, fp) = data();
        let one = BarsView::fixed(series.bars(), 60_000);
        let mut d = Derived::new(Timeframe::Minutes(15), series.capacity());
        d.update(&series, None);
        let view = d.view();
        let opts = VolumeOptions {
            split: true,
            average: None,
        };
        let (mut a, mut b) = (
            VolumeFrame::with_capacity(200),
            VolumeFrame::with_capacity(200),
        );
        a.build(&fp, one, 0..one.len(), opts);
        b.build(&fp, view, 0..view.len(), opts);
        assert_eq!(b.bars.len(), 8);
        for v in &b.bars {
            let mins = a.bars.iter().filter(|m| {
                let t = one.bars[m.bar].time;
                t >= view.bars[v.bar].time && t < view.end(v.bar)
            });
            let (mut tot, mut buy, mut sell) = (0, 0, 0);
            for m in mins {
                tot += m.total.0;
                buy += m.buy.0;
                sell += m.sell.0;
            }
            assert_eq!((v.total.0, v.buy.0, v.sell.0), (tot, buy, sell));
        }
    }

    /// History bars (no counted trades) keep their total, with no side.
    #[test]
    fn bars_without_trades_have_no_side() {
        let bars = [Bar {
            time: UtcMillis(0),
            open: Price(1),
            high: Price(2),
            low: Price(0),
            close: Price(0),
            volume: Qty(500),
        }];
        let fp = Footprint::new(60_000, 10);
        let mut f = VolumeFrame::with_capacity(4);
        f.build(
            &fp,
            BarsView::fixed(&bars, 60_000),
            0..1,
            VolumeOptions {
                split: true,
                average: None,
            },
        );
        assert_eq!(f.bars[0].no_side(), Qty(500));
        assert!(!f.bars[0].up);
    }

    #[test]
    fn the_average_is_the_mean_of_the_last_n_totals() {
        let (series, fp) = data();
        let view = BarsView::fixed(series.bars(), 60_000);
        let mut f = VolumeFrame::with_capacity(200);
        let opts = VolumeOptions {
            split: false,
            average: Some(20),
        };
        // Visible from bar 50: the window reaches back before the range.
        f.build(&fp, view, 50..100, opts);
        for v in &f.bars {
            let want: i64 = view.bars[v.bar + 1 - 20..=v.bar]
                .iter()
                .map(|b| b.volume.0)
                .sum();
            assert!((v.avg.unwrap() - want as f64 / 20.0).abs() < 1e-9);
        }
        // Defined from the 20th bar on.
        f.build(&fp, view, 0..30, opts);
        assert!(f.bars[18].avg.is_none() && f.bars[19].avg.is_some());
        // The newest, off screen, has its own average.
        let last = view.len() - 1;
        let want: i64 = view.bars[last + 1 - 20..].iter().map(|b| b.volume.0).sum();
        let newest = f.newest.unwrap();
        assert_eq!(newest.bar, last);
        assert!((newest.avg.unwrap() - want as f64 / 20.0).abs() < 1e-9);
        assert_eq!(f.at(5).map(|v| v.bar), Some(5));
        assert_eq!(f.at(40), None);
    }

    /// Never past the reserved buffer (the newest bars kept).
    #[test]
    fn the_buffer_never_grows() {
        let (series, fp) = data();
        let view = BarsView::fixed(series.bars(), 60_000);
        let mut f = VolumeFrame::with_capacity(10);
        let cap = f.bars.capacity();
        f.build(&fp, view, 0..view.len(), VolumeOptions::default());
        assert_eq!(f.bars.len(), cap);
        assert_eq!(f.bars.capacity(), cap);
        assert_eq!(f.bars.last().unwrap().bar, view.len() - 1);
    }
}
