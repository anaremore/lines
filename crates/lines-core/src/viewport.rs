//! Chart coordinate math: bar index / price  <->  pixels, plus pan, zoom and drag-scaling.
//!
//! The x axis is in *bar-index* space, not wall time, so session gaps (nights, weekends,
//! the daily CME halt) take no horizontal room. Bar `i`'s centre is at index `i as f64`.
//! The y axis is in the contract's scaled-integer price units (as `f64`, display only).
//! Pixel coordinates are logical pixels relative to the plot area's top-left corner.

use crate::bars::Bar;

pub const MIN_BAR_PX: f64 = 1.0;
pub const MAX_BAR_PX: f64 = 80.0;
/// Bar width of a new chart, and after a double-click on the time axis.
pub const DEFAULT_BAR_PX: f64 = 8.0;
/// Default empty space (in bars) right of the newest bar when following live.
pub const RIGHT_MARGIN_BARS: f64 = 3.0;
const PRICE_PAD_FRACTION: f64 = 0.08;
/// Manual price scaling never shows fewer than this many ticks top to bottom...
pub const MIN_PRICE_SPAN_TICKS: f64 = 4.0;
/// ...or more than this many.
pub const MAX_PRICE_SPAN_TICKS: f64 = 1.0e7;
/// Drag scaling is exponential: the scale changes by `e^(distance_px * SCALE_PER_PX)`, so
/// equal drag distances give equal zoom ratios at any zoom level.
pub const SCALE_PER_PX: f64 = 0.01;
/// A free (Ctrl+drag) scale stays undecided until the pointer has moved this far (logical
/// px), then locks to the dominant direction for the rest of the drag.
pub const DRAG_LOCK_PX: f64 = 4.0;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Viewport {
    pub width: f64,
    pub height: f64,
    /// Fractional bar index shown at the right edge (x = width).
    pub right_index: f64,
    /// Horizontal pixels per bar.
    pub bar_px: f64,
    /// Price (scaled units) at y = height.
    pub price_lo: f64,
    /// Price (scaled units) at y = 0.
    pub price_hi: f64,
    /// Fit the price range to visible bars every frame.
    pub auto_price: bool,
    /// A new bar shifts the view by one bar, so the newest bar keeps its place on screen.
    /// On exactly while the newest bar is visible ([`Viewport::newest_visible`]); a view
    /// scrolled back into history stays put.
    pub follow_live: bool,
    /// While following live: bars between the newest bar and the right edge. Set by
    /// whatever put the view at the live edge (pan, zoom, scaling), so new bars never make
    /// the view jump.
    pub live_margin: f64,
    /// A drag is in progress: new data must not move the time axis under the pointer.
    /// Set with [`Viewport::set_held`].
    pub held: bool,
    /// New data doesn't refit the price range: something aligned to it is under the pointer (the
    /// option chart's chain, decision 0029). The trader's own zoom and pan still apply.
    pub price_held: bool,
    /// `fit_prices` has found bars since this was last reset.
    pub price_fitted: bool,
    /// The auto-fit's smallest price range (scaled units; 0: only the tick's own minimum). Lets
    /// something aligned to the scale keep room: the option chart's chain needs several strikes.
    pub min_price_span: f64,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            width: 800.0,
            height: 600.0,
            right_index: 0.0,
            bar_px: DEFAULT_BAR_PX,
            price_lo: 0.0,
            price_hi: 1.0,
            auto_price: true,
            follow_live: true,
            live_margin: RIGHT_MARGIN_BARS,
            held: false,
            price_held: false,
            price_fitted: false,
            min_price_span: 0.0,
        }
    }
}

impl Viewport {
    pub fn set_size(&mut self, width: f64, height: f64) {
        self.width = width.max(1.0);
        self.height = height.max(1.0);
    }

    #[inline]
    pub fn index_to_x(&self, index: f64) -> f64 {
        self.width - (self.right_index - index) * self.bar_px
    }

    #[inline]
    pub fn x_to_index(&self, x: f64) -> f64 {
        self.right_index - (self.width - x) / self.bar_px
    }

    #[inline]
    pub fn price_to_y(&self, price: f64) -> f64 {
        (self.price_hi - price) / (self.price_hi - self.price_lo) * self.height
    }

    #[inline]
    pub fn y_to_price(&self, y: f64) -> f64 {
        self.price_hi - y / self.height * (self.price_hi - self.price_lo)
    }

    /// Indices of bars (possibly partially) visible, clamped to `0..len`.
    pub fn visible_range(&self, len: usize) -> std::ops::Range<usize> {
        if len == 0 {
            return 0..0;
        }
        // A bar counts as visible when its centre lies within half a bar of the plot edges.
        let first = (self.x_to_index(0.0) - 0.5).ceil().max(0.0);
        let last = (self.right_index + 0.5).floor();
        if last < 0.0 {
            return 0..0;
        }
        let first = (first as usize).min(len);
        let end = ((last as usize).saturating_add(1)).min(len);
        first..end.max(first)
    }

