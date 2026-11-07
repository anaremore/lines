//! Chart sub-panes (roadmap 10e: cumulative delta is the first): a strip below the price plot
//! that shares the plot's time axis and crosshair, with its own value scale. Pure layout,
//! tested here; the chart draws from it.
//!
//! - The chart's height above the time axis is split between the price plot and the sub-pane
//!   by a **ratio** (the sub-pane's share), saved with the chart (`ChartState::subpane`).
//! - A **divider** between them is dragged to change the ratio; neither side gets smaller
//!   than its minimum (the price plot's is larger).
//! - The time axis stays at the bottom, under the sub-pane; bars line up because both use the
//!   chart's viewport for x.

/// The sub-pane's share of the height when first shown.
pub const DEFAULT_RATIO: f32 = 0.25;
/// Ratios outside this range are clamped (saved files, drags).
pub const MIN_RATIO: f32 = 0.08;
pub const MAX_RATIO: f32 = 0.7;
/// The divider's height (logical px), drawn as a line in its middle; also its hit zone.
pub const DIVIDER_PX: f32 = 6.0;
/// Neither pane gets smaller than this (logical px).
pub const MIN_PRICE_PX: f32 = 60.0;
pub const MIN_SUB_PX: f32 = 30.0;

/// Vertical layout of the area above the time axis, plot-local (0 = the top of the chart).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Split {
    /// The price plot's height.
    pub price_h: f32,
    /// The divider's top (`price_h`); the sub-pane starts below it.
    pub divider_top: f32,
    /// The sub-pane's top and height.
    pub sub_top: f32,
    pub sub_h: f32,
}

impl Split {
    /// The divider's middle (where its line is drawn).
    pub fn divider_mid(&self) -> f32 {
        self.divider_top + DIVIDER_PX * 0.5
    }

    /// Whether plot-local `y` is on the divider.
    pub fn on_divider(&self, y: f32) -> bool {
        y >= self.divider_top && y < self.sub_top
    }

    /// Whether plot-local `y` is in the sub-pane.
    pub fn in_sub(&self, y: f32) -> bool {
        y >= self.sub_top && y < self.sub_top + self.sub_h
    }
}

pub fn clamp_ratio(r: f32) -> f32 {
    if r.is_finite() {
        r.clamp(MIN_RATIO, MAX_RATIO)
    } else {
        DEFAULT_RATIO
    }
}

/// Split `total` px (above the time axis) for a sub-pane of share `ratio`. `None` when there
/// isn't room for both panes at their minimums (the sub-pane is then not shown).
#[cfg(test)]
pub fn split(total: f32, ratio: f32) -> Option<Split> {
    split_at_scale(total, ratio, 1.0)
}

/// [`split`] at a display scale: the sub-pane's height is a whole number of device pixels,
/// so its top (and every snapped edge drawn from it) sits on a device pixel when `total`
/// does.
pub fn split_at_scale(total: f32, ratio: f32, scale: f32) -> Option<Split> {
    let avail = total - DIVIDER_PX;
    if avail < MIN_PRICE_PX + MIN_SUB_PX {
        return None;
    }
    let s = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let sub_h =
        ((avail * clamp_ratio(ratio) * s).round() / s).clamp(MIN_SUB_PX, avail - MIN_PRICE_PX);
    let price_h = avail - sub_h;
    Some(Split {
        price_h,
        divider_top: price_h,
        sub_top: price_h + DIVIDER_PX,
        sub_h,
    })
}

/// The ratio after dragging the divider by `dy` px (down = a smaller sub-pane) from where a
/// drag started at `start_ratio`, kept within both panes' minimums.
pub fn drag(total: f32, start_ratio: f32, dy: f32) -> f32 {
    let avail = (total - DIVIDER_PX).max(1.0);
    let sub_h = avail * clamp_ratio(start_ratio) - dy;
    let lo = (MIN_SUB_PX / avail).max(MIN_RATIO);
    let hi = ((avail - MIN_PRICE_PX) / avail).min(MAX_RATIO);
    if lo > hi {
        return clamp_ratio(start_ratio);
    }
    (sub_h / avail).clamp(lo, hi)
}

/// A value scale for a sub-pane: the visible `[lo, hi]` padded so candles don't touch the
/// edges (auto-fit, as the price plot; zero is drawn only when it is in range). Maps a value
/// to a sub-pane-local y.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Scale {
    pub lo: f64,
    pub hi: f64,
    pub height: f32,
}

impl Scale {
    pub fn fit(lo: i64, hi: i64, height: f32) -> Scale {
        let (lo, hi) = (lo.min(hi) as f64, hi.max(lo) as f64);
        let span = (hi - lo).max(1.0);
        let pad = span * 0.08;
        Scale {
            lo: lo - pad,
            hi: hi + pad,
            height,
        }
    }

