//! Chart geometry for a VWAP: one column per visible bar that has a value. Pure and
//! allocation-free (the chart layer calls it in paint; `tests/no_alloc.rs`).
//!
//! A column spans its bar's slot horizontally and holds the VWAP's y and the ±kσ band edges.
//! The chart draws a level segment across each column, a connector to the previous column in
//! the same reset period, and band fills between the edges: all axis-aligned quads, so the
//! line is crisp at every DPI and nothing is tessellated per frame. Values are drawn per bar,
//! the resolution they are computed at.
//!
//! On a higher timeframe (roadmap 9) the engine still computes per 1-minute bar: each chart
//! bar takes the line at its bucket's last 1-minute point (bands too), approximate if any
//! point in the bucket is.
//!
//! On tick bars (roadmap 9b, [`BarsView::trades`]) each bar takes the line at its last trade:
//! the 1-minute point holding that trade (the engine's resolution is the minute), approximate
//! if that point is.

use lines_core::{BarsView, UtcMillis, Viewport};

use crate::delta::DeltaPoint;
use crate::vwap::{Bands, Point};

#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Column {
    /// Slot edges, plot-local px.
    pub left: f32,
    pub right: f32,
    /// The VWAP, plot-local px.
    pub y: f32,
    /// ±kσ edges for k = 1, 2, 3 (`up[k-1]` above, `down[k-1]` below); equal to `y` when off.
    pub up: [f32; 3],
    pub down: [f32; 3],
    pub approx: bool,
    /// Continues the previous column's line (same reset period, adjacent bar).
    pub joins: bool,
}

/// Fill `out` (at most its capacity: never allocates) with the columns for the visible bars.
/// `points` are sorted by time; `bars` are the chart's bars (any timeframe).
pub fn build_columns(
    points: &[Point],
    bars: BarsView<'_>,
    vp: &Viewport,
    bands: Bands,
    out: &mut Vec<Column>,
) {
    out.clear();
    let range = vp.visible_range(bars.len());
    if range.is_empty() || points.is_empty() {
        return;
    }
    let half = (vp.bar_px * 0.5) as f32;
    let first_t = bars.bars[range.start].time;
    let mut j = points.partition_point(|p| p.time < first_t);
    let mut prev: Option<(usize, i64)> = None;
    for i in range {
        if out.len() == out.capacity() {
            break;
        }
        let (last, approx) = match bars.trades {
            Some(tb) => {
                let Some(p) = tb
                    .last_trade(i)
                    .and_then(|t| point_at(points, t, |p| p.time))
                else {
                    continue;
                };
                (p, p.approx)
            }
            None => {
                let t = bars.bars[i].time;
                let end = bars.end(i);
                while j < points.len() && points[j].time < t {
                    j += 1;
                }
                // The bucket's points: the last one is drawn, any approximate one flags it.
                let mut last = None;
                let mut approx = false;
                while j < points.len() && points[j].time < end {
                    approx |= points[j].approx;
                    last = Some(&points[j]);
                    j += 1;
                }
                let Some(p) = last else {
                    continue;
                };
                (p, approx)
            }
        };
        let p = last;
        let x = vp.index_to_x(i as f64) as f32;
        let y = vp.price_to_y(p.vwap) as f32;
        let mut up = [y; 3];
        let mut down = [y; 3];
        for k in 1..=3u8 {
            if bands.has(k) {
                let d = p.sd * k as f64;
                up[k as usize - 1] = vp.price_to_y(p.vwap + d) as f32;
                down[k as usize - 1] = vp.price_to_y(p.vwap - d) as f32;
            }
        }
        let joins = prev.is_some_and(|(pi, period)| pi + 1 == i && period == p.period);
        out.push(Column {
            left: x - half,
            right: x + half,
            y,
            up,
            down,
            approx,
            joins,
        });
        prev = Some((i, p.period));
    }
}