    /// Drag by (dx, dy) logical pixels: content follows the pointer.
    pub fn pan(&mut self, dx: f64, dy: f64, len: usize) {
        self.right_index -= dx / self.bar_px;
        if dy != 0.0 {
            self.auto_price = false;
            let per_px = (self.price_hi - self.price_lo) / self.height;
            self.price_hi += dy * per_px;
            self.price_lo += dy * per_px;
        }
        self.clamp(len);
        self.update_follow(len);
    }

    /// Zoom time axis by `factor` (>1 = zoom in) keeping the bar under `anchor_x` fixed.
    pub fn zoom_at(&mut self, anchor_x: f64, factor: f64, len: usize) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let anchor_index = self.x_to_index(anchor_x);
        self.bar_px = (self.bar_px * factor).clamp(MIN_BAR_PX, MAX_BAR_PX);
        self.right_index = anchor_index + (self.width - anchor_x) / self.bar_px;
        self.clamp(len);
        self.update_follow(len);
    }

    /// Zoom price axis by `factor` (>1 = zoom in) around `anchor_y`. Disables auto-scale.
    /// The span stays within [`MIN_PRICE_SPAN_TICKS`, `MAX_PRICE_SPAN_TICKS`] ticks.
    pub fn zoom_price_at(&mut self, anchor_y: f64, factor: f64, tick: i64) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let from = *self;
        self.scale_price_from(&from, self.y_to_price(anchor_y), factor, tick);
    }

    /// Set the price range to `from`'s, zoomed by `factor` around `anchor` (a price, which
    /// keeps its screen position). Disables auto-scale.
    fn scale_price_from(&mut self, from: &Viewport, anchor: f64, factor: f64, tick: i64) {
        let span0 = from.price_hi - from.price_lo;
        if !(anchor.is_finite() && span0.is_finite() && span0 > 0.0) {
            return;
        }
        let tick = tick.max(1) as f64;
        // Never jump on the first move if the current span is already outside the limits.
        let min = (MIN_PRICE_SPAN_TICKS * tick).min(span0);
        let max = (MAX_PRICE_SPAN_TICKS * tick).max(span0);
        let ratio = (span0 / factor).clamp(min, max) / span0;
        self.auto_price = false;
        self.price_hi = anchor + (from.price_hi - anchor) * ratio;
        self.price_lo = anchor - (anchor - from.price_lo) * ratio;
    }

    /// Whether the newest bar's centre is on screen.
    pub fn newest_visible(&self, len: usize) -> bool {
        len > 0 && (0.0..=self.width).contains(&self.index_to_x((len - 1) as f64))
    }

    /// Anchor for a wheel time-zoom with the pointer at `cursor_x`: the right edge while the
    /// newest bar is visible (so it stays pinned there, with the same margin in bars),
    /// otherwise the pointer.
    pub fn time_zoom_anchor(&self, cursor_x: f64, len: usize) -> f64 {
        if self.newest_visible(len) {
            self.width
        } else {
            cursor_x
        }
    }

    /// Re-enable price auto-scale (the range is refitted on the next `on_data`).
    pub fn restore_auto_price(&mut self) {
        self.auto_price = true;
        self.price_fitted = false;
    }

    /// Reset the time axis only (a double-click on it): the default bar width, back to the
    /// newest bar with the default margin, following live. The price range is untouched
    /// (a manual price scale stays manual).
    pub fn reset_time(&mut self, len: usize) {
        self.bar_px = DEFAULT_BAR_PX;
        self.follow_live = true;
        self.live_margin = RIGHT_MARGIN_BARS;
        self.snap_to_latest(len);
    }

    /// Jump to the newest bar and re-enable auto-scale and live following.
    pub fn reset(&mut self, len: usize) {
        self.auto_price = true;
        self.price_fitted = false;
        self.follow_live = true;
        self.live_margin = RIGHT_MARGIN_BARS;
        self.snap_to_latest(len);
    }

    /// Hold the time axis still during a drag. On release, live following resumes with the
    /// margin the view has *now* (bars that arrived during the drag don't cause a jump).
    pub fn set_held(&mut self, held: bool, len: usize) {
        if self.held && !held {
            self.update_follow(len);
        }
        self.held = held;
    }

    /// Call when the series changed. Keeps the newest bar in view if following (and not held).
    pub fn on_data(&mut self, bars: &[Bar], tick: i64) {
        if self.follow_live && !self.held {
            self.snap_to_latest(bars.len());
        }
        // A hold keeps a range, so it has to have one first (a new symbol, the trader's reset).
        if self.auto_price && (!self.price_held || !self.price_fitted) {
            self.fit_prices(bars, tick);
        }
    }

    /// Fit price range to the visible bars, with padding. `tick` (scaled units) gives a
    /// minimum range when the visible bars are flat.
    pub fn fit_prices(&mut self, bars: &[Bar], tick: i64) {
        let range = self.visible_range(bars.len());
        let mut lo = i64::MAX;
        let mut hi = i64::MIN;
        for b in &bars[range] {
            lo = lo.min(b.low.0);
            hi = hi.max(b.high.0);
        }
        if lo > hi {
            return; // nothing visible: keep the current range
        }
        let (lo, hi) = (lo as f64, hi as f64);
        let min_span = ((tick.max(1) * 10) as f64).max(self.min_price_span);
        let span = (hi - lo).max(min_span);
        let mid = (hi + lo) / 2.0;
        let pad = span * PRICE_PAD_FRACTION;
        self.price_lo = mid - span / 2.0 - pad;
        self.price_hi = mid + span / 2.0 + pad;
        self.price_fitted = true;
    }

    fn snap_to_latest(&mut self, len: usize) {
        if len > 0 {
            self.right_index = (len - 1) as f64 + self.live_margin;
        }
    }

    /// Never scroll so far that no bar is visible.
    fn clamp(&mut self, len: usize) {
        if len == 0 {
            return;
        }
        let max_right = (len - 1) as f64 + self.width / self.bar_px * 0.8;
        let min_right = self.width / self.bar_px * 0.2;
        self.right_index = self.right_index.clamp(min_right.min(max_right), max_right);
    }

    /// Follow live exactly while the newest bar is on screen, keeping its place.
    fn update_follow(&mut self, len: usize) {
        self.follow_live = self.newest_visible(len);
        if self.follow_live {
            self.live_margin = self.right_index - (len - 1) as f64;
        }
    }
}

