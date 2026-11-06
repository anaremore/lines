//! Chart geometry for a VWAP: one column per visible bar that has a value. Pure and
//! allocation-free (the chart layer calls it in paint; `tests/no_alloc.rs`).
//!
//! A column spans its bar's slot horizontally and holds the VWAP's y and the ±kσ band edges.
//! The chart draws a level segment across each column, a connector to the previous column in
//! the same reset period, and band fills between the edges: all axis-aligned quads, so the
//! line is crisp at every DPI and nothing is tessellated per frame. Values are drawn per bar,
//! the resolution they are computed at.

use lines_core::{Bar, Viewport};

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
/// `points` are sorted by time; `bars` are the chart's bars.
pub fn build_columns(
    points: &[Point],
    bars: &[Bar],
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
    let first_t = bars[range.start].time;
    let mut j = points.partition_point(|p| p.time < first_t);
    let mut prev: Option<(usize, i64)> = None;
    for i in range {
        if out.len() == out.capacity() {
            break;
        }
        let t = bars[i].time;
        while j < points.len() && points[j].time < t {
            j += 1;
        }
        let Some(p) = points.get(j).filter(|p| p.time == t) else {
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
            approx: p.approx,
            joins,
        });
        prev = Some((i, p.period));
    }
}