/// Columns for a study computed on the chart's own bars (averages, Bollinger Bands: one point
/// per chart bar, at the bar's start time). A bar takes the point at its start time. Trade-built
/// bars can share a start millisecond: such a group pairs with the points of that time
/// aligned at its end (a missing value is an early bar's, before the window fills), except the
/// newest group, aligned at its start (a missing value is the newest bar's, the engine not
/// there yet). A bar without a point has no column. Fills `out` up to its capacity: never
/// allocates.
pub fn build_bar_columns(
    points: &[Point],
    bars: BarsView<'_>,
    vp: &Viewport,
    bands: Bands,
    out: &mut Vec<Column>,
) {
    out.clear();
    let range = vp.visible_range(bars.len());
    if range.is_empty() || points.is_empty() {
        return;
    }
    let half = (vp.bar_px * 0.5) as f32;
    let all = bars.bars;
    let mut prev: Option<usize> = None;
    for i in range {
        if out.len() == out.capacity() {
            break;
        }
        let t = all[i].time;
        let p0 = points.partition_point(|p| p.time < t);
        let k = points[p0..].partition_point(|p| p.time <= t);
        if k == 0 {
            continue;
        }
        let alone = (i == 0 || all[i - 1].time != t) && all.get(i + 1).is_none_or(|b| b.time != t);
        let p = if k == 1 && alone {
            &points[p0]
        } else {
            let mut g0 = i;
            while g0 > 0 && all[g0 - 1].time == t {
                g0 -= 1;
            }
            let mut g1 = i + 1;
            while g1 < all.len() && all[g1].time == t {
                g1 += 1;
            }
            let (m, at) = (g1 - g0, i - g0);
            let at = if k >= m || g1 == all.len() {
                at
            } else {
                match at.checked_sub(m - k) {
                    Some(a) => a,
                    None => continue,
                }
            };
            match points.get(p0 + at).filter(|_| at < k) {
                Some(p) => p,
                None => continue,
            }
        };
        let x = vp.index_to_x(i as f64) as f32;
        let y = vp.price_to_y(p.vwap) as f32;
        let mut up = [y; 3];
        let mut down = [y; 3];
        for kk in 1..=3u8 {
            if bands.has(kk) {
                let d = p.sd * kk as f64;
                up[kk as usize - 1] = vp.price_to_y(p.vwap + d) as f32;
                down[kk as usize - 1] = vp.price_to_y(p.vwap - d) as f32;
            }
        }
        out.push(Column {
            left: x - half,
            right: x + half,
            y,
            up,
            down,
            approx: p.approx,
            joins: prev.is_some_and(|pi| pi + 1 == i),
        });
        prev = Some(i);
    }
}

/// The 1-minute point holding `t` (points sorted by their minute's start), if there is one.
fn point_at<P>(points: &[P], t: UtcMillis, time: impl Fn(&P) -> UtcMillis) -> Option<&P> {
    let k = points.partition_point(|p| time(p) <= t).checked_sub(1)?;
    let p = &points[k];
    (t.0 - time(p).0 < UtcMillis::MINUTE).then_some(p)
}

/// One chart bar of cumulative delta (quantity units): the merge of its bucket's 1-minute
/// candles.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct DeltaCandle {
    /// Index of the chart bar.
    pub bar: usize,
    pub open: i64,
    pub high: i64,
    pub low: i64,
    pub close: i64,
    pub approx: bool,
}

