//! Turns bars + viewport into flat draw lists. Runs every frame, so it must not allocate:
//! all outputs are caller-owned `Vec`s that are cleared and refilled **within their existing
//! capacity** (items beyond capacity are dropped, never reallocated). Size them once with
//! [`Scene::with_capacity`] / [`Scene::ensure_capacity`] outside the paint path.

use crate::bars::Bar;
use crate::price::{Price, PriceFormat};
use crate::time::UtcMillis;
use crate::viewport::Viewport;

/// One candle in logical pixels, already snapped to device pixels.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct Candle {
    pub x: f32,
    pub half_width: f32,
    pub wick_top: f32,
    pub wick_bottom: f32,
    pub body_top: f32,
    pub body_bottom: f32,
    pub up: bool,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct PriceGridLine {
    pub price: Price,
    pub y: f32,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TimeGridLine {
    pub time: UtcMillis,
    pub x: f32,
}

/// Reusable per-frame draw lists.
#[derive(Debug, Default)]
pub struct Scene {
    pub candles: Vec<Candle>,
    pub price_lines: Vec<PriceGridLine>,
    pub time_lines: Vec<TimeGridLine>,
}

pub const MAX_GRID_LINES: usize = 64;

impl Scene {
    pub fn with_capacity(max_candles: usize) -> Self {
        Scene {
            candles: Vec::with_capacity(max_candles),
            price_lines: Vec::with_capacity(MAX_GRID_LINES),
            time_lines: Vec::with_capacity(MAX_GRID_LINES),
        }
    }

    /// Candles that can ever be visible at `width` px: one per MIN_BAR_PX plus edges.
    pub fn candles_needed(width: f64) -> usize {
        (width / crate::viewport::MIN_BAR_PX).ceil() as usize + 4
    }

    /// Grow buffers if the plot got wider. Call on resize, outside the paint path.
    pub fn ensure_capacity(&mut self, width: f64) {
        let need = Self::candles_needed(width);
        if self.candles.capacity() < need {
            self.candles.reserve_exact(need - self.candles.len());
        }
    }

    /// Rebuild all draw lists. Never allocates.
    pub fn build(
        &mut self,
        bars: &[Bar],
        vp: &Viewport,
        fmt: &PriceFormat,
        scale_factor: f32,
        period_ms: i64,
    ) {
        build_candles(bars, vp, scale_factor, &mut self.candles);
        build_price_grid(vp, fmt, 48.0, &mut self.price_lines);
        build_time_grid(bars, vp, period_ms, 90.0, &mut self.time_lines);
    }
}

#[inline]
fn snap(v: f64, s: f32) -> f32 {
    let s = s as f64;
    ((v * s).round() / s) as f32
}

/// Fill `out` (within capacity) with the visible candles.
pub fn build_candles(bars: &[Bar], vp: &Viewport, scale_factor: f32, out: &mut Vec<Candle>) {
    out.clear();
    let s = scale_factor.max(0.5);
    let device_px = 1.0 / s;
    // Body is ~70% of the slot (leaving a gap) in whole device pixels; when bars are packed
    // tighter than 3 device pixels the body collapses to a single device pixel.
    let slot_device_px = vp.bar_px as f32 * s;
    let half_width = if slot_device_px < 3.0 {
        device_px * 0.5
    } else {
        (vp.bar_px as f32 * 0.35 * s).round().max(1.0) / s
    };
    for i in vp.visible_range(bars.len()) {
        if out.len() == out.capacity() {
            break;
        }
        let b = &bars[i];
        let x = snap(vp.index_to_x(i as f64), s);
        let (o, c) = (b.open.0 as f64, b.close.0 as f64);
        let mut body_top = snap(vp.price_to_y(o.max(c)), s);
        let mut body_bottom = snap(vp.price_to_y(o.min(c)), s);
        if body_bottom - body_top < device_px {
            // Doji: keep a visible one-device-pixel body.
            body_bottom = body_top + device_px;
        }
        if body_top > body_bottom {
            std::mem::swap(&mut body_top, &mut body_bottom);
        }
        out.push(Candle {
            x,
            half_width,
            wick_top: snap(vp.price_to_y(b.high.0 as f64), s),
            wick_bottom: snap(vp.price_to_y(b.low.0 as f64), s).max(body_bottom),
            body_top,
            body_bottom,
            up: b.close >= b.open,
        });
    }
}

/// Choose a "nice" price step: a multiple of the tick of the form {1,2,5}·10^k ticks, so
/// every gridline lands exactly on a tradable price.
pub fn price_step(vp: &Viewport, tick: i64, min_px: f64) -> i64 {
    let tick = tick.max(1);
    let px_per_unit = vp.height / (vp.price_hi - vp.price_lo).abs().max(f64::MIN_POSITIVE);
    let mut pow = 1i64;
    loop {
        for m in [1i64, 2, 5] {
            let step = tick.saturating_mul(m).saturating_mul(pow);
            if step as f64 * px_per_unit >= min_px || step == i64::MAX {
                return step;
            }
        }
        pow = pow.saturating_mul(10);
    }
}

/// Fill `out` (within capacity) with horizontal grid lines on exact tick multiples.
pub fn build_price_grid(
    vp: &Viewport,
    fmt: &PriceFormat,
    min_px: f64,
    out: &mut Vec<PriceGridLine>,
) {
    out.clear();
    let step = price_step(vp, fmt.tick, min_px);
    let lo = vp.price_lo.min(vp.price_hi);
    let hi = vp.price_lo.max(vp.price_hi);
    if !(lo.is_finite() && hi.is_finite()) {
        return;
    }
    let mut p = ((lo / step as f64).ceil() as i64).saturating_mul(step);
    while (p as f64) <= hi && out.len() < out.capacity() {
        out.push(PriceGridLine {
            price: Price(p),
            y: vp.price_to_y(p as f64) as f32,
        });
        p = p.saturating_add(step);
    }
}

/// Bar-count multiples used for time grid spacing (in units of the bar period).
const TIME_STEPS: [i64; 14] = [
    1, 2, 5, 10, 15, 30, 60, 120, 240, 360, 720, 1440, 2880, 10080,
];

/// Fill `out` (within capacity) with vertical grid lines at round times.
pub fn build_time_grid(
    bars: &[Bar],
    vp: &Viewport,
    period_ms: i64,
    min_px: f64,
    out: &mut Vec<TimeGridLine>,
) {
    out.clear();
    let bars_per_line = (min_px / vp.bar_px).ceil().max(1.0) as i64;
    let step_ms = TIME_STEPS
        .iter()
        .map(|m| m * period_ms)
        .find(|ms| *ms >= bars_per_line * period_ms)
        .unwrap_or(TIME_STEPS[TIME_STEPS.len() - 1] * period_ms);
    let mut last_x = f64::NEG_INFINITY;
    for i in vp.visible_range(bars.len()) {
        if out.len() == out.capacity() {
            break;
        }
        let t = bars[i].time;
        // First bar at or after each round boundary (handles gaps where the exact bar is missing).
        let is_boundary = t.0.rem_euclid(step_ms) == 0
            || (i > 0 && bars[i - 1].time.floor_to(step_ms) != t.floor_to(step_ms));
        let x = vp.index_to_x(i as f64);
        if is_boundary && x - last_x >= min_px * 0.75 {
            out.push(TimeGridLine {
                time: t,
                x: x as f32,
            });
            last_x = x;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bars(n: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| {
                let p = 400_000 + (i as i64 % 11) * 25;
                Bar {
                    time: UtcMillis(
                        1_700_000_000_000 - 1_700_000_000_000 % 60_000 + i as i64 * 60_000,
                    ),
                    open: Price(p),
                    high: Price(p + 100),
                    low: Price(p - 75),
                    close: Price(if i % 2 == 0 { p + 50 } else { p - 50 }),
                    volume: 1,
                }
            })
            .collect()
    }

    fn es() -> PriceFormat {
        PriceFormat::from_cqg(0.01, 2, 0.25).unwrap()
    }

    #[test]
    fn candles_respect_ohlc_ordering() {
        let b = bars(500);
        let mut vp = Viewport::default();
        vp.on_data(&b, 25);
        let mut scene = Scene::with_capacity(Scene::candles_needed(vp.width));
        scene.build(&b, &vp, &es(), 1.5, 60_000);
        assert!(!scene.candles.is_empty());
        for c in &scene.candles {
            assert!(c.wick_top <= c.body_top, "{c:?}");
            assert!(c.body_top < c.body_bottom, "{c:?}");
            assert!(c.body_bottom <= c.wick_bottom, "{c:?}");
            assert!(c.half_width > 0.0);
        }
        // Up/down follows close vs open.
        let first = vp.visible_range(b.len()).start;
        assert_eq!(scene.candles[0].up, b[first].close >= b[first].open);
    }

    #[test]
    fn price_grid_lands_on_ticks() {
        let vp = Viewport {
            price_lo: 400_013.0,
            price_hi: 401_987.0,
            ..Viewport::default()
        };
        let mut out = Vec::with_capacity(MAX_GRID_LINES);
        build_price_grid(&vp, &es(), 48.0, &mut out);
        assert!(out.len() >= 2);
        let step = out[1].price.0 - out[0].price.0;
        for l in &out {
            assert_eq!(l.price.0 % 25, 0, "gridline off-tick: {l:?}");
            assert!((l.price.0 as f64) >= vp.price_lo && (l.price.0 as f64) <= vp.price_hi);
        }
        for w in out.windows(2) {
            assert_eq!(w[1].price.0 - w[0].price.0, step);
            assert!((w[0].y - w[1].y) as f64 >= 48.0 - 1e-3);
        }
    }

    #[test]
    fn price_step_grows_with_range() {
        let mut vp = Viewport {
            price_lo: 0.0,
            price_hi: 1_000.0,
            ..Viewport::default()
        };
        let small = price_step(&vp, 25, 48.0);
        vp.price_hi = 1_000_000.0;
        let big = price_step(&vp, 25, 48.0);
        assert!(big > small);
        assert_eq!(small % 25, 0);
        assert_eq!(big % 25, 0);
    }

    #[test]
    fn time_grid_is_spaced() {
        let b = bars(2000);
        let mut vp = Viewport::default();
        vp.on_data(&b, 25);
        let mut out = Vec::with_capacity(MAX_GRID_LINES);
        build_time_grid(&b, &vp, 60_000, 90.0, &mut out);
        assert!(out.len() >= 2);
        for w in out.windows(2) {
            assert!(w[1].x - w[0].x >= 90.0 * 0.75 - 1e-3);
            assert!(w[1].time > w[0].time);
        }
    }

    #[test]
    fn outputs_never_exceed_capacity() {
        let b = bars(5000);
        let mut vp = Viewport::default();
        vp.on_data(&b, 25);
        vp.zoom_at(400.0, 1e-9, b.len()); // 1 px per bar => ~800 visible
        let mut out = Vec::with_capacity(10);
        build_candles(&b, &vp, 1.0, &mut out);
        assert_eq!(out.len(), 10);
        assert_eq!(out.capacity(), 10);
    }
}