/// Which scale a drag changes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ScaleAxis {
    Time,
    Price,
}

/// How a scale drag started.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ScaleMode {
    /// Ctrl+drag in the plot: horizontal scales time, vertical scales price, locked to the
    /// dominant direction once the pointer has moved [`DRAG_LOCK_PX`].
    Free,
    /// Drag on the time axis.
    Time,
    /// Drag on the price axis.
    Price,
}

/// A drag that scales one axis around the point under the mouse-down.
///
/// Every update is computed from the viewport as it was at mouse-down and the *total* drag
/// distance, so scaling is exactly exponential in distance and never drifts. Dragging right
/// widens bars (zoom in on time); dragging up stretches prices (zoom in on price). The data
/// point under the mouse-down keeps its screen position, except where the time clamp has to
/// keep bars on screen.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ScaleDrag {
    x: f64,
    y: f64,
    start: Viewport,
    axis: Option<ScaleAxis>,
}

impl ScaleDrag {
    /// Start a drag at plot-local (`x`, `y`). On the price axis `x` may be beyond the plot's
    /// width and on the time axis `y` beyond its height; only the scaled coordinate matters.
    pub fn new(vp: &Viewport, mode: ScaleMode, x: f64, y: f64) -> Self {
        ScaleDrag {
            x,
            y,
            start: *vp,
            axis: match mode {
                ScaleMode::Free => None,
                ScaleMode::Time => Some(ScaleAxis::Time),
                ScaleMode::Price => Some(ScaleAxis::Price),
            },
        }
    }

    /// The locked axis, or `None` while a free drag is still undecided.
    pub fn axis(&self) -> Option<ScaleAxis> {
        self.axis
    }