/// Fill `out` (at most its capacity: never allocates) with the delta candles of the chart bars
/// `range`. A bar's candle opens at its bucket's first 1-minute candle, closes at its last,
/// and spans their extremes; if a reset falls inside the bucket (a weekly bar over several
/// sessions), the candle covers the last reset period in it. Returns the visible extremes
/// (`None` without candles), for the sub-pane's scale.
pub fn build_delta_candles(
    points: &[DeltaPoint],
    bars: BarsView<'_>,
    range: std::ops::Range<usize>,
    out: &mut Vec<DeltaCandle>,
) -> Option<(i64, i64)> {
    out.clear();
    if range.is_empty() || points.is_empty() || range.start >= bars.len() {
        return None;
    }
    let first_t = bars.bars[range.start].time;
    let mut j = points.partition_point(|p| p.time < first_t);
    let mut extent: Option<(i64, i64)> = None;
    // Tick bars: the delta at each bar's last trade (the minute point holding it), drawn from
    // the previous bar's close in the same reset period.
    if let Some(tb) = bars.trades {
        let mut prev: Option<(usize, i64, i64)> = None; // (bar, period, close)
        for i in range.start..range.end.min(bars.len()) {
            if out.len() == out.capacity() {
                break;
            }
            let Some(p) = tb
                .last_trade(i)
                .and_then(|t| point_at(points, t, |p| p.time))
            else {
                continue;
            };
            let open = match prev {
                Some((b, period, close)) if b + 1 == i && period == p.period => close,
                _ => p.open,
            };
            let k = DeltaCandle {
                bar: i,
                open,
                high: open.max(p.close),
                low: open.min(p.close),
                close: p.close,
                approx: p.approx,
            };
            extent = Some(extent.map_or((k.low, k.high), |(a, b)| (a.min(k.low), b.max(k.high))));
            out.push(k);
            prev = Some((i, p.period, p.close));
        }
        return extent;
    }
    for i in range.start..range.end.min(bars.len()) {
        if out.len() == out.capacity() {
            break;
        }
        let (t, end) = (bars.bars[i].time, bars.end(i));
        while j < points.len() && points[j].time < t {
            j += 1;
        }
        let mut c: Option<(DeltaCandle, i64)> = None;
        while j < points.len() && points[j].time < end {
            let p = &points[j];
            c = Some(match c {
                Some((mut k, period)) if period == p.period => {
                    k.high = k.high.max(p.high);
                    k.low = k.low.min(p.low);
                    k.close = p.close;
                    k.approx |= p.approx;
                    (k, period)
                }
                _ => (
                    DeltaCandle {
                        bar: i,
                        open: p.open,
                        high: p.high,
                        low: p.low,
                        close: p.close,
                        approx: p.approx,
                    },
                    p.period,
                ),
            });
            j += 1;
        }
        if let Some((k, _)) = c {
            extent = Some(extent.map_or((k.low, k.high), |(a, b)| (a.min(k.low), b.max(k.high))));
            out.push(k);
        }
    }
    extent
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{Bar, BarSeries, Derived, Price, Qty, Timeframe, UtcMillis};

    const M: i64 = 60_000;

    fn point(i: i64, approx: bool, period: i64) -> Point {
        Point {
            time: UtcMillis(i * M),
            vwap: 1_000.0 + i as f64,
            sd: 2.0,
            approx,
            period,
            forming: false,
        }
    }

    /// On a 5-minute chart each column is the line at its bucket's last 1-minute point, bands
    /// too; approximate if any point in the bucket is.
    #[test]
    fn higher_timeframes_sample_the_last_point_of_each_bucket() {
        let mut series = BarSeries::new(M, 100);
        for i in 0..20 {
            series.upsert(Bar {
                time: UtcMillis(i * M),
                open: Price(1_000),
                high: Price(1_050),
                low: Price(950),
                close: Price(1_000),
                volume: Qty(1),
            });
        }
        let mut d = Derived::new(Timeframe::Minutes(5), series.capacity());
        d.update(&series, None);
        // Minute 2 is approximate; minute 13 has no value; 15..19 are a new period.
        let points: Vec<Point> = (0..20)
            .filter(|&i| i != 13)
            .map(|i| point(i, i == 2, i / 15))
            .collect();
        let mut vp = Viewport::default();
        vp.set_size(400.0, 400.0);
        vp.price_lo = 900.0;
        vp.price_hi = 1_100.0;
        vp.bar_px = 20.0;
        vp.right_index = 3.0;
        let mut out = Vec::with_capacity(16);
        build_columns(&points, d.view(), &vp, Bands(0b001), &mut out);
        assert_eq!(out.len(), 4);
        for (k, (c, last)) in out.iter().zip([4, 9, 14, 19]).enumerate() {
            let want = vp.price_to_y(1_000.0 + last as f64) as f32;
            assert_eq!(c.y, want, "bucket {k}");
            assert_eq!(c.up[0], vp.price_to_y(1_002.0 + last as f64) as f32);
            assert_eq!(c.down[0], vp.price_to_y(998.0 + last as f64) as f32);
            assert_eq!(c.approx, k == 0, "bucket {k}");
        }
        assert_eq!(
            out.iter().map(|c| c.joins).collect::<Vec<_>>(),
            [false, true, true, false],
            "a new period breaks the line"
        );
        // A 1-minute chart still draws every point.
        build_columns(
            &points,
            BarsView::fixed(series.bars(), M),
            &vp,
            Bands::NONE,
            &mut out,
        );
        assert_eq!(
            out.iter().map(|c| c.approx).collect::<Vec<_>>(),
            [false, false, true, false],
            "per-minute flags on the 1-minute chart"
        );
    }

    fn dp(i: i64, open: i64, high: i64, low: i64, close: i64, period: i64) -> DeltaPoint {
        DeltaPoint {
            time: UtcMillis(i * M),
            open,
            high,
            low,
            close,
            approx: i == 6,
            period,
            forming: false,
        }
    }

    /// A 5-minute delta candle is the merge of its five 1-minute candles: first open, last
    /// close, extremes; a reset inside a bucket starts the candle over.
    #[test]
    fn delta_candles_merge_their_bucket() {
        let mut series = BarSeries::new(M, 100);
        for i in 0..10 {
            series.upsert(Bar {
                time: UtcMillis(i * M),
                open: Price(1_000),
                high: Price(1_000),
                low: Price(1_000),
                close: Price(1_000),
                volume: Qty(1),
            });
        }
        let mut d = Derived::new(Timeframe::Minutes(5), series.capacity());
        d.update(&series, None);
        let points = vec![
            dp(0, 0, 4, -1, 3, 0),
            dp(1, 3, 3, -5, -5, 0),
            dp(2, -5, 9, -5, 8, 0),
            dp(3, 8, 8, 2, 2, 0),
            dp(4, 2, 6, 1, 6, 0),
            // Minute 6 is approximate; a reset at minute 8.
            dp(5, 6, 7, 5, 7, 0),
            dp(6, 7, 7, 0, 0, 0),
            dp(8, 0, 2, -3, -3, 1),
            dp(9, -3, 1, -4, 1, 1),
        ];
        let mut out = Vec::with_capacity(8);
        let ext = build_delta_candles(&points, d.view(), 0..2, &mut out);
        assert_eq!(
            out,
            vec![
                DeltaCandle {
                    bar: 0,
                    open: 0,
                    high: 9,
                    low: -5,
                    close: 6,
                    approx: false
                },
                DeltaCandle {
                    bar: 1,
                    open: 0,
                    high: 2,
                    low: -4,
                    close: 1,
                    approx: false
                },
            ],
            "the second bucket shows its last period only"
        );
        assert_eq!(ext, Some((-5, 9)));
        // Never more than the buffer holds.
        let mut small = Vec::with_capacity(1);
        build_delta_candles(&points, d.view(), 0..2, &mut small);
        assert_eq!((small.len(), small.capacity()), (1, 1));
    }
}

