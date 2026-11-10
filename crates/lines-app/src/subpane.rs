//! Chart lower panes (sub-panes; roadmap 10e: cumulative delta was the first, Volume the
//! second): strips below the price plot that share the plot's time axis and crosshair, each
//! with its own value scale. Pure layout, tested here; the chart draws from it.
//!
//! The mechanism is reusable: a study declares that it draws in a lower pane
//! (`IndicatorConfig::in_subpane`), and the chart stacks one pane per such shown study, top
//! to bottom in the studies' order ([`stack_at_scale`]), up to [`MAX_PANES`].
//!
//! - The chart's height above the time axis is shared between the price plot and the lower
//!   panes, each pane by a **ratio** (its share), saved with the chart (`ChartState::subpane`
//!   for the first, `ChartState::subpanes` for all when there are more).
//! - A **divider** above each pane is dragged to change the ratios: the top one trades height
//!   with the price plot, a lower one with the pane above it ([`drag_stack`]); no pane gets
//!   smaller than its minimum (the price plot's is larger).
//! - The time axis stays at the bottom, under the last pane; bars line up because every pane
//!   uses the chart's viewport for x.

/// The sub-pane's share of the height when first shown.
pub const DEFAULT_RATIO: f32 = 0.25;
/// A second or later lower pane's share when first shown (the price plot keeps the most).
pub const DEFAULT_MORE_RATIO: f32 = 0.18;
/// Lower panes shown at once (more shown studies than this: the rest wait for room).
pub const MAX_PANES: usize = 4;
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

/// Pane `i`'s share when first shown.
pub fn default_ratio(i: usize) -> f32 {
    if i == 0 {
        DEFAULT_RATIO
    } else {
        DEFAULT_MORE_RATIO
    }
}

/// The price plot and up to [`MAX_PANES`] lower panes stacked under it (plot-local; fixed
/// size, no allocation).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Stack {
    pub price_h: f32,
    panes: [Split; MAX_PANES],
    n: usize,
}

impl Stack {
    /// The panes shown, top to bottom (each one's `price_h` is the price plot's).
    pub fn panes(&self) -> &[Split] {
        &self.panes[..self.n]
    }

    /// The pane whose divider is at plot-local `y`.
    pub fn divider_at(&self, y: f32) -> Option<usize> {
        self.panes().iter().position(|s| s.on_divider(y))
    }

    /// The pane plot-local `y` is in.
    pub fn pane_at(&self, y: f32) -> Option<usize> {
        self.panes().iter().position(|s| s.in_sub(y))
    }

    /// Each shown pane's share of the height as drawn (the minimums may have clamped the
    /// saved ratios): a drag starts from these, so a divider follows the pointer at once.
    pub fn ratios(&self, total: f32) -> [f32; MAX_PANES] {
        let avail = (total - self.n as f32 * DIVIDER_PX).max(1.0);
        let mut r = [0.0; MAX_PANES];
        for (k, s) in self.panes().iter().enumerate() {
            r[k] = s.sub_h / avail;
        }
        r
    }
}

/// Stack `ratios.len()` lower panes (at most [`MAX_PANES`]) under the price plot in `total` px
/// above the time axis, at a display scale (heights are whole device pixels). Panes that don't
/// fit at their minimums are left out from the bottom; `None` when not even one fits. Each
/// pane takes its ratio of the height left after the dividers, shrunk together when they
/// would leave the price plot under its minimum.
pub fn stack_at_scale(total: f32, ratios: &[f32], scale: f32) -> Option<Stack> {
    let s = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let mut n = ratios.len().min(MAX_PANES);
    while n > 0 && total - n as f32 * (DIVIDER_PX + MIN_SUB_PX) < MIN_PRICE_PX {
        n -= 1;
    }
    if n == 0 {
        return None;
    }
    let avail = total - n as f32 * DIVIDER_PX;
    let room = avail - MIN_PRICE_PX;
    let mut want = [0.0f32; MAX_PANES];
    let mut sum = 0.0;
    for k in 0..n {
        want[k] = (avail * clamp_ratio(ratios[k])).max(MIN_SUB_PX);
        sum += want[k];
    }
    // Too tall together: each gives up the same share of its height above the minimum.
    if sum > room {
        let extra: f32 = want[..n].iter().map(|w| w - MIN_SUB_PX).sum();
        let over = sum - room;
        let keep = if extra > 0.0 {
            (1.0 - over / extra).max(0.0)
        } else {
            0.0
        };
        for w in &mut want[..n] {
            *w = MIN_SUB_PX + (*w - MIN_SUB_PX) * keep;
        }
    }
    let mut hs = [0.0f32; MAX_PANES];
    let mut used = 0.0;
    for k in 0..n {
        // Whole device pixels, never past what is left for the panes below and the price.
        let left = room - used - (n - k - 1) as f32 * MIN_SUB_PX;
        hs[k] = ((want[k] * s).round() / s).clamp(MIN_SUB_PX, left.max(MIN_SUB_PX));
        used += hs[k];
    }
    let price_h = avail - used;
    let mut panes = [Split {
        price_h,
        divider_top: 0.0,
        sub_top: 0.0,
        sub_h: 0.0,
    }; MAX_PANES];
    let mut top = price_h;
    for k in 0..n {
        panes[k].divider_top = top;
        panes[k].sub_top = top + DIVIDER_PX;
        panes[k].sub_h = hs[k];
        top = panes[k].sub_top + hs[k];
    }
    Some(Stack { price_h, panes, n })
}