    /// Apply the drag with the pointer now at plot-local (`x`, `y`). Only the scaled axis
    /// of `vp` changes; the other keeps tracking data (e.g. price auto-scale while scaling
    /// time). Allocation-free.
    pub fn update(&mut self, vp: &mut Viewport, x: f64, y: f64, len: usize, tick: i64) {
        let (dx, dy) = (x - self.x, y - self.y);
        if !(dx.is_finite() && dy.is_finite()) {
            return;
        }
        let axis = match self.axis {
            Some(axis) => axis,
            None if dx.abs().max(dy.abs()) < DRAG_LOCK_PX => return,
            None => {
                let axis = if dx.abs() >= dy.abs() {
                    ScaleAxis::Time
                } else {
                    ScaleAxis::Price
                };
                self.axis = Some(axis);
                axis
            }
        };
        let s = &self.start;
        match axis {
            ScaleAxis::Time => {
                let anchor = s.x_to_index(self.x);
                vp.bar_px = (s.bar_px * (dx * SCALE_PER_PX).exp()).clamp(MIN_BAR_PX, MAX_BAR_PX);
                vp.right_index = anchor + (vp.width - self.x) / vp.bar_px;
                vp.clamp(len);
                vp.update_follow(len);
            }
            ScaleAxis::Price => {
                let anchor = s.y_to_price(self.y);
                vp.scale_price_from(s, anchor, (-dy * SCALE_PER_PX).exp(), tick);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Price, Qty, UtcMillis};

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn vp() -> Viewport {
        Viewport {
            width: 1000.0,
            height: 500.0,
            right_index: 99.0,
            bar_px: 10.0,
            price_lo: 1000.0,
            price_hi: 2000.0,
            auto_price: true,
            follow_live: true,
            live_margin: RIGHT_MARGIN_BARS,
            held: false,
            price_held: false,
            price_fitted: false,
            min_price_span: 0.0,
        }
    }

    fn series(n: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| {
                let p = 1000 + (i as i64 % 7) * 25;
                Bar {
                    time: UtcMillis(i as i64 * 60_000),
                    open: Price(p),
                    high: Price(p + 50),
                    low: Price(p - 50),
                    close: Price(p + 25),
                    volume: Qty(1),
                }
            })
            .collect()
    }

    #[test]
    fn index_x_roundtrip() {
        let v = vp();
        assert!(approx(v.index_to_x(99.0), 1000.0));
        assert!(approx(v.index_to_x(0.0), 10.0));
        for x in [0.0, 1.5, 333.3, 1000.0] {
            assert!(approx(v.index_to_x(v.x_to_index(x)), x));
        }
    }

    #[test]
    fn price_y_roundtrip_and_orientation() {
        let v = vp();
        assert!(approx(v.price_to_y(2000.0), 0.0));
        assert!(approx(v.price_to_y(1000.0), 500.0));
        assert!(
            v.price_to_y(1900.0) < v.price_to_y(1100.0),
            "higher price is higher on screen"
        );
        for y in [0.0, 12.5, 250.0, 499.0] {
            assert!(approx(v.price_to_y(v.y_to_price(y)), y));
        }
    }

    #[test]
    fn visible_range_clamps() {
        let v = vp(); // shows indices ~0..=99
        assert_eq!(v.visible_range(200), 0..100);
        assert_eq!(v.visible_range(50), 0..50);
        assert_eq!(v.visible_range(0), 0..0);
        let mut w = vp();
        w.right_index = 149.0;
        assert_eq!(w.visible_range(500), 49..150);
    }

    #[test]
    fn pan_moves_content_with_pointer() {
        let mut v = vp();
        v.right_index = 150.0;
        v.pan(100.0, 0.0, 1000); // drag right by 10 bars => look further into the past
        assert!(approx(v.right_index, 140.0));
        assert!(!v.follow_live);
        assert!(v.auto_price, "horizontal pan keeps auto-scale");
        v.pan(0.0, 50.0, 1000); // drag down => prices move down => range shifts up
        assert!(!v.auto_price);
        assert!(approx(v.price_hi, 2100.0));
        assert!(approx(v.price_lo, 1100.0));
    }

    #[test]
    fn pan_is_clamped() {
        let mut v = vp();
        v.pan(1.0e9, 0.0, 100);
        assert!(
            !v.visible_range(100).is_empty(),
            "can't scroll past the first bar"
        );
        v.pan(-1.0e9, 0.0, 100);
        assert!(
            !v.visible_range(100).is_empty(),
            "can't scroll past the last bar"
        );
        assert!(v.follow_live, "scrolling to the end re-enables follow");
    }

    #[test]
    fn zoom_keeps_anchor_fixed() {
        let mut v = vp();
        v.right_index = 500.0;
        let anchor_x = 400.0;
        let before = v.x_to_index(anchor_x);
        v.zoom_at(anchor_x, 2.0, 10_000);
        assert!(approx(v.bar_px, 20.0));
        assert!(approx(v.x_to_index(anchor_x), before));
        v.zoom_at(anchor_x, 0.25, 10_000);
        assert!(approx(v.bar_px, 5.0));
        assert!(approx(v.x_to_index(anchor_x), before));
    }

    #[test]
    fn zoom_is_clamped() {
        let mut v = vp();
        v.zoom_at(500.0, 1e9, 1000);
        assert_eq!(v.bar_px, MAX_BAR_PX);
        v.zoom_at(500.0, 1e-9, 1000);
        assert_eq!(v.bar_px, MIN_BAR_PX);
        let before = v;
        v.zoom_at(500.0, f64::NAN, 1000);
        assert_eq!(v, before);
    }

    #[test]
    fn price_zoom_keeps_anchor_and_disables_auto() {
        let mut v = vp();
        let p = v.y_to_price(100.0);
        v.zoom_price_at(100.0, 2.0, 25);
        assert!(!v.auto_price);
        assert!(approx(v.y_to_price(100.0), p));
        assert!(approx(v.price_hi - v.price_lo, 500.0));
    }

    #[test]
    fn follow_and_fit() {
        let bars = series(300);
        let mut v = vp();
        v.on_data(&bars, 25);
        assert!(approx(v.right_index, 299.0 + RIGHT_MARGIN_BARS));
        let range = v.visible_range(bars.len());
        let lo = bars[range.clone()].iter().map(|b| b.low.0).min().unwrap() as f64;
        let hi = bars[range].iter().map(|b| b.high.0).max().unwrap() as f64;
        assert!(v.price_lo < lo && v.price_hi > hi);
        // Flat data still gets a non-zero range.
        let mut flat = series(5);
        for b in &mut flat {
            *b = Bar {
                high: Price(100),
                low: Price(100),
                ..*b
            };
        }
        v.fit_prices(&flat, 25);
        assert!(v.price_hi - v.price_lo >= 250.0);
    }

    #[test]
    fn not_following_ignores_new_bars() {
        let mut bars = series(300);
        let mut v = vp();
        v.on_data(&bars, 25);
        v.pan(500.0, 0.0, bars.len());
        let r = v.right_index;
        bars.extend(series(10));
        v.on_data(&bars, 25);
        assert_eq!(v.right_index, r);
    }

    // ---- wheel zoom pinning, drag scaling --------------------------------------------

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6 * a.abs().max(b.abs()).max(1.0)
    }

