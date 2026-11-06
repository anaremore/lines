//! Chart coordinate math: bar index / price  <->  pixels, plus pan and zoom.
//!
//! The x axis is in *bar-index* space, not wall time, so session gaps (nights, weekends,
//! the daily CME halt) take no horizontal room. Bar `i`'s centre is at index `i as f64`.
//! The y axis is in the contract's scaled-integer price units (as `f64`, display only).
//! Pixel coordinates are logical pixels relative to the plot area's top-left corner.

use crate::bars::Bar;

pub const MIN_BAR_PX: f64 = 1.0;
pub const MAX_BAR_PX: f64 = 80.0;
/// Empty space (in bars) kept to the right of the newest bar when following live.
pub const RIGHT_MARGIN_BARS: f64 = 3.0;
const PRICE_PAD_FRACTION: f64 = 0.08;

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
    /// Keep the newest bar in view as new bars arrive.
    pub follow_live: bool,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            width: 800.0,
            height: 600.0,
            right_index: 0.0,
            bar_px: 8.0,
            price_lo: 0.0,
            price_hi: 1.0,
            auto_price: true,
            follow_live: true,
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
    pub fn zoom_price_at(&mut self, anchor_y: f64, factor: f64) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        self.auto_price = false;
        let anchor = self.y_to_price(anchor_y);
        self.price_hi = anchor + (self.price_hi - anchor) / factor;
        self.price_lo = anchor - (anchor - self.price_lo) / factor;
    }

    /// Jump to the newest bar and re-enable auto-scale and live following.
    pub fn reset(&mut self, len: usize) {
        self.auto_price = true;
        self.follow_live = true;
        self.snap_to_latest(len);
    }

    /// Call when the series changed. Keeps the newest bar in view if following.
    pub fn on_data(&mut self, bars: &[Bar], tick: i64) {
        if self.follow_live {
            self.snap_to_latest(bars.len());
        }
        if self.auto_price {
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
        let min_span = (tick.max(1) * 10) as f64;
        let span = (hi - lo).max(min_span);
        let mid = (hi + lo) / 2.0;
        let pad = span * PRICE_PAD_FRACTION;
        self.price_lo = mid - span / 2.0 - pad;
        self.price_hi = mid + span / 2.0 + pad;
    }

    fn snap_to_latest(&mut self, len: usize) {
        if len > 0 {
            self.right_index = (len - 1) as f64 + RIGHT_MARGIN_BARS;
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

    fn update_follow(&mut self, len: usize) {
        self.follow_live = len > 0 && self.right_index >= (len - 1) as f64 + 0.5;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Price, UtcMillis};

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
                    volume: 1,
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
        v.zoom_price_at(100.0, 2.0);
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
}