/// The ratios after dragging divider `k` by `dy` px (down = pane `k` smaller) from where a
/// drag started at `start` (the shown panes' shares, [`Stack::ratios`]) with `n` panes: the
/// top divider trades height with the price plot (the other panes keep theirs), a lower one
/// with the pane above it. Every pane keeps its minimum, the price plot its own.
pub fn drag_stack(
    total: f32,
    n: usize,
    start: [f32; MAX_PANES],
    k: usize,
    dy: f32,
) -> [f32; MAX_PANES] {
    let n = n.min(MAX_PANES);
    let mut out = start;
    if k >= n {
        return out;
    }
    let avail = (total - n as f32 * DIVIDER_PX).max(1.0);
    let lo = (MIN_SUB_PX / avail).max(MIN_RATIO);
    if k == 0 {
        let others: f32 = start[1..n].iter().sum();
        let hi = ((avail - MIN_PRICE_PX) / avail - others).min(MAX_RATIO);
        if lo <= hi {
            out[0] = (start[0] - dy / avail).clamp(lo, hi);
        }
    } else {
        // The pair keeps its sum: the divider moves between the two panes.
        // Neither of the two past its minimum or the ratio cap.
        let pair = start[k - 1] + start[k];
        let lo_k = lo.max(pair - MAX_RATIO);
        let hi = (pair - lo).min(MAX_RATIO);
        if lo_k <= hi {
            out[k] = (start[k] - dy / avail).clamp(lo_k, hi);
            out[k - 1] = pair - out[k];
        }
    }
    out
}

/// The panes' ratios from a chart's saved fields: `subpane` (the first, which older builds
/// write and read), then `subpanes` (all, when one below the first was moved) over it; each
/// clamped, the rest defaults.
pub fn restore(subpane: Option<f32>, subpanes: &[f32]) -> [f32; MAX_PANES] {
    let mut r: [f32; MAX_PANES] = std::array::from_fn(default_ratio);
    if let Some(x) = subpane {
        r[0] = clamp_ratio(x);
    }
    for (k, x) in subpanes.iter().take(MAX_PANES).enumerate() {
        r[k] = clamp_ratio(*x);
    }
    r
}