    #[test]
    fn time_zoom_pins_newest_bar_to_right_edge() {
        let bars = series(300);
        let mut v = vp();
        v.on_data(&bars, 25); // following live: newest bar 299, 3 bars of margin
        assert!(v.newest_visible(bars.len()));
        let anchor = v.time_zoom_anchor(200.0, bars.len());
        assert_eq!(anchor, v.width, "pointer position is ignored while live");
        for factor in [2.0, 0.5, 0.25, 3.0] {
            v.zoom_at(anchor, factor, bars.len());
            assert!(close(v.right_index, 299.0 + RIGHT_MARGIN_BARS), "{v:?}");
            assert!(v.follow_live);
            v.on_data(&bars, 25);
            assert!(close(v.right_index, 299.0 + RIGHT_MARGIN_BARS));
        }
    }

    #[test]
    fn time_zoom_uses_cursor_when_newest_bar_is_off_screen() {
        let mut v = vp();
        v.right_index = 500.0;
        v.follow_live = false;
        assert!(!v.newest_visible(1000));
        assert_eq!(v.time_zoom_anchor(321.0, 1000), 321.0);
        // Newest bar exactly on the right or left edge still counts as visible.
        v.right_index = 999.0;
        assert!(v.newest_visible(1000));
        v.right_index = 999.0 + 99.9; // newest bar at x = 1.0
        assert!(v.newest_visible(1000));
        v.right_index = 999.0 + 100.1; // newest bar at x = -1.0
        assert!(!v.newest_visible(1000));
        assert!(!v.newest_visible(0), "no bars: nothing to pin");
        assert_eq!(v.time_zoom_anchor(321.0, 0), 321.0);
    }

    #[test]
    fn following_keeps_the_margin_the_user_chose() {
        let mut bars = series(300);
        let mut v = vp();
        v.on_data(&bars, 25);
        v.pan(-100.0, 0.0, bars.len()); // 10 more bars of empty space on the right
        assert!(v.follow_live);
        assert!(close(v.live_margin, RIGHT_MARGIN_BARS + 10.0));
        bars.extend(series(1));
        v.on_data(&bars, 25);
        assert!(
            close(v.right_index, 300.0 + RIGHT_MARGIN_BARS + 10.0),
            "no jump"
        );
        v.reset(bars.len());
        assert!(close(v.right_index, 300.0 + RIGHT_MARGIN_BARS));
    }

    #[test]
    fn a_new_bar_shifts_the_view_only_while_the_newest_bar_is_visible() {
        let mut bars = series(300);
        let mut v = vp(); // 1000 px, 10 px per bar
        v.on_data(&bars, 25);
        // Newest bar in the middle of the plot: still visible, so a new bar keeps it there
        // (the view shifts by one bar).
        v.pan(-500.0, 0.0, bars.len());
        assert!(v.newest_visible(bars.len()));
        assert!(v.follow_live);
        let x = v.index_to_x(299.0);
        bars.extend(series(1));
        v.on_data(&bars, 25);
        assert!(
            close(v.index_to_x(300.0), x),
            "the newest bar keeps its place"
        );
        // Scrolled back so the newest bar is off the right edge: new bars don't move it.
        v.pan(600.0, 0.0, bars.len());
        assert!(!v.newest_visible(bars.len()));
        assert!(!v.follow_live);
        let right = v.right_index;
        bars.extend(series(5));
        v.on_data(&bars, 25);
        assert_eq!(v.right_index, right);
        // Newest bar's centre exactly on the right edge: visible, so following again.
        v.pan(-10.0 * (bars.len() as f64 - 1.0 - right), 0.0, bars.len());
        assert!(close(v.right_index, (bars.len() - 1) as f64));
        assert!(v.follow_live);
    }

