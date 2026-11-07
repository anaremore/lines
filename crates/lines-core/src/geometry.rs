//! Turns bars + viewport into flat draw lists, and the pointer into a snapped crosshair.
//! Runs every frame, so it must not allocate: all outputs are caller-owned `Vec`s that are
//! cleared and refilled **within their existing capacity** (items beyond capacity are dropped,
//! never reallocated). Size them once with [`Scene::with_capacity`] /
//! [`Scene::ensure_capacity`] outside the paint path.
//!
//! Device-pixel conventions (so 1-device-pixel lines are crisp at any DPI scale, including
//! fractional ones like 1.5): horizontal lines and box edges sit on device-pixel
//! *boundaries* ([`snap`]); a line drawn there covers `[y, y + 1 device px)`. Vertical
//! centre lines (wicks, time gridlines, the crosshair) sit on device-pixel *centres*
//! ([`snap_center`]); a line drawn there covers `[x - ½, x + ½)` device px. Candle bodies are
//! an odd number of device pixels wide so the wick runs exactly through the middle.

pub mod order_tags;

use crate::bars::Bar;
use crate::price::{DisplayStyle, Price, PriceFormat};
use crate::qty::QtyFormat;
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
    /// What the label shows (the date only on a new day).
    pub label: TimeLabel,
}

/// Reusable per-frame draw lists.
#[derive(Debug, Default)]
pub struct Scene {
    pub candles: Vec<Candle>,
    pub price_lines: Vec<PriceGridLine>,
    pub time_lines: Vec<TimeGridLine>,
    /// Minimum label spacing on both axes (set by the app from its font).
    pub space: GridSpace,
}

pub const MAX_GRID_LINES: usize = 64;