/// What a chart saves (the inverse of [`restore`]): `subpane` only when the first pane moved,
/// `subpanes` only when one below it did.
pub fn saved(r: &[f32; MAX_PANES]) -> (Option<f32>, Vec<f32>) {
    let first = (r[0] != DEFAULT_RATIO).then_some(r[0]);
    let more = (1..MAX_PANES).any(|k| r[k] != default_ratio(k));
    (first, if more { r.to_vec() } else { Vec::new() })
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
/// does. One pane of [`stack_at_scale`] (tests hold the two the same).
#[cfg(test)]
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
/// drag started at `start_ratio`, kept within both panes' minimums. One pane of
/// [`drag_stack`].
#[cfg(test)]
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
    /// [`Scale::fit`] for fractional values (external series), with `hi` kept at least `top`
    /// px below the pane's top (its title) when the pane leaves the lines room.
    pub fn fit_below(lo: f64, hi: f64, height: f32, top: f32) -> Scale {
        let (lo, hi) = (lo.min(hi), hi.max(lo));
        // A flat line still gets a range (2 % of its value, at least 0.02).
        let span = (hi - lo).max(hi.abs().max(1.0) * 0.02);
        let pad = span * 0.08;
        let mut s = Scale {
            lo: lo - pad,
            hi: hi + pad,
            height,
        };
        let room = height - top;
        if top > 0.0 && room >= height * 0.4 {
            // y(hi) >= top: the top value sits below the title.
            let need = (hi - s.lo) * f64::from(height) / f64::from(room) + s.lo;
            s.hi = s.hi.max(need);
        }
        s
    }

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

    /// A scale from zero at the pane's bottom edge to `hi` (padded at the top only): bars
    /// that grow from a baseline (volume) keep it on the pane's edge.
    pub fn from_zero(hi: i64, height: f32) -> Scale {
        let hi = hi.max(1) as f64;
        Scale {
            lo: 0.0,
            hi: hi * 1.08,
            height,
        }
    }

    /// [`Scale::from_zero`] with `hi` kept at least `top` px below the pane's top (a title
    /// there), when the pane is tall enough to leave the bars room.
    pub fn from_zero_below(hi: i64, height: f32, top: f32) -> Scale {
        let s = Scale::from_zero(hi, height);
        let room = height - top;
        if top <= 0.0 || room < height * 0.4 {
            return s;
        }
        let hi = hi.max(1) as f64;
        Scale {
            hi: s.hi.max(hi * f64::from(height) / f64::from(room)),
            ..s
        }
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

    /// One pane stacks exactly as the single split always did (saved charts look the same).
    #[test]
    fn one_pane_stacks_as_the_single_split() {
        for (total, ratio, scale) in [(606.0, 0.25, 1.0), (606.0, 0.0, 1.0), (600.0, 0.33, 1.5)] {
            let st = stack_at_scale(total, &[ratio], scale).unwrap();
            assert_eq!(st.panes(), &[split_at_scale(total, ratio, scale).unwrap()]);
        }
        assert_eq!(
            stack_at_scale(MIN_PRICE_PX + MIN_SUB_PX + DIVIDER_PX - 1.0, &[0.25], 1.0),
            None
        );
    }

    #[test]
    fn several_panes_stack_under_the_price_plot() {
        let st = stack_at_scale(612.0, &[0.25, 0.2], 1.0).unwrap();
        let p = st.panes();
        assert_eq!(p.len(), 2);
        // 600 px after two dividers: 150 + 120, the price plot the rest.
        assert_eq!((p[0].sub_h, p[1].sub_h), (150.0, 120.0));
        assert_eq!(st.price_h, 330.0);
        assert_eq!(p[0].divider_top, 330.0);
        assert_eq!(p[1].divider_top, p[0].sub_top + p[0].sub_h);
        assert_eq!(
            p[1].sub_top + p[1].sub_h,
            612.0,
            "the last pane ends at the axis"
        );
        assert_eq!(st.pane_at(p[1].sub_top + 1.0), Some(1));
        assert_eq!(st.divider_at(p[1].divider_top + 1.0), Some(1));
        assert_eq!(st.pane_at(10.0), None);
        // Too tall together: shrunk, the price plot keeps its minimum.
        let st = stack_at_scale(412.0, &[0.7, 0.7, 0.7], 1.0).unwrap();
        assert!(st.price_h >= MIN_PRICE_PX - 1e-3, "{}", st.price_h);
        assert!(st.panes().iter().all(|s| s.sub_h >= MIN_SUB_PX));
        // Too small for every pane: the bottom ones wait for room.
        let st = stack_at_scale(
            MIN_PRICE_PX + 2.0 * (MIN_SUB_PX + DIVIDER_PX),
            &[0.3; 4],
            1.0,
        )
        .unwrap();
        assert_eq!(st.panes().len(), 2);
        // Device pixels at 1.5: every top on one.
        let st = stack_at_scale(600.0, &[0.25, 0.18, 0.2], 1.5).unwrap();
        for s in st.panes() {
            let dev = s.sub_top * 1.5;
            assert!((dev - dev.round()).abs() < 1e-3, "{}", s.sub_top);
        }
    }

    #[test]
    fn dragging_a_divider_trades_with_its_neighbor() {
        let total = 612.0;
        let st = stack_at_scale(total, &[0.25, 0.2], 1.0).unwrap();
        let r = st.ratios(total);
        // The top divider up 60 px: the first pane grows, the second keeps its height.
        let up = drag_stack(total, 2, r, 0, -60.0);
        let after = stack_at_scale(total, &up[..2], 1.0).unwrap();
        assert_eq!(after.panes()[0].sub_h, 210.0);
        assert_eq!(after.panes()[1].sub_h, 120.0);
        assert_eq!(after.price_h, st.price_h - 60.0);
        // The divider between them down 30 px: the first grows, the second shrinks, the
        // price plot stays.
        let mid = drag_stack(total, 2, r, 1, 30.0);
        let after = stack_at_scale(total, &mid[..2], 1.0).unwrap();
        assert_eq!(after.panes()[0].sub_h, 180.0);
        assert_eq!(after.panes()[1].sub_h, 90.0);
        assert_eq!(after.price_h, st.price_h);
        // The pane above never past the cap (the divider keeps following the pointer).
        let big = drag_stack(total, 2, [0.6, 0.2, 0.0, 0.0], 1, 1_000.0);
        assert!(big[0] <= MAX_RATIO + 1e-6 && (big[0] + big[1] - 0.8).abs() < 1e-6);
        // Far: minimums hold.
        let far = drag_stack(total, 2, r, 1, 1_000.0);
        let after = stack_at_scale(total, &far[..2], 1.0).unwrap();
        assert!(after.panes()[1].sub_h >= MIN_SUB_PX);
        let far = drag_stack(total, 2, r, 0, -1_000.0);
        let after = stack_at_scale(total, &far[..2], 1.0).unwrap();
        assert!(after.price_h >= MIN_PRICE_PX - 1e-3);
        // One pane: the same as the single divider's drag.
        let one = stack_at_scale(606.0, &[0.25], 1.0).unwrap().ratios(606.0);
        assert!((drag_stack(606.0, 1, one, 0, -60.0)[0] - drag(606.0, 0.25, -60.0)).abs() < 1e-6);
    }

    #[test]
    fn a_fractional_scale_fits_and_clears_the_title() {
        let s = Scale::fit_below(-0.8, 1.2, 100.0, 0.0);
        assert!(s.lo < -0.8 && s.hi > 1.2);
        let s = Scale::fit_below(-0.8, 1.2, 100.0, 30.0);
        assert!(s.y(1.2) >= 30.0 - 1e-3, "{}", s.y(1.2));
        assert!((s.value(s.y(0.5)) - 0.5).abs() < 1e-4);
        let flat = Scale::fit_below(2.0, 2.0, 100.0, 0.0);
        assert!(flat.hi > flat.lo && flat.contains(2.0));
    }

    #[test]
    fn a_zero_based_scale_keeps_zero_on_the_bottom_edge() {
        let s = Scale::from_zero(1_000, 100.0);
        assert_eq!(s.y(0.0), 100.0);
        assert!(s.y(1_000.0) > 0.0, "padded at the top");
        assert!(Scale::from_zero(0, 100.0).hi > 0.0);
    }

    /// Saved ratios round-trip; untouched panes save nothing; a file from an older build (only
    /// `subpane`) restores the first pane; a damaged value falls back.
    #[test]
    fn ratios_save_and_restore() {
        let defaults: [f32; MAX_PANES] = std::array::from_fn(default_ratio);
        assert_eq!(saved(&defaults), (None, Vec::new()));
        assert_eq!(restore(None, &[]), defaults);
        let mut top = defaults;
        top[0] = 0.4;
        assert_eq!(
            saved(&top),
            (Some(0.4), Vec::new()),
            "the top divider: subpane only"
        );
        assert_eq!(restore(Some(0.4), &[]), top, "an older build's file");
        let mut lower = defaults;
        lower[1] = 0.3;
        let (a, b) = saved(&lower);
        assert_eq!(a, None);
        assert_eq!(restore(a, &b), lower);
        assert_eq!(restore(Some(f32::NAN), &[])[0], DEFAULT_RATIO);
        assert_eq!(restore(None, &[0.3, 99.0])[1], MAX_RATIO);
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
