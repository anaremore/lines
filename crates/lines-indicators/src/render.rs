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

use lines_core::{BarsView, Viewport};

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
        let t = bars.bars[i].time;
        let end = bars.end(i);
        while j < points.len() && points[j].time < t {
            j += 1;
        }
        // The bucket's points: the last one is drawn, any approximate one flags the bar.
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
}