    #[test]
    fn time_reset_leaves_the_price_axis_alone() {
        let bars = series(300);
        let mut v = vp();
        v.on_data(&bars, 25);
        v.zoom_price_at(100.0, 2.0, 25);
        let (lo, hi) = (v.price_lo, v.price_hi);
        v.zoom_at(500.0, 3.0, bars.len());
        v.pan(900.0, 0.0, bars.len());
        assert!(!v.follow_live);
        v.reset_time(bars.len());
        assert_eq!(v.bar_px, DEFAULT_BAR_PX);
        assert!(v.follow_live);
        assert!(close(v.right_index, 299.0 + RIGHT_MARGIN_BARS));
        assert!(!v.auto_price, "manual price scale stays");
        assert_eq!((v.price_lo, v.price_hi), (lo, hi));
        v.on_data(&bars, 25);
        assert_eq!((v.price_lo, v.price_hi), (lo, hi));
    }

    fn drag_at(mode: ScaleMode, x: f64, y: f64) -> (Viewport, ScaleDrag) {
        let mut v = vp();
        v.right_index = 500.0;
        v.follow_live = false;
        (v, ScaleDrag::new(&v, mode, x, y))
    }

    #[test]
    fn free_drag_waits_then_locks_to_dominant_direction() {
        let (mut v, mut d) = drag_at(ScaleMode::Free, 400.0, 200.0);
        let before = v;
        d.update(&mut v, 402.0, 197.0, 10_000, 25);
        assert_eq!(d.axis(), None, "below the lock distance");
        assert_eq!(v, before, "nothing moves before the lock");
        d.update(&mut v, 403.0, 192.0, 10_000, 25); // |dy| 8 > |dx| 3
        assert_eq!(d.axis(), Some(ScaleAxis::Price));
        assert!(!v.auto_price);
        assert_eq!(v.bar_px, before.bar_px);
        // Locked: a later horizontal swing doesn't switch to time, nor change it.
        d.update(&mut v, 600.0, 192.0, 10_000, 25);
        assert_eq!(d.axis(), Some(ScaleAxis::Price));
        assert_eq!(
            (v.bar_px, v.right_index),
            (before.bar_px, before.right_index)
        );

        let (mut v, mut d) = drag_at(ScaleMode::Free, 400.0, 200.0);
        d.update(&mut v, 395.0, 203.0, 10_000, 25); // |dx| 5 > |dy| 3
        assert_eq!(d.axis(), Some(ScaleAxis::Time));
        assert!(v.auto_price, "scaling time leaves price auto-scale alone");
        let (lo, hi) = (v.price_lo, v.price_hi);
        d.update(&mut v, 395.0, 400.0, 10_000, 25);
        assert_eq!(
            (v.price_lo, v.price_hi),
            (lo, hi),
            "vertical motion ignored"
        );
    }

    #[test]
    fn axis_drags_are_locked_from_the_start() {
        let (mut v, mut d) = drag_at(ScaleMode::Price, 1030.0, 200.0);
        assert_eq!(d.axis(), Some(ScaleAxis::Price));
        d.update(&mut v, 1200.0, 199.0, 10_000, 25); // tiny vertical, big horizontal
        assert!(
            v.price_hi - v.price_lo < 1000.0,
            "1 px up already scales price"
        );
        assert_eq!(v.bar_px, 10.0);
        let (mut v, mut d) = drag_at(ScaleMode::Time, 400.0, 520.0);
        d.update(&mut v, 401.0, 100.0, 10_000, 25);
        assert!(v.bar_px > 10.0);
        assert_eq!((v.price_lo, v.price_hi), (1000.0, 2000.0));
        assert!(v.auto_price);
    }

    #[test]
    fn drag_keeps_the_point_under_mouse_down_fixed() {
        for (x, y) in [(400.0, 200.0), (13.0, 7.0), (999.0, 499.0)] {
            let (mut v, mut d) = drag_at(ScaleMode::Free, x, y);
            let (index, price) = (v.x_to_index(x), v.y_to_price(y));
            for dx in [10.0, 80.0, -150.0, 37.5] {
                d.update(&mut v, x + dx, y + 1.0, 10_000, 25);
                assert!(close(v.x_to_index(x), index), "time anchor moved at {x}");
            }
            let (mut v, mut d) = drag_at(ScaleMode::Free, x, y);
            for dy in [-10.0, -80.0, 150.0, 37.5] {
                d.update(&mut v, x + 1.0, y + dy, 10_000, 25);
                assert!(close(v.y_to_price(y), price), "price anchor moved at {y}");
            }
        }
    }