impl Scene {
    pub fn with_capacity(max_candles: usize) -> Self {
        Scene {
            candles: Vec::with_capacity(max_candles),
            price_lines: Vec::with_capacity(MAX_GRID_LINES),
            time_lines: Vec::with_capacity(MAX_GRID_LINES),
            space: GridSpace::default(),
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
        build_price_grid(
            vp,
            fmt,
            scale_factor,
            self.space.price_px,
            &mut self.price_lines,
        );
        build_time_grid(
            bars,
            vp,
            period_ms,
            scale_factor,
            &self.space,
            &mut self.time_lines,
        );
    }
}

/// Clamp a DPI scale to something usable (GPUI never reports < 1, but be defensive).
#[inline]
fn dpi(scale_factor: f32) -> f64 {
    if scale_factor.is_finite() {
        (scale_factor as f64).max(0.5)
    } else {
        1.0
    }
}

/// Nearest device-pixel boundary, in logical px.
#[inline]
pub fn snap(v: f64, scale_factor: f32) -> f32 {
    let s = dpi(scale_factor);
    ((v * s).round() / s) as f32
}

/// Centre of the device pixel containing `v`, in logical px.
#[inline]
pub fn snap_center(v: f64, scale_factor: f32) -> f32 {
    let s = dpi(scale_factor);
    (((v * s).floor() + 0.5) / s) as f32
}

/// Screen x of bar slot `index`'s centre line (shared by wicks, time grid and crosshair so
/// they coincide exactly).
#[inline]
pub fn slot_x(vp: &Viewport, index: f64, scale_factor: f32) -> f32 {
    snap_center(vp.index_to_x(index), scale_factor)
}

/// Half a candle body's width at `bar_px` per bar: the body is ~70% of the slot (leaving a
/// gap), an odd number of device pixels wide so it is centred on the one-device-pixel wick;
/// when bars are packed tighter than 3 device pixels it collapses to the wick's single device
/// pixel. Shared by every candle drawn on a chart's time axis (price, sub-panes).
pub fn candle_half_width(bar_px: f64, scale_factor: f32) -> f32 {
    let s = dpi(scale_factor) as f32;
    if bar_px as f32 * s < 3.0 {
        0.5 / s
    } else {
        let side = (bar_px as f32 * 0.35 * s - 0.5).round().max(1.0);
        (side + 0.5) / s
    }
}

/// Fill `out` (within capacity) with the visible candles.
pub fn build_candles(bars: &[Bar], vp: &Viewport, scale_factor: f32, out: &mut Vec<Candle>) {
    out.clear();
    let s = dpi(scale_factor) as f32;
    let half_width = candle_half_width(vp.bar_px, scale_factor);
    let device_px = 1.0 / s;
    for i in vp.visible_range(bars.len()) {
        if out.len() == out.capacity() {
            break;
        }
        let b = &bars[i];
        let x = slot_x(vp, i as f64, s);
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

/// How far apart axis labels must be, in logical px (uPlot's "space"): each axis takes the
/// smallest step from its fixed table whose labels clear it. The app measures its label
/// font; the defaults suit a 12 px UI font.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GridSpace {
    /// Price axis: label height plus a gap (labels are stacked vertically).
    pub price_px: f64,
    /// Time axis: the width of an `HH:MM` label plus a gap.
    pub time_px: f64,
    /// Time axis: the width of a `MM-DD HH:MM` label (a new day mid-step) plus a gap.
    pub date_time_px: f64,
}

impl Default for GridSpace {
    fn default() -> Self {
        GridSpace {
            price_px: 36.0,
            time_px: 56.0,
            date_time_px: 96.0,
        }
    }
}

/// Price step for gridlines: the smallest entry of a fixed table whose lines are at least
/// `min_px` apart. Every step is a multiple of the tick, so each line is a tradable price.
///
/// The table is "nice" in the price's own notation: decimal prices step through
/// {1, 2, 2.5, 5}·10^k (ES: 0.25 0.5 1 2 2.5 5 10 …, entries that aren't tick multiples are
/// skipped); fractional prices (32nds) halve a point (1/64 … 1/2) before whole points. If
/// nothing in the table fits (an odd tick), the step is {1, 2, 5}·10^k ticks.
pub fn price_step(vp: &Viewport, fmt: &PriceFormat, min_px: f64) -> i64 {
    let tick = fmt.tick.max(1);
    let px_per_unit = vp.height / (vp.price_hi - vp.price_lo).abs().max(f64::MIN_POSITIVE);
    let fits = |units: i64| units as f64 * px_per_unit >= min_px;
    // A price `n / d` (display units) in price units, if whole and a tick multiple.
    let (num, den) = (fmt.scale.num() as i128, fmt.scale.den() as i128);
    let units = |n: i128, d: i128| -> Option<i64> {
        let (a, b) = (n * den, d * num);
        (a % b == 0)
            .then(|| a / b)
            .and_then(|u| i64::try_from(u).ok())
            .filter(|u| *u > 0 && u % tick == 0)
    };
    let binary = !matches!(fmt.style, DisplayStyle::Decimal { .. });
    if binary {
        // 1/4096 … 1/2 of a point, then whole points below.
        for k in (1..=12).rev() {
            if let Some(u) = units(1, 1 << k)
                && fits(u)
            {
                return u;
            }
        }
    }
    // {1, 2, 2.5, 5} x 10^e, e from -18 (fractional prices: from whole points) to 18.
    for e in if binary { 0 } else { -18 }..=18i32 {
        for (m, d) in [(10i128, 10i128), (20, 10), (25, 10), (50, 10)] {
            let (n, d) = if e >= 0 {
                (m * 10i128.pow(e as u32), d)
            } else {
                (m, d * 10i128.pow(e.unsigned_abs()))
            };
            if let Some(u) = units(n, d)
                && fits(u)
            {
                return u;
            }
        }
    }
    let mut pow = 1i64;
    loop {
        for m in [1i64, 2, 5] {
            let step = tick.saturating_mul(m).saturating_mul(pow);
            if fits(step) || step == i64::MAX {
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
    scale_factor: f32,
    min_px: f64,
    out: &mut Vec<PriceGridLine>,
) {
    out.clear();
    let step = price_step(vp, fmt, min_px);
    let lo = vp.price_lo.min(vp.price_hi);
    let hi = vp.price_lo.max(vp.price_hi);
    if !(lo.is_finite() && hi.is_finite()) {
        return;
    }
    let mut p = ((lo / step as f64).ceil() as i64).saturating_mul(step);
    while (p as f64) <= hi && out.len() < out.capacity() {
        out.push(PriceGridLine {
            price: Price(p),
            y: snap(vp.price_to_y(p as f64), scale_factor),
        });
        p = p.saturating_add(step);
    }
}

const SEC: i64 = 1_000;
const MIN: i64 = 60 * SEC;
const HOUR: i64 = 60 * MIN;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;

/// Time steps for the time axis (uPlot's table, to a week). Entries that aren't whole bar
/// periods are skipped; past the table, whole weeks.
const TIME_STEPS: [i64; 26] = [
    SEC,
    2 * SEC,
    5 * SEC,
    10 * SEC,
    15 * SEC,
    30 * SEC,
    MIN,
    2 * MIN,
    5 * MIN,
    10 * MIN,
    15 * MIN,
    30 * MIN,
    HOUR,
    2 * HOUR,
    3 * HOUR,
    4 * HOUR,
    6 * HOUR,
    8 * HOUR,
    12 * HOUR,
    DAY,
    2 * DAY,
    3 * DAY,
    WEEK,
    2 * WEEK,
    4 * WEEK,
    8 * WEEK,
];

/// What a time gridline's label shows.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TimeLabel {
    /// `HH:MM`: same day as the line before it.
    Time,
    /// `HH:MM:SS`: the same, on a step under a minute (seconds bars, fast tick bars).
    TimeSeconds,
    /// `MM-DD`: the first line of a new day, on midnight (or every line of a step of a day
    /// or more).
    Date,
    /// `MM-DD HH:MM`: the first line of a new day, not on midnight.
    DateTime,
}

impl TimeLabel {
    /// Write the label for `t` (UTC, as the axis), without allocating.
    pub fn write<W: std::fmt::Write>(self, t: UtcMillis, out: &mut W) -> std::fmt::Result {
        let (_, mo, d, h, mi, sec) = t.to_civil();
        match self {
            TimeLabel::Time => write!(out, "{h:02}:{mi:02}"),
            TimeLabel::TimeSeconds => write!(out, "{h:02}:{mi:02}:{sec:02}"),
            TimeLabel::Date => write!(out, "{mo:02}-{d:02}"),
            TimeLabel::DateTime => write!(out, "{mo:02}-{d:02} {h:02}:{mi:02}"),
        }
    }
}

/// The time step for gridlines: the smallest table entry that is a whole number of bar
/// periods and puts `HH:MM` labels at least `min_px` apart.
pub fn time_step(vp: &Viewport, period_ms: i64, min_px: f64) -> i64 {
    let period = period_ms.max(1);
    let px_per_ms = vp.bar_px / period as f64;
    // Under a minute, labels carry seconds (`HH:MM:SS`): they need about half as much room
    // again.
    let fits = |ms: i64| ms as f64 * px_per_ms >= if ms < MIN { min_px * 1.5 } else { min_px };
    if let Some(s) = TIME_STEPS
        .iter()
        .copied()
        .find(|s| *s % period == 0 && *s >= period && fits(*s))
    {
        return s;
    }
    let mut s = (WEEK / period).max(1) * period;
    while !fits(s) && s < i64::MAX / 2 {
        s *= 2;
    }
    s
}

/// Start of the step-`step_ms` bucket holding `t`. Weeks start on Monday 00:00 UTC.
fn time_bucket(t: UtcMillis, step_ms: i64) -> i64 {
    if step_ms >= WEEK && step_ms % WEEK == 0 {
        // 1970-01-01 was a Thursday: the first Monday is 4 days later.
        let off = 4 * DAY;
        (t.0 - off).div_euclid(step_ms) * step_ms + off
    } else {
        t.0.div_euclid(step_ms) * step_ms
    }
}

/// Fill `out` (within capacity) with vertical grid lines at round times: the first bar of
/// each step bucket (bars missing in a gap move the line to the next bar), where its label
/// clears the previous one. The first line of the view and the first of each new UTC day
/// carry the date ([`TimeLabel`]); the rest show only the time.
pub fn build_time_grid(
    bars: &[Bar],
    vp: &Viewport,
    period_ms: i64,
    scale_factor: f32,
    space: &GridSpace,
    out: &mut Vec<TimeGridLine>,
) {
    out.clear();
    let step = time_step(vp, period_ms, space.time_px);
    let mut last: Option<(f64, f64, i64)> = None; // (x, label space, day)
    for i in vp.visible_range(bars.len()) {
        if out.len() == out.capacity() {
            break;
        }
        let t = bars[i].time;
        let is_boundary = t.0 - time_bucket(t, step) == 0
            || (i > 0 && time_bucket(bars[i - 1].time, step) != time_bucket(t, step));
        if !is_boundary {
            continue;
        }
        let x = vp.index_to_x(i as f64);
        if let Some((lx, room, _)) = last
            && x - lx < room
        {
            continue;
        }
        let day = t.0.div_euclid(DAY);
        let label = if last.is_some_and(|(_, _, d)| d == day) {
            if step < MIN {
                TimeLabel::TimeSeconds
            } else {
                TimeLabel::Time
            }
        } else if step >= DAY || t.0.rem_euclid(DAY) == 0 {
            TimeLabel::Date
        } else {
            TimeLabel::DateTime
        };
        let room = match label {
            TimeLabel::DateTime => space.date_time_px,
            TimeLabel::TimeSeconds => space.time_px * 1.5,
            _ => space.time_px,
        };
        out.push(TimeGridLine {
            time: t,
            x: slot_x(vp, i as f64, scale_factor),
            label,
        });
        last = Some((x, room, day));
    }
}

// ---- crosshair -----------------------------------------------------------------------

/// What the crosshair shows for one pointer position. All pixel values are logical px
/// relative to the plot's top-left corner, already device-pixel snapped (see module docs).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Crosshair {
    /// Bar slot under the pointer. Can be negative (left of the first bar) or `>= len`
    /// (right of the newest bar, in the empty right margin).
    pub slot: i64,
    /// Index of the bar in that slot, if there is one.
    pub bar: Option<usize>,
    /// The slot's time: the bar's own time, or extrapolated by whole periods beyond either
    /// end of the series. `None` when there are no bars.
    pub time: Option<UtcMillis>,
    /// Price under the pointer, snapped to the instrument tick and kept on screen.
    pub price: Price,
    /// Vertical line: the slot's centre (a device-pixel centre, same as the wick).
    pub x: f32,
    /// Horizontal line: `price`'s y (a device-pixel boundary).
    pub y: f32,
}

/// Bar slot whose centre is nearest to plot-local `x`. Slot `i` spans
/// `[index_to_x(i) - bar_px/2, index_to_x(i) + bar_px/2)`.
#[inline]
pub fn hit_slot(vp: &Viewport, x: f64) -> i64 {
    // `as` saturates, so absurd inputs give i64::MIN/MAX rather than UB or a panic.
    (vp.x_to_index(x) + 0.5).floor() as i64
}

/// Price under plot-local `y`, rounded to the nearest multiple of `tick`. If that tick lies
/// just outside the visible range (pointer at the very top/bottom edge), the neighbouring
/// tick inside the range is used instead when there is one, so the line stays on screen.
pub fn snap_price(vp: &Viewport, y: f64, tick: i64) -> Option<Price> {
    let tick = tick.max(1);
    let raw = vp.y_to_price(y);
    let (lo, hi) = (vp.price_lo.min(vp.price_hi), vp.price_lo.max(vp.price_hi));
    if !(raw.is_finite() && lo.is_finite() && hi.is_finite()) {
        return None;
    }
    let tf = tick as f64;
    let mut n = (raw / tf).round();
    if n * tf > hi && (n - 1.0) * tf >= lo {
        n -= 1.0;
    } else if n * tf < lo && (n + 1.0) * tf <= hi {
        n += 1.0;
    }
    if n.abs() >= 9.0e18 {
        return None;
    }
    (n as i64).checked_mul(tick).map(Price)
}

/// Time of bar slot `slot`: the bar's time inside the series, else extrapolated from the
/// nearest end by whole periods (bars beyond the data don't exist yet / any more).
pub fn slot_time(bars: &[Bar], slot: i64, period_ms: i64) -> Option<UtcMillis> {
    let (first, last) = (bars.first()?, bars.last()?);
    let last_index = (bars.len() - 1) as i64;
    let t = if slot < 0 {
        first.time.0.saturating_add(slot.saturating_mul(period_ms))
    } else if slot > last_index {
        last.time
            .0
            .saturating_add((slot - last_index).saturating_mul(period_ms))
    } else {
        bars[slot as usize].time.0
    };
    Some(UtcMillis(t))
}

/// The bar slot whose period contains `t`: the inverse of [`slot_time`], for marking another
/// chart's crosshair time (crosshair sync). Inside the series it is the bar with
/// `time <= t < time + period`; beyond either end, whole periods from the nearest bar. `None`
/// when there are no bars, or `t` falls between two bars (no bar covers that time here: the
/// marker shows as missing rather than on a neighbour).
pub fn time_slot(bars: &[Bar], t: UtcMillis, period_ms: i64) -> Option<i64> {
    let (first, last) = (bars.first()?, bars.last()?);
    let p = period_ms.max(1);
    if t.0 < first.time.0 {
        return Some((t.0 - first.time.0).div_euclid(p));
    }
    let last_index = (bars.len() - 1) as i64;
    if t.0 >= last.time.0 {
        return Some(last_index + (t.0 - last.time.0) / p);
    }
    let i = bars.partition_point(|b| b.time.0 <= t.0) - 1;
    (t.0 < bars[i].time.0 + p).then_some(i as i64)
}

/// How the crosshair's price follows the pointer.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum CrosshairMode {
    /// The tick nearest the pointer.
    #[default]
    Normal,
    /// Magnet: over a bar, its open, high, low or close nearest the pointer (off the bars,
    /// as `Normal`).
    Magnet,
}

/// Of `bar`'s open, high, low and close, the one nearest `price` (ties: in that order).
pub fn magnet_price(bar: &Bar, price: f64) -> Price {
    let mut best = bar.open;
    for p in [bar.high, bar.low, bar.close] {
        if (p.0 as f64 - price).abs() < (best.0 as f64 - price).abs() {
            best = p;
        }
    }
    best
}

/// Hit-test the pointer at plot-local logical (`x`, `y`). `None` when the pointer is outside
/// the plot `[0, width) × [0, height)` or not a finite position. Pure and allocation-free.
#[allow(clippy::too_many_arguments)]
pub fn crosshair(
    bars: &[Bar],
    vp: &Viewport,
    tick: i64,
    period_ms: i64,
    scale_factor: f32,
    mode: CrosshairMode,
    x: f64,
    y: f64,
) -> Option<Crosshair> {
    if !(x >= 0.0 && x < vp.width && y >= 0.0 && y < vp.height) {
        return None; // also rejects NaN
    }
    let slot = hit_slot(vp, x);
    let bar = usize::try_from(slot).ok().filter(|&i| i < bars.len());
    let price = match (mode, bar) {
        (CrosshairMode::Magnet, Some(i)) => magnet_price(&bars[i], vp.y_to_price(y)),
        _ => snap_price(vp, y, tick)?,
    };
    Some(Crosshair {
        slot,
        bar,
        time: slot_time(bars, slot, period_ms),
        price,
        x: slot_x(vp, slot as f64, scale_factor),
        y: snap(vp.price_to_y(price.0 as f64), scale_factor),
    })
}

/// OHLCV readout for one bar, e.g. `O 4925.25  H 4926.00  L 4924.75  C 4925.50  V 12`.
/// Prices use the instrument's display format; volume its quantity format. No allocation.
pub fn write_ohlcv<W: std::fmt::Write>(
    bar: &Bar,
    price: &PriceFormat,
    qty: QtyFormat,
    out: &mut W,
) -> std::fmt::Result {
    for (tag, p) in [
        ("O ", bar.open),
        ("  H ", bar.high),
        ("  L ", bar.low),
        ("  C ", bar.close),
    ] {
        out.write_str(tag)?;
        price.write(p, out)?;
    }
    out.write_str("  V ")?;
    qty.write(bar.volume, out)
}

/// A vertical span on an axis, `[top, bottom)` in logical px: a label's or a tag's box.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Span {
    pub top: f32,
    pub bottom: f32,
}

impl Span {
    /// The box `half` above and below `centre`.
    pub fn around(centre: f32, half: f32) -> Span {
        Span {
            top: centre - half,
            bottom: centre + half,
        }
    }