#[cfg(test)]
mod tick_tests {
    use lines_core::{Price, Qty, TickBars, TickReset, UtcMillis, Viewport};

    use super::*;

    fn point(minute: i64, vwap: f64, approx: bool) -> Point {
        Point {
            time: UtcMillis(minute * 60_000),
            vwap,
            sd: 0.0,
            approx,
            period: 0,
            forming: false,
        }
    }

    /// On tick bars each bar takes the VWAP of the minute holding its last trade.
    #[test]
    fn tick_bars_sample_vwap_at_their_last_trade() {
        // 3-trade bars: trades at 10 s, 40 s, 70 s | 100 s, 130 s, 190 s | 200 s.
        let mut tb = TickBars::new(3, TickReset::Continuous, None, 100);
        for s in [10, 40, 70, 100, 130, 190, 200] {
            tb.push(UtcMillis(s * 1000), Price(100), Qty(1), None);
        }
        let points = [
            point(0, 100.0, false),
            point(1, 101.0, false),
            point(2, 102.0, false),
            point(3, 103.0, true),
        ];
        let mut vp = Viewport::default();
        vp.set_size(300.0, 200.0);
        vp.price_lo = 90.0;
        vp.price_hi = 110.0;
        vp.right_index = 2.0;
        let mut out = Vec::with_capacity(8);
        build_columns(&points, tb.view(), &vp, Bands::default(), &mut out);
        let ys: Vec<f32> = out.iter().map(|c| c.y).collect();
        // Last trades at 70 s (minute 1), 190 s (minute 3), 200 s (minute 3).
        let y = |v: f64| vp.price_to_y(v) as f32;
        assert_eq!(ys, [y(101.0), y(103.0), y(103.0)]);
        assert_eq!(
            out.iter().map(|c| c.approx).collect::<Vec<_>>(),
            [false, true, true]
        );
    }
}