    #[test]
    fn drag_scaling_is_exponential_in_distance() {
        let (mut v, mut d) = drag_at(ScaleMode::Time, 500.0, 0.0);
        d.update(&mut v, 600.0, 0.0, 10_000, 25);
        assert!(close(v.bar_px, 10.0 * 1.0f64.exp()));
        d.update(&mut v, 700.0, 0.0, 10_000, 25);
        assert!(
            close(v.bar_px, 10.0 * 2.0f64.exp()),
            "from the start, not compounding"
        );
        d.update(&mut v, 400.0, 0.0, 10_000, 25);
        assert!(close(v.bar_px, 10.0 / 1.0f64.exp()), "drag left zooms out");
        d.update(&mut v, 500.0, 0.0, 10_000, 25);
        assert!(
            close(v.bar_px, 10.0),
            "back to the start restores the scale"
        );

        let (mut v, mut d) = drag_at(ScaleMode::Price, 1030.0, 250.0);
        d.update(&mut v, 1030.0, 150.0, 10_000, 25); // up 100 px
        assert!(
            close(v.price_hi - v.price_lo, 1000.0 / 1.0f64.exp()),
            "up zooms in"
        );
        d.update(&mut v, 1030.0, 350.0, 10_000, 25); // down 100 px
        assert!(
            close(v.price_hi - v.price_lo, 1000.0 * 1.0f64.exp()),
            "down zooms out"
        );
    }

    #[test]
    fn drag_scaling_is_clamped() {
        let (mut v, mut d) = drag_at(ScaleMode::Time, 500.0, 0.0);
        d.update(&mut v, 1.0e6, 0.0, 10_000, 25);
        assert_eq!(v.bar_px, MAX_BAR_PX);
        d.update(&mut v, -1.0e6, 0.0, 10_000, 25);
        assert_eq!(v.bar_px, MIN_BAR_PX);
        d.update(&mut v, f64::NAN, 0.0, 10_000, 25);
        assert_eq!(v.bar_px, MIN_BAR_PX, "NaN ignored");

        let (mut v, mut d) = drag_at(ScaleMode::Price, 1030.0, 250.0);
        d.update(&mut v, 1030.0, -1.0e6, 10_000, 25);
        assert!(close(v.price_hi - v.price_lo, MIN_PRICE_SPAN_TICKS * 25.0));
        assert!(
            close(v.y_to_price(250.0), 1500.0),
            "anchor survives the clamp"
        );
        d.update(&mut v, 1030.0, 1.0e6, 10_000, 25);
        assert!(close(v.price_hi - v.price_lo, MAX_PRICE_SPAN_TICKS * 25.0));
        assert!(close(v.y_to_price(250.0), 1500.0));
    }

    #[test]
    fn price_clamp_never_jumps_when_already_outside_limits() {
        let mut v = vp(); // span 1000 = 4 ticks of 250
        let mut d = ScaleDrag::new(&v, ScaleMode::Price, 1030.0, 250.0);
        d.update(&mut v, 1030.0, 249.0, 1000, 500); // min span would be 2000
        assert!(
            close(v.price_hi - v.price_lo, 1000.0),
            "no jump to the minimum"
        );
        v.zoom_price_at(250.0, 2.0, 500);
        assert!(
            close(v.price_hi - v.price_lo, 1000.0),
            "can't zoom further in"
        );
        v.zoom_price_at(250.0, 0.5, 500);
        assert!(close(v.price_hi - v.price_lo, 2000.0), "can zoom out");
    }

    #[test]
    fn time_drag_at_first_and_last_bar() {
        let len = 200;
        // Anchored on the first bar.
        let mut v = vp();
        v.right_index = 60.0; // bar 0 at x = 400
        v.follow_live = false;
        let x0 = v.index_to_x(0.0);
        let mut d = ScaleDrag::new(&v, ScaleMode::Time, x0, 520.0);
        d.update(&mut v, x0 + 90.0, 520.0, len, 25);
        assert!(
            close(v.index_to_x(0.0), x0),
            "first bar stays under the pointer"
        );
        // Zooming far out keeps bars on screen instead of losing the data.
        d.update(&mut v, x0 - 1.0e5, 520.0, len, 25);
        assert!(!v.visible_range(len).is_empty());

        // Anchored on the newest bar while following live: no jump, still following.
        let bars = series(len);
        let mut v = vp();
        v.on_data(&bars, 25);
        let x = v.index_to_x((len - 1) as f64);
        let mut d = ScaleDrag::new(&v, ScaleMode::Time, x, 520.0);
        for dx in [40.0, -60.0, 120.0] {
            d.update(&mut v, x + dx, 520.0, len, 25);
            assert!(close(v.index_to_x((len - 1) as f64), x));
            assert!(v.follow_live);
            let before = v.right_index;
            v.on_data(&bars, 25);
            assert!(
                close(v.right_index, before),
                "next frame doesn't move the view"
            );
        }
    }

    #[test]
    fn restore_auto_price_refits() {
        let bars = series(300);
        let mut v = vp();
        v.on_data(&bars, 25);
        let fitted = (v.price_lo, v.price_hi);
        v.zoom_price_at(100.0, 3.0, 25);
        assert!(!v.auto_price);
        v.on_data(&bars, 25);
        assert_ne!((v.price_lo, v.price_hi), fitted, "manual range kept");
        v.restore_auto_price();
        v.on_data(&bars, 25);
        assert_eq!((v.price_lo, v.price_hi), fitted);
    }