    /// The boxes share some height (touching edges don't count).
    pub fn overlaps(self, o: Span) -> bool {
        self.top < o.bottom && o.top < self.bottom
    }
}

/// An axis label is drawn only where no tag (last price, crosshair price) covers any of it:
/// a half-hidden number under a tag reads as a different price. Allocation-free.
pub fn label_clear_of_tags(label: Span, tags: &[Option<Span>]) -> bool {
    tags.iter().flatten().all(|t| !label.overlaps(*t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DisplayStyle, PriceScale, Qty};

    #[test]
    fn axis_labels_under_a_tag_are_hidden() {
        // Label boxes are 15.6 px tall (12 px text), tags 18 px; labels every 20 px.
        let label = |y: f32| Span::around(y, 7.8);
        let last = Some(Span::around(103.0, 9.0)); // 94..112
        let cross = Some(Span::around(160.0, 9.0)); // 151..169
        let tags = [last, cross];
        // The owner's case: "23885.50" tag just above "23885.00", partly covering it.
        assert!(!label_clear_of_tags(label(110.0), &tags));
        assert!(!label_clear_of_tags(label(100.0), &tags));
        assert!(!label_clear_of_tags(label(155.0), &tags));
        // Clear of both, including a label that only touches a tag's edge.
        assert!(label_clear_of_tags(label(80.0), &tags));
        assert!(
            label_clear_of_tags(label(120.0), &tags),
            "starts just below the tag"
        );
        assert!(label_clear_of_tags(
            Span {
                top: 112.0,
                bottom: 127.6
            },
            &tags
        ));
        assert!(label_clear_of_tags(label(200.0), &tags));
        // No tags: every label shows.
        assert!(label_clear_of_tags(label(110.0), &[None, None]));
    }

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
                    volume: Qty(1),
                }
            })
            .collect()
    }

    fn es() -> PriceFormat {
        PriceFormat::new(
            PriceScale::decimal(2),
            DisplayStyle::Decimal { digits: 2 },
            25,
        )
    }

    /// Every candle on a chart's time axis (price, and sub-panes through the same function)
    /// has a body an odd number of device pixels wide, so the one-device-pixel wick is centred.
    #[test]
    fn candle_bodies_are_an_odd_number_of_device_pixels() {
        for dpi in [1.0f32, 1.25, 1.5, 2.0] {
            for bar_px in [1.0, 2.0, 3.0, 4.9, 7.0, 12.5, 40.0, 80.0] {
                let w = candle_half_width(bar_px, dpi) * 2.0 * dpi;
                assert!((w - w.round()).abs() < 1e-4, "{dpi} {bar_px}: {w}");
                assert_eq!(w.round() as i64 % 2, 1, "{dpi} {bar_px}: {w}");
            }
        }
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
        build_price_grid(&vp, &es(), 1.0, 48.0, &mut out);
        assert!(out.len() >= 2);
        let step = out[1].price.0 - out[0].price.0;
        for l in &out {
            assert_eq!(l.price.0 % 25, 0, "gridline off-tick: {l:?}");
            assert!((l.price.0 as f64) >= vp.price_lo && (l.price.0 as f64) <= vp.price_hi);
        }
        for w in out.windows(2) {
            assert_eq!(w[1].price.0 - w[0].price.0, step);
            // Snapping to device pixels moves each line by at most half a device pixel.
            assert!((w[0].y - w[1].y) as f64 >= 48.0 - 1.0);
        }
    }

    #[test]
    fn price_step_grows_with_range() {
        let mut vp = Viewport {
            price_lo: 0.0,
            price_hi: 1_000.0,
            ..Viewport::default()
        };
        let small = price_step(&vp, &es(), 48.0);
        vp.price_hi = 1_000_000.0;
        let big = price_step(&vp, &es(), 48.0);
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
        let space = GridSpace::default();
        build_time_grid(&b, &vp, 60_000, 1.0, &space, &mut out);
        assert!(out.len() >= 2);
        for w in out.windows(2) {
            let room = match w[0].label {
                TimeLabel::DateTime => space.date_time_px,
                _ => space.time_px,
            };
            // Device-pixel snapping moves each line by at most half a device pixel.
            assert!((w[1].x - w[0].x) as f64 >= room - 1.0, "{w:?}");
            assert!(w[1].time > w[0].time);
        }
    }

    /// Plot of `height` px over `span` price units starting at 400_000.
    fn price_vp(span: f64) -> Viewport {
        Viewport {
            height: 500.0,
            price_lo: 400_000.0,
            price_hi: 400_000.0 + span,
            ..Viewport::default()
        }
    }

    #[test]
    fn price_steps_come_from_the_nice_table_and_are_tick_multiples() {
        // ES: tick 0.25 (25 units of 0.01). Steps 0.25 0.5 1 2 2.5 5 10 20 25 50 …; 0.1, 0.2
        // and 1.25-style steps never appear.
        let es = es();
        let mut seen = Vec::new();
        for k in 0..60 {
            let span = 200.0 * 1.25f64.powi(k);
            let step = price_step(&price_vp(span), &es, 36.0);
            assert_eq!(step % 25, 0, "{step}");
            // The smallest table entry that clears 36 px.
            assert!(step as f64 * 500.0 / span >= 36.0);
            if seen.last() != Some(&step) {
                seen.push(step);
            }
        }
        assert_eq!(
            &seen[..10],
            &[25, 50, 100, 200, 250, 500, 1000, 2000, 2500, 5000]
        );
        // Each is the smallest that fits: the previous table entry would be too tight.
        let step = price_step(&price_vp(10_000.0), &es, 36.0); // 0.05 px per unit
        assert_eq!(step, 1000, "720 px > 36 px at 10.00; 5.00 gives 25 px");
    }

    #[test]
    fn fractional_prices_step_in_halves_of_a_point() {
        // ZN: unit 1/128 point, tick 1/64 (2 units). Steps: 1/64 1/32 1/16 1/8 1/4 1/2 then
        // 1 2 2.5 5 … points.
        let zn = PriceFormat::new(
            PriceScale::new(1, 128),
            DisplayStyle::SubFraction {
                denominator: 32,
                sub: 4,
            },
            2,
        );
        let mut seen = Vec::new();
        for k in 0..40 {
            let span = 10.0 * 1.3f64.powi(k);
            let step = price_step(&price_vp(span), &zn, 36.0);
            assert_eq!(step % 2, 0);
            if seen.last() != Some(&step) {
                seen.push(step);
            }
        }
        assert_eq!(&seen[..9], &[2, 4, 8, 16, 32, 64, 128, 256, 320]);
    }

    #[test]
    fn odd_ticks_fall_back_to_tick_multiples() {
        // A tick of 3 units divides no nice decimal below 30 (and 30 = 3 x 10 is in the
        // table as 0.30): every step is still a tick multiple.
        let f = PriceFormat::new(
            PriceScale::decimal(2),
            DisplayStyle::Decimal { digits: 2 },
            3,
        );
        for k in 0..40 {
            let step = price_step(&price_vp(50.0 * 1.4f64.powi(k)), &f, 36.0);
            assert_eq!(step % 3, 0, "{step}");
        }
    }

    #[test]
    fn time_steps_are_whole_periods_from_the_table() {
        let mut vp = Viewport::default();
        for bar_px in [80.0, 20.0, 8.0, 3.0, 1.0] {
            vp.bar_px = bar_px;
            for period in [1_000, 5_000, 60_000, 300_000, 3_600_000, DAY] {
                let s = time_step(&vp, period, 56.0);
                assert_eq!(s % period, 0, "{s} {period}");
                assert!(s as f64 / period as f64 * bar_px >= 56.0);
            }
        }
        // One-minute bars at 8 px: 7 bars per label; 10 minutes is the first that fits.
        vp.bar_px = 8.0;
        assert_eq!(time_step(&vp, 60_000, 56.0), 10 * MIN);
        // 5-minute bars at 8 px: 30 minutes is 48 px, so an hour; steps under a bar skipped.
        assert_eq!(time_step(&vp, 300_000, 56.0), HOUR);
        // Beyond the table: whole weeks, doubling.
        vp.bar_px = 1.0;
        assert_eq!(time_step(&vp, DAY, 56.0), 8 * WEEK);
    }

    /// Seconds bars (roadmap 9b): steps under a minute label to the second and get room for
    /// it; whole minutes stay `HH:MM`.
    #[test]
    fn sub_minute_steps_label_seconds() {
        let mut vp = Viewport {
            bar_px: 60.0,
            ..Viewport::default()
        };
        // 30 s bars at 60 px: 30 s would leave 60 px, short of 1.5 × 56: a minute.
        assert_eq!(time_step(&vp, 30_000, 56.0), MIN);
        vp.bar_px = 90.0;
        assert_eq!(time_step(&vp, 30_000, 56.0), 30 * SEC);
        let mut s = String::new();
        let t = UtcMillis::parse_iso("2026-11-05T18:53:30Z").unwrap();
        TimeLabel::TimeSeconds.write(t, &mut s).unwrap();
        assert_eq!(s, "18:53:30");
        s.clear();
        // A step of a minute or more keeps `HH:MM`, even for a bar starting off the minute
        // (tick, volume and range bars start at a trade).
        TimeLabel::Time.write(t, &mut s).unwrap();
        assert_eq!(s, "18:53");
        // The grid picks the seconds label only under a minute.
        let b: Vec<Bar> = (0..40)
            .map(|i| Bar {
                time: UtcMillis(t.0 + i * 30_000),
                open: Price(1),
                high: Price(1),
                low: Price(1),
                close: Price(1),
                volume: Qty(1),
            })
            .collect();
        let mut vp = Viewport::default();
        vp.set_size(1200.0, 400.0);
        vp.on_data(&b, 90);
        vp.bar_px = 90.0;
        let mut out = Vec::with_capacity(MAX_GRID_LINES);
        build_time_grid(&b, &vp, 30_000, 1.0, &GridSpace::default(), &mut out);
        assert!(
            out.iter()
                .skip(1)
                .all(|l| l.label == TimeLabel::TimeSeconds),
            "{out:?}"
        );
    }

    /// One-minute bars from `start` (UTC ms), `n` of them, skipping minutes in `gaps`.
    fn minute_bars(start: i64, n: usize) -> Vec<Bar> {
        (0..n)
            .map(|i| Bar {
                time: UtcMillis(start + i as i64 * MIN),
                open: Price(400_000),
                high: Price(400_100),
                low: Price(399_900),
                close: Price(400_050),
                volume: Qty(1),
            })
            .collect()
    }

    #[test]
    fn the_date_shows_only_on_the_first_line_of_a_new_day() {
        // 2026-11-05 22:00Z to 2026-11-06 02:00Z, one-minute bars, all on screen.
        let start = crate::time::days_from_civil(2026, 11, 5) * DAY + 22 * HOUR; // 2026-11-05T22:00Z
        assert_eq!(start.rem_euclid(DAY), 22 * HOUR);
        let b = minute_bars(start, 240);
        let mut vp = Viewport {
            width: 1200.0,
            bar_px: 5.0,
            ..Viewport::default()
        };
        vp.on_data(&b, 25);
        let mut out = Vec::with_capacity(MAX_GRID_LINES);
        build_time_grid(&b, &vp, MIN, 1.0, &GridSpace::default(), &mut out);
        let labels: Vec<(String, TimeLabel)> = out
            .iter()
            .map(|l| {
                let mut s = String::new();
                l.label.write(l.time, &mut s).unwrap();
                (s, l.label)
            })
            .collect();
        // First line of the view: the date with its time; midnight: the date alone; every
        // other line the time only.
        assert_eq!(labels[0].1, TimeLabel::DateTime, "{labels:?}");
        assert!(labels[0].0.starts_with("11-05 "), "{labels:?}");
        let dates: Vec<&str> = labels
            .iter()
            .filter(|(_, k)| *k != TimeLabel::Time)
            .map(|(s, _)| s.as_str())
            .collect();
        assert_eq!(dates, [labels[0].0.as_str(), "11-06"], "{labels:?}");
        assert!(labels.iter().any(|(s, _)| s == "23:00"));
        assert!(labels.iter().any(|(s, _)| s == "01:00"));
        // Daily steps: every line is a date.
        vp.bar_px = 1.0;
        let b = minute_bars(start, 6 * 1440);
        vp.on_data(&b, 25);
        build_time_grid(&b, &vp, MIN, 1.0, &GridSpace::default(), &mut out);
        assert!(
            out.iter()
                .all(|l| l.label != TimeLabel::DateTime || l == &out[0])
        );
    }

    #[test]
    fn week_lines_start_on_monday() {
        // Monday 2026-11-02 00:00Z.
        let monday = crate::time::days_from_civil(2026, 11, 2) * DAY;
        assert_eq!(UtcMillis(monday).weekday(), 1);
        assert_eq!(time_bucket(UtcMillis(monday + 5 * DAY), WEEK), monday);
        assert_eq!(time_bucket(UtcMillis(monday - 1), WEEK), monday - WEEK);
    }

    #[test]
    fn magnet_snaps_to_the_nearest_of_ohlc() {
        let b = Bar {
            time: UtcMillis(0),
            open: Price(400_000),
            high: Price(400_500),
            low: Price(399_000),
            close: Price(400_250),
            volume: Qty(1),
        };
        assert_eq!(magnet_price(&b, 400_600.0), Price(400_500));
        assert_eq!(magnet_price(&b, 400_130.0), Price(400_250));
        assert_eq!(magnet_price(&b, 400_100.0), Price(400_000));
        assert_eq!(magnet_price(&b, 300_000.0), Price(399_000));
        // Through the hit-test: over a bar the price is one of its OHLC; off the bars the
        // pointer's tick.
        let bars = bars(100);
        let vp = plot();
        for y in [5.0, 120.0, 260.0, 499.0] {
            let c =
                crosshair(&bars, &vp, 25, 60_000, 1.0, CrosshairMode::Magnet, 500.0, y).unwrap();
            let bar = bars[c.bar.unwrap()];
            assert!([bar.open, bar.high, bar.low, bar.close].contains(&c.price));
            assert_eq!(c.y, snap(vp.price_to_y(c.price.0 as f64), 1.0));
        }
        let off = crosshair(
            &bars[..90],
            &vp,
            25,
            60_000,
            1.0,
            CrosshairMode::Magnet,
            995.0,
            250.0,
        )
        .unwrap();
        assert_eq!(off.bar, None);
        assert_eq!(off.price, snap_price(&vp, 250.0, 25).unwrap());
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

    // ---- crosshair ---------------------------------------------------------------

    const DPI: [f32; 5] = [1.0, 1.25, 1.5, 1.75, 2.0];

    /// 1000 x 500 plot, 10 px per bar, bar 99 centred on the right edge, prices 400_000..402_000.
    fn plot() -> Viewport {
        Viewport {
            width: 1000.0,
            height: 500.0,
            right_index: 99.0,
            bar_px: 10.0,
            price_lo: 400_000.0,
            price_hi: 402_000.0,
            auto_price: false,
            follow_live: false,
            ..Viewport::default()
        }
    }

    fn on_device_grid(v: f32, s: f32) -> bool {
        let d = v as f64 * s as f64;
        (d - d.round()).abs() < 1e-3
    }

    fn on_device_centre(v: f32, s: f32) -> bool {
        let d = v as f64 * s as f64 - 0.5;
        (d - d.round()).abs() < 1e-3
    }

    #[test]
    fn hit_slot_picks_nearest_centre() {
        let vp = plot();
        // Bar i is centred at x = 10 + 10 i; its slot is [5 + 10 i, 15 + 10 i).
        assert_eq!(hit_slot(&vp, 10.0), 0);
        assert_eq!(hit_slot(&vp, 14.99), 0);
        assert_eq!(hit_slot(&vp, 15.0), 1);
        assert_eq!(hit_slot(&vp, 5.0), 0);
        assert_eq!(hit_slot(&vp, 4.99), -1);
        assert_eq!(hit_slot(&vp, 0.0), -1);
        assert_eq!(hit_slot(&vp, 999.0), 99);
        for i in 0..100 {
            let x = vp.index_to_x(i as f64);
            assert_eq!(hit_slot(&vp, x), i);
            assert_eq!(hit_slot(&vp, x - 4.9), i);
            assert_eq!(hit_slot(&vp, x + 4.9), i);
        }
    }

    #[test]
    fn crosshair_inside_data() {
        let b = bars(100);
        let vp = plot();
        let ch = crosshair(
            &b,
            &vp,
            25,
            60_000,
            1.0,
            CrosshairMode::Normal,
            503.0,
            250.0,
        )
        .unwrap();
        assert_eq!(ch.slot, 49);
        assert_eq!(ch.bar, Some(49));
        assert_eq!(ch.time, Some(b[49].time));
        assert_eq!(ch.x, 500.5, "slot centre, on a device-pixel centre");
        assert_eq!(ch.price, Price(401_000));
        assert_eq!(ch.y, 250.0);
    }

    #[test]
    fn crosshair_price_snaps_to_tick() {
        let b = bars(100);
        let vp = plot(); // 4 price units per px; tick 25 = 6.25 px
        for y in [0.0, 1.0, 3.0, 3.2, 100.7, 250.0, 333.3, 499.0, 499.99] {
            let ch = crosshair(&b, &vp, 25, 60_000, 1.0, CrosshairMode::Normal, 500.0, y).unwrap();
            assert_eq!(ch.price.0 % 25, 0, "off tick at y={y}: {ch:?}");
            let raw = vp.y_to_price(y);
            assert!(
                (ch.price.0 as f64 - raw).abs() <= 25.0,
                "y={y}: {} vs raw {raw}",
                ch.price.0
            );
            assert!(ch.y >= 0.0 && ch.y <= 500.0, "line off screen at y={y}");
        }
        // 401_000 is at y=250, 401_025 at y=243.75: y=247 is nearer 401_000.
        assert_eq!(snap_price(&vp, 247.0, 25), Some(Price(401_000)));
        assert_eq!(snap_price(&vp, 246.0, 25), Some(Price(401_025)));
    }

    #[test]
    fn crosshair_price_stays_on_screen_at_edges() {
        // Range 400_010..401_990: the tick nearest the top edge (402_000) is above it.
        let vp = Viewport {
            price_lo: 400_010.0,
            price_hi: 401_990.0,
            ..plot()
        };
        assert_eq!(snap_price(&vp, 0.0, 25), Some(Price(401_975)));
        assert_eq!(snap_price(&vp, 499.9, 25), Some(Price(400_025)));
        // A tick bigger than the whole range: nothing on screen to move to, keep the nearest.
        assert_eq!(snap_price(&vp, 250.0, 100_000), Some(Price(400_000)));
    }

    #[test]
    fn crosshair_before_first_bar() {
        let b = bars(20);
        let mut vp = plot();
        vp.right_index = 60.0; // bar 0 centred at x = 400; empty space to its left
        let ch = crosshair(&b, &vp, 25, 60_000, 1.0, CrosshairMode::Normal, 100.0, 10.0).unwrap();
        assert_eq!(ch.slot, -30);
        assert_eq!(ch.bar, None, "no bar before the first one");
        assert_eq!(ch.time, Some(UtcMillis(b[0].time.0 - 30 * 60_000)));
        assert_eq!(ch.x, 100.5);
        // The left half of bar 0's slot still hits bar 0.
        let ch = crosshair(&b, &vp, 25, 60_000, 1.0, CrosshairMode::Normal, 395.0, 10.0).unwrap();
        assert_eq!(ch.bar, Some(0));
        let ch = crosshair(&b, &vp, 25, 60_000, 1.0, CrosshairMode::Normal, 394.9, 10.0).unwrap();
        assert_eq!(ch.bar, None);
        assert_eq!(ch.slot, -1);
    }

    #[test]
    fn crosshair_after_last_bar() {
        let b = bars(97); // last bar 96; slots 97..=99 are the empty right margin
        let vp = plot();
        let ch = crosshair(&b, &vp, 25, 60_000, 1.0, CrosshairMode::Normal, 995.0, 10.0).unwrap();
        assert_eq!(ch.slot, 99);
        assert_eq!(ch.bar, None);
        assert_eq!(ch.time, Some(UtcMillis(b[96].time.0 + 3 * 60_000)));
        let ch = crosshair(&b, &vp, 25, 60_000, 1.0, CrosshairMode::Normal, 974.9, 10.0).unwrap();
        assert_eq!(ch.bar, Some(96));
    }

    #[test]
    fn crosshair_without_data() {
        let vp = plot();
        let ch = crosshair(
            &[],
            &vp,
            25,
            60_000,
            1.0,
            CrosshairMode::Normal,
            500.0,
            250.0,
        )
        .unwrap();
        assert_eq!(ch.bar, None);
        assert_eq!(ch.time, None, "no time without bars");
        assert_eq!(ch.price, Price(401_000), "the price axis still works");
        assert_eq!(slot_time(&[], 0, 60_000), None);
    }

    #[test]
    fn crosshair_hidden_outside_plot() {
        let b = bars(100);
        let vp = plot();
        for (x, y) in [
            (-0.01, 10.0),
            (1000.0, 10.0), // the price axis starts here
            (10.0, -0.01),
            (10.0, 500.0), // the time axis starts here
            (f64::NAN, 10.0),
            (10.0, f64::INFINITY),
        ] {
            assert_eq!(
                crosshair(&b, &vp, 25, 60_000, 1.0, CrosshairMode::Normal, x, y),
                None,
                "({x}, {y})"
            );
        }
        assert!(crosshair(&b, &vp, 25, 60_000, 1.0, CrosshairMode::Normal, 0.0, 0.0).is_some());
        assert!(
            crosshair(
                &b,
                &vp,
                25,
                60_000,
                1.0,
                CrosshairMode::Normal,
                999.99,
                499.99
            )
            .is_some()
        );
    }

    #[test]
    fn crosshair_is_device_pixel_aligned_at_any_dpi() {
        let b = bars(100);
        for s in DPI {
            let mut vp = plot();
            vp.bar_px = 7.3; // fractional bar spacing
            vp.right_index = 101.37;
            vp.price_lo = 400_003.0;
            for x in [0.0, 0.4, 33.3, 333.33, 666.7, 999.9] {
                for y in [0.0, 0.7, 123.45, 499.9] {
                    let ch =
                        crosshair(&b, &vp, 25, 60_000, s, CrosshairMode::Normal, x, y).unwrap();
                    assert!(on_device_centre(ch.x, s), "x {} at {s}", ch.x);
                    assert!(on_device_grid(ch.y, s), "y {} at {s}", ch.y);
                    // Snapping moves a line by at most half a device pixel.
                    let exact_x = vp.index_to_x(ch.slot as f64);
                    assert!((ch.x as f64 - exact_x).abs() <= 0.5 / s as f64 + 1e-4);
                    let exact_y = vp.price_to_y(ch.price.0 as f64);
                    assert!((ch.y as f64 - exact_y).abs() <= 0.5 / s as f64 + 1e-4);
                    assert_eq!(hit_slot(&vp, x), ch.slot, "same slot at every DPI");
                }
            }
        }
    }

    #[test]
    fn crosshair_line_runs_through_the_wick() {
        let b = bars(100);
        for s in DPI {
            let mut vp = plot();
            vp.bar_px = 6.6;
            vp.right_index = 99.6;
            let mut candles = Vec::with_capacity(400);
            build_candles(&b, &vp, s, &mut candles);
            let first = vp.visible_range(b.len()).start;
            for (k, c) in candles.iter().enumerate() {
                let i = first + k;
                let x = vp.index_to_x(i as f64);
                if !(0.0..vp.width).contains(&x) {
                    continue;
                }
                let ch =
                    crosshair(&b, &vp, 25, 60_000, s, CrosshairMode::Normal, x, 100.0).unwrap();
                assert_eq!(ch.bar, Some(i));
                assert_eq!(ch.x, c.x, "crosshair and wick differ at bar {i}, scale {s}");
            }
        }
    }

    #[test]
    fn candle_bodies_are_centred_on_the_wick() {
        let b = bars(100);
        for s in DPI {
            for bar_px in [1.0, 2.0, 3.3, 6.6, 10.0, 41.0] {
                let vp = Viewport { bar_px, ..plot() };
                let mut out = Vec::with_capacity(1200);
                build_candles(&b, &vp, s, &mut out);
                assert!(!out.is_empty());
                for c in &out {
                    assert!(on_device_centre(c.x, s), "{c:?} at {s}");
                    let width_dev = c.half_width as f64 * 2.0 * s as f64;
                    assert!(
                        (width_dev - width_dev.round()).abs() < 1e-3
                            && width_dev.round() as i64 % 2 == 1,
                        "body {width_dev} device px at {s}, bar_px {bar_px}"
                    );
                    assert!(on_device_grid(c.x - c.half_width, s));
                    assert!(on_device_grid(c.body_top, s) && on_device_grid(c.wick_top, s));
                }
            }
        }
    }

    #[test]
    fn grid_lines_are_device_pixel_aligned() {
        let b = bars(2000);
        for s in DPI {
            let mut vp = Viewport::default();
            vp.on_data(&b, 25);
            vp.zoom_at(400.0, 1.37, b.len());
            let mut scene = Scene::with_capacity(Scene::candles_needed(vp.width));
            scene.build(&b, &vp, &es(), s, 60_000);
            assert!(!scene.price_lines.is_empty() && !scene.time_lines.is_empty());
            for l in &scene.price_lines {
                assert!(on_device_grid(l.y, s), "{l:?} at {s}");
            }
            for l in &scene.time_lines {
                assert!(on_device_centre(l.x, s), "{l:?} at {s}");
            }
        }
    }

    #[test]
    fn ohlcv_readout_uses_instrument_formats() {
        let bar = Bar {
            time: UtcMillis(0),
            open: Price(492_525),
            high: Price(492_600),
            low: Price(492_475),
            close: Price(492_550),
            volume: Qty(1_234),
        };
        let mut s = crate::FixedStr::<128>::new();
        write_ohlcv(&bar, &es(), QtyFormat::WHOLE, &mut s).unwrap();
        assert_eq!(
            s.as_str(),
            "O 4925.25  H 4926.00  L 4924.75  C 4925.50  V 1234"
        );
        // Treasury 32nds and fractional crypto volume.
        let zn = PriceFormat::new(
            PriceScale::new(1, 64),
            DisplayStyle::Fraction { denominator: 32 },
            1,
        );
        let bar = Bar {
            open: Price(112 * 64 + 32),
            high: Price(112 * 64 + 34),
            low: Price(112 * 64),
            close: Price(112 * 64 + 2),
            volume: Qty(150_000_000),
            ..bar
        };
        s.clear();
        write_ohlcv(&bar, &zn, QtyFormat { decimals: 8 }, &mut s).unwrap();
        assert_eq!(s.as_str(), "O 112'16  H 112'17  L 112'00  C 112'01  V 1.5");
    }

    #[test]
    fn time_slot_inverts_slot_time() {
        let b = bars(10);
        let p = 60_000;
        for slot in -5..15 {
            let t = slot_time(&b, slot, p).unwrap();
            assert_eq!(time_slot(&b, t, p), Some(slot), "slot {slot}");
            // Anywhere inside the period maps to the same slot.
            assert_eq!(
                time_slot(&b, UtcMillis(t.0 + p - 1), p),
                Some(slot),
                "slot {slot}"
            );
        }
        assert_eq!(time_slot(&[], UtcMillis(0), p), None);
        // A minute with no bar (gap): missing, not the neighbour.
        let mut gap = b.clone();
        gap.remove(4);
        let t = b[4].time;
        assert_eq!(time_slot(&gap, t, p), None);
        assert_eq!(time_slot(&gap, b[5].time, p), Some(4));
    }
}