    pub fn y(&self, v: f64) -> f32 {
        ((self.hi - v) / (self.hi - self.lo) * self.height as f64) as f32
    }

    /// The value at sub-pane-local `y` (the inverse of [`Scale::y`]).
    pub fn value(&self, y: f32) -> f64 {
        self.hi - f64::from(y) / f64::from(self.height.max(1.0)) * (self.hi - self.lo)
    }

    pub fn contains(&self, v: f64) -> bool {
        (self.lo..=self.hi).contains(&v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_split_respects_the_ratio_and_both_minimums() {
        let s = split(606.0, 0.25).unwrap();
        assert_eq!(s.sub_h, 150.0);
        assert_eq!(s.price_h, 450.0);
        assert_eq!(s.sub_top, 456.0);
        assert_eq!(s.price_h + DIVIDER_PX + s.sub_h, 606.0);
        // Tiny ratios and tiny charts: minimums win, and too small means no sub-pane.
        assert_eq!(
            split(606.0, 0.0).unwrap().sub_h,
            (600.0 * MIN_RATIO).round()
        );
        let tight = split(MIN_PRICE_PX + MIN_SUB_PX + DIVIDER_PX, 0.5).unwrap();
        assert_eq!((tight.price_h, tight.sub_h), (MIN_PRICE_PX, MIN_SUB_PX));
        assert_eq!(split(MIN_PRICE_PX + MIN_SUB_PX, 0.25), None);
        // A damaged saved ratio falls back to the default.
        assert_eq!(split(606.0, f32::NAN).unwrap().sub_h, 150.0);
    }

    #[test]
    fn at_fractional_scales_the_sub_pane_starts_on_a_device_pixel() {
        for scale in [1.25f32, 1.5, 1.75, 2.0] {
            for ratio in [0.1, 0.25, 0.33, 0.5] {
                let s = split_at_scale(600.0, ratio, scale).unwrap();
                let dev = s.sub_top * scale;
                assert!(
                    (dev - dev.round()).abs() < 1e-3,
                    "{scale} {ratio}: {}",
                    s.sub_top
                );
            }
        }
    }

    #[test]
    fn hit_zones() {
        let s = split(606.0, 0.25).unwrap();
        assert!(s.on_divider(450.0) && s.on_divider(455.9));
        assert!(!s.on_divider(449.9) && !s.on_divider(456.0));
        assert!(s.in_sub(456.0) && s.in_sub(605.9) && !s.in_sub(606.0));
        assert_eq!(s.divider_mid(), 453.0);
    }

    #[test]
    fn dragging_moves_the_divider_within_limits() {
        // Up 60 px: the sub-pane grows from 150 to 210 of 600.
        assert!((drag(606.0, 0.25, -60.0) - 0.35).abs() < 1e-6);
        // Down far: the sub-pane's minimum.
        assert!((drag(606.0, 0.25, 1_000.0) - MIN_RATIO.max(MIN_SUB_PX / 600.0)).abs() < 1e-6);
        // Up far: the price plot keeps its minimum (or the ratio cap).
        let r = drag(606.0, 0.25, -1_000.0);
        assert!((r - MAX_RATIO.min((600.0 - MIN_PRICE_PX) / 600.0)).abs() < 1e-6);
        // Round trip through the split: the divider follows the pointer.
        let before = split(606.0, 0.25).unwrap();
        let after = split(606.0, drag(606.0, 0.25, -30.0)).unwrap();
        assert_eq!(after.divider_top, before.divider_top - 30.0);
    }

    #[test]
    fn the_scale_fits_the_visible_values_with_padding() {
        // Cumulative delta far from zero still fills the pane.
        let s = Scale::fit(-5_200, -4_800, 100.0);
        assert!(s.lo < -5_200.0 && s.hi > -4_800.0);
        assert!(!s.contains(0.0), "zero only when in range");
        assert!(s.y(-4_800.0) > 0.0 && s.y(-5_200.0) < 100.0, "padded");
        assert!(s.y(-4_800.0) < s.y(-5_200.0), "up is up");
        assert!(
            (s.y(-4_800.0) - (100.0 - s.y(-5_200.0))).abs() < 1e-3,
            "centred"
        );
        assert!(
            (s.value(s.y(-5_000.0)) + 5_000.0).abs() < 1e-6,
            "value inverts y"
        );
        let flat = Scale::fit(7, 7, 100.0);
        assert!(flat.hi > flat.lo && flat.contains(7.0));
    }
}