    #[test]
    fn held_view_ignores_new_bars_and_resumes_without_a_jump() {
        let mut bars = series(300);
        let mut v = vp();
        v.on_data(&bars, 25);
        assert!(v.follow_live);
        v.set_held(true, bars.len());
        let r = v.right_index;
        bars.extend(series(2));
        v.on_data(&bars, 25);
        assert_eq!(v.right_index, r, "nothing moves under the pointer mid-drag");
        v.set_held(false, bars.len());
        v.on_data(&bars, 25);
        assert!(close(v.right_index, r), "release doesn't jump");
        assert!(v.follow_live);
        bars.extend(series(1));
        v.on_data(&bars, 25);
        assert!(close(v.right_index, r + 1.0), "and live following resumes");
    }

    /// Holding the price leaves time alone: the chart keeps following new bars.
    #[test]
    fn a_held_price_still_follows_the_bars() {
        let bars = |n: i64| -> Vec<Bar> {
            (0..n)
                .map(|i| Bar {
                    time: UtcMillis(i * 60_000),
                    open: Price(100),
                    high: Price(110 + i),
                    low: Price(100),
                    close: Price(110 + i),
                    volume: Qty(1),
                })
                .collect()
        };
        let mut v = Viewport::default();
        v.set_size(800.0, 600.0);
        v.on_data(&bars(10), 1);
        let (right, prices) = (v.right_index, (v.price_lo, v.price_hi));
        v.price_held = true;
        v.on_data(&bars(40), 1);
        assert!(v.right_index > right, "time still follows");
        assert_eq!((v.price_lo, v.price_hi), prices, "the price doesn't move");
    }

    /// A hold keeps a range it has: before the first fit, and after the trader's own reset of the
    /// scale, the bars still fit.
    #[test]
    fn a_hold_doesnt_stop_the_first_fit_or_the_traders_reset() {
        let bar = |low: i64, high: i64| Bar {
            time: UtcMillis(0),
            open: Price(low),
            high: Price(high),
            low: Price(low),
            close: Price(high),
            volume: Qty(1),
        };
        let mut v = Viewport::default();
        v.set_size(800.0, 600.0);
        v.right_index = 0.0;
        v.price_held = true;
        v.on_data(&[bar(68_300, 68_450)], 1);
        assert!(v.price_lo < 68_300.0 && v.price_hi > 68_450.0, "first fit");
        let fitted = (v.price_lo, v.price_hi);
        v.on_data(&[bar(60_000, 70_000)], 1);
        assert_eq!((v.price_lo, v.price_hi), fitted, "then it holds");
        v.restore_auto_price();
        v.on_data(&[bar(60_000, 70_000)], 1);
        assert!(v.price_lo < 60_000.0, "the reset refits");
        assert!(v.price_held, "and the hold goes on");
        let fitted = (v.price_lo, v.price_hi);
        v.on_data(&[bar(68_300, 68_450)], 1);
        assert_eq!((v.price_lo, v.price_hi), fitted);
        // A double-click on the plot, after the trader's own zoom.
        v.auto_price = false;
        v.reset(1);
        v.right_index = 0.0;
        v.on_data(&[bar(68_300, 68_450)], 1);
        assert!(v.price_hi < 69_000.0, "the plot's reset refits too");
    }

    /// A minimum span widens the fit around the bars, for what's aligned to the scale; bars that
    /// already span more are unchanged, and while the price is held nothing refits.
    #[test]
    fn the_fit_keeps_a_minimum_span_around_the_bars() {
        let bar = |low: i64, high: i64| Bar {
            time: UtcMillis(0),
            open: Price(low),
            high: Price(high),
            low: Price(low),
            close: Price(high),
            volume: Qty(1),
        };
        let bars = [bar(68_300, 68_450)];
        let mut v = Viewport::default();
        v.set_size(800.0, 600.0);
        v.right_index = 0.0;
        v.fit_prices(&bars, 1);
        let narrow = v.price_hi - v.price_lo;
        v.min_price_span = 1_000.0;
        v.fit_prices(&bars, 1);
        let wide = v.price_hi - v.price_lo;
        assert!(wide >= 1_000.0 && wide > narrow, "{narrow} {wide}");
        let mid = (v.price_hi + v.price_lo) / 2.0;
        assert_eq!(mid, (68_300.0 + 68_450.0) / 2.0, "around the bars");
        v.min_price_span = 10.0;
        v.fit_prices(&bars, 1);
        assert_eq!(v.price_hi - v.price_lo, narrow, "bars span more: unchanged");
        // Held: new data moves nothing.
        v.price_held = true;
        let before = (v.price_lo, v.price_hi);
        v.on_data(&[bar(60_000, 70_000)], 1);
        assert_eq!((v.price_lo, v.price_hi), before);
    }
}
