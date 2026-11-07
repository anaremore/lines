//! Chart drawings (roadmap 11): lines, rays, zones, notes and measures in **data space**.
//!
//! A drawing's anchors are a time and an exact [`Price`] (on the product's tick grid), never
//! pixels or bar indices, so a drawing survives zoom, pan, timeframe changes and restarts and
//! shows on every chart of its product. Each frame the chart *projects* its drawings to plot
//! coordinates through a [`TimeAxis`] (its timeframe's bars) and hit-tests the projections.
//! Projection, hit-testing and snapping are pure and allocation-free (`tests/no_alloc.rs`).
//!
//! Time ↔ x: a time inside a bar's bucket maps to that bar's slot plus the fraction of the
//! bucket elapsed (a bucket's start is the bar's centre), so an anchor placed on a 1-minute
//! bar sits proportionally inside the 15-minute bar holding it and returns exactly to its
//! minute on the 1-minute chart. Placing or moving an anchor snaps its time to the start of
//! the bar under the pointer (of the chart it is edited on) and its price to the tick grid,
//! or with the magnet to the bar's open, high, low or close.
//!
//! [`DrawingSet`] holds a product's drawings with undo and redo of every change.

use crate::bars::Bar;
use crate::geometry::magnet_price;
use crate::instrument::Ratio;
use crate::price::{Price, PriceFormat};
use crate::style::{Color, FillStyle, LineStyle, TextStyle};
use crate::time::UtcMillis;
use crate::timeframe::BarsView;
use crate::viewport::Viewport;

/// A point in data space.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Anchor {
    pub time: UtcMillis,
    pub price: Price,
}

/// The drawing tools.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    /// A price level across the whole chart.
    HLine,
    /// A price level from an anchor to the right edge.
    HRay,
    /// A segment between two anchors.
    Trend,
    /// From the first anchor through the second, to the plot's edge.
    Ray,
    /// A zone: the rectangle between two anchors.
    Rect,
    /// A time across the whole chart.
    VLine,
    /// A text note at an anchor.
    Note,
    /// A price-range measure: ticks, points, $ per contract, bars and duration. Temporary
    /// unless pinned.
    Measure,
}

impl Tool {
    pub const ALL: [Tool; 8] = [
        Tool::HLine,
        Tool::HRay,
        Tool::Trend,
        Tool::Ray,
        Tool::Rect,
        Tool::VLine,
        Tool::Note,
        Tool::Measure,
    ];

    /// Anchors placed to draw it: one click, or two (click, then click or drag-release).
    pub fn anchors(self) -> usize {
        match self {
            Tool::HLine | Tool::HRay | Tool::VLine | Tool::Note => 1,
            Tool::Trend | Tool::Ray | Tool::Rect | Tool::Measure => 2,
        }
    }

    /// What menus and the palette call it.
    pub fn label(self) -> &'static str {
        match self {
            Tool::HLine => "horizontal line",
            Tool::HRay => "horizontal ray",
            Tool::Trend => "trend line",
            Tool::Ray => "ray",
            Tool::Rect => "rectangle (zone)",
            Tool::VLine => "vertical line",
            Tool::Note => "text note",
            Tool::Measure => "measure",
        }
    }

    /// The file's name for it.
    pub fn key(self) -> &'static str {
        match self {
            Tool::HLine => "hline",
            Tool::HRay => "hray",
            Tool::Trend => "trend",
            Tool::Ray => "ray",
            Tool::Rect => "rect",
            Tool::VLine => "vline",
            Tool::Note => "note",
            Tool::Measure => "measure",
        }
    }

    pub fn from_key(s: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.key() == s)
    }
}

/// How a drawing is drawn (decision 0026): its line, a zone's fill, a note's text, and the
/// drawing-only options. Which parts and options apply depends on the tool
/// ([`Tool::has_fill`], [`Tool::has_text`], [`Tool::extends`], [`Tool::has_axis_tag`]); the
/// others are kept but unused.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Style {
    pub line: LineStyle,
    /// Zones and measures.
    pub fill: FillStyle,
    /// Notes.
    pub text: TextStyle,
    /// Continue the line past its first anchor to the plot's edge (trend lines, rays,
    /// horizontal rays, zones).
    pub extend_left: bool,
    /// Continue past the second anchor (trend lines, zones; rays always do).
    pub extend_right: bool,
    /// The drawing's price on the price axis (horizontal lines and rays, a zone's two edges).
    pub axis_tag: bool,
}

impl Style {
    /// Drawing colors the theme offers (`Color::Palette` indices).
    pub const COLORS: u8 = 4;

    pub const DEFAULT: Style = Style {
        line: LineStyle::DEFAULT,
        fill: FillStyle::DEFAULT,
        text: TextStyle::DEFAULT,
        extend_left: false,
        extend_right: false,
        axis_tag: false,
    };

    /// In range (a file may hold anything): widths 1–4, opacities 10–100 %, palette indices
    /// wrapped, no study-only color.
    pub fn normalised(self) -> Style {
        let wrap = |c: Color| match c {
            Color::Palette(i) => Color::Palette(i % Self::COLORS),
            // A study part's own color means nothing on a drawing: its default color.
            Color::Own => Color::Palette(0),
            c => c,
        };
        let mut s = self;
        s.line = s.line.normalised();
        s.line.color = wrap(s.line.color);
        s.fill = s.fill.normalised();
        s.fill.color = wrap(s.fill.color);
        s.text.color = wrap(s.text.color);
        s
    }

    /// The line in color `c`; a fill that matched the old line color follows it (a zone's or
    /// a measure's fill stays its line's color unless it was set apart).
    pub fn with_line_color(self, c: Color) -> Style {
        let mut s = self;
        if s.fill.color == s.line.color {
            s.fill.color = c;
        }
        s.line.color = c;
        s
    }

    /// The line's color as a palette index, if it is one (the legacy single color).
    pub fn palette_index(&self) -> Option<u8> {
        match self.line.color {
            Color::Palette(i) => Some(i),
            _ => None,
        }
    }
}

impl Default for Style {
    fn default() -> Self {
        Style::DEFAULT
    }
}

impl Tool {
    /// Zones and measures have a fill.
    pub fn has_fill(self) -> bool {
        matches!(self, Tool::Rect | Tool::Measure)
    }

    /// Notes have text.
    pub fn has_text(self) -> bool {
        self == Tool::Note
    }

    /// Which extensions apply: (left, right). A ray always extends right, so only left is
    /// offered; a horizontal ray likewise.
    pub fn extends(self) -> (bool, bool) {
        match self {
            Tool::Trend | Tool::Rect => (true, true),
            Tool::Ray | Tool::HRay => (true, false),
            _ => (false, false),
        }
    }

    /// Whether the drawing can tag its price on the axis (an exact price: horizontal lines and
    /// rays, a zone's top and bottom).
    pub fn has_axis_tag(self) -> bool {
        matches!(self, Tool::HLine | Tool::HRay | Tool::Rect)
    }

    /// Whether its line has a type and width (a note's line is only its edge marker).
    pub fn has_stroke(self) -> bool {
        self != Tool::Note
    }
}

pub type DrawingId = u64;

/// One drawing. `a` is the first anchor; `b` the second for two-anchor tools (equal to `a`
/// otherwise). A horizontal line uses only `a.price`, a vertical line only `a.time`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drawing {
    pub id: DrawingId,
    pub tool: Tool,
    pub a: Anchor,
    pub b: Anchor,
    pub style: Style,
    /// A note's text (empty for other tools).
    pub text: String,
}

impl Drawing {
    pub fn new(tool: Tool, a: Anchor, b: Anchor, style: Style) -> Drawing {
        let b = if tool.anchors() == 2 { b } else { a };
        Drawing {
            id: 0,
            tool,
            a,
            b,
            style,
            text: String::new(),
        }
    }

    /// The drawing with handle `i` (0 = `a`, 1 = `b`) moved to `at`.
    pub fn with_handle(&self, i: usize, at: Anchor) -> Drawing {
        let mut d = self.clone();
        match (i, self.tool.anchors()) {
            (0, 1) => {
                d.a = at;
                d.b = at;
            }
            (0, _) => d.a = at,
            _ => d.b = at,
        }
        d
    }

    /// Whether it is on the tick grid (`tick` units); a file may hold anything.
    pub fn on_grid(&self, tick: i64) -> bool {
        let t = tick.max(1);
        self.a.price.0 % t == 0 && self.b.price.0 % t == 0
    }
}

// ---- time axis --------------------------------------------------------------------------

/// Maps times to fractional bar indices of one chart's bars (its timeframe) and back.
#[derive(Copy, Clone, Debug)]
pub struct TimeAxis<'a> {
    pub view: BarsView<'a>,
}

impl<'a> TimeAxis<'a> {
    pub fn new(view: BarsView<'a>) -> TimeAxis<'a> {
        TimeAxis { view }
    }

    /// The fractional index of `t`: bar `i`'s start is `i`, its end `i + 1` (a gap between
    /// buckets collapses onto the next bar's start); outside the bars, by whole periods.
    /// `None` without bars.
    pub fn index_of(&self, t: UtcMillis) -> Option<f64> {
        let bars = self.view.bars;
        let (first, last) = (bars.first()?, bars.len() - 1);
        let p = self.view.period_ms.max(1) as f64;
        if t < first.time {
            return Some((t.0 - first.time.0) as f64 / p);
        }
        let i = bars.partition_point(|b| b.time <= t) - 1;
        let start = bars[i].time.0;
        if i == last && t >= self.view.end(i) {
            return Some(last as f64 + (t.0 - start) as f64 / p);
        }
        let len = (self.view.end(i).0 - start).max(1) as f64;
        Some(i as f64 + ((t.0 - start) as f64 / len).min(1.0))
    }

    /// The start of slot `slot`'s bar; outside the bars, extrapolated by whole periods.
    pub fn slot_time(&self, slot: i64) -> Option<UtcMillis> {
        let bars = self.view.bars;
        let (first, last) = (bars.first()?, bars.len() as i64 - 1);
        let p = self.view.period_ms.max(1);
        Some(if slot < 0 {
            UtcMillis(first.time.0 + slot * p)
        } else if slot > last {
            UtcMillis(bars[last as usize].time.0 + (slot - last) * p)
        } else {
            bars[slot as usize].time
        })
    }

    /// The bar in slot `slot`, if there is one.
    pub fn bar(&self, slot: i64) -> Option<&'a Bar> {
        usize::try_from(slot)
            .ok()
            .and_then(|i| self.view.bars.get(i))
    }

    /// Plot-local x of `t`.
    pub fn x_of(&self, vp: &Viewport, t: UtcMillis) -> Option<f32> {
        self.index_of(t).map(|i| vp.index_to_x(i) as f32)
    }
}

/// Snap a pointer at plot-local (`x`, `y`) to an anchor: the start of the bar under it and a
/// price on the tick grid, or with `magnet` the bar's open, high, low or close nearest the
/// pointer (off the bars, the grid). `None` without bars.
pub fn snap_anchor(
    axis: &TimeAxis<'_>,
    vp: &Viewport,
    tick: i64,
    magnet: bool,
    x: f64,
    y: f64,
) -> Option<Anchor> {
    let slot = (vp.x_to_index(x) + 0.5).floor() as i64;
    let time = axis.slot_time(slot)?;
    let raw = vp.y_to_price(y);
    let price = match axis.bar(slot) {
        Some(bar) if magnet => magnet_price(bar, raw),
        _ => grid_price(raw, tick),
    };
    Some(Anchor { time, price })
}

/// The tick-grid price nearest `raw` (units).
pub fn grid_price(raw: f64, tick: i64) -> Price {
    let t = tick.max(1) as f64;
    Price(((raw / t).round() * t) as i64)
}

// ---- projection -------------------------------------------------------------------------

/// A drawing in plot-local logical px for one frame. Lines are already extended or clipped
/// to the plot as drawn; `handles` are the anchors' points.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Projected {
    pub id: DrawingId,
    pub tool: Tool,
    pub style: Style,
    /// Line start and end; a zone's or measure's corners (any order); a note's anchor and the
    /// bottom-right of its text box (the chart widens it to the shaped text).
    pub p0: (f32, f32),
    pub p1: (f32, f32),
    pub handles: [(f32, f32); 2],
    pub n_handles: u8,
    /// The anchors' prices (for axis tags): `a`, `b`.
    pub prices: [Price; 2],
}

/// Default size of a note's box before the chart measures its text (logical px).
pub const NOTE_BOX: (f32, f32) = (80.0, 18.0);

/// Project `d` for this frame. `None` when nothing of it can be placed (no bars) or it lies
/// wholly outside the plot (off-screen drawings are skipped, never clipped to a sliver).
pub fn project(d: &Drawing, axis: &TimeAxis<'_>, vp: &Viewport) -> Option<Projected> {
    let (w, h) = (vp.width as f32, vp.height as f32);
    let y = |p: Price| vp.price_to_y(p.0 as f64) as f32;
    let xa = axis.x_of(vp, d.a.time);
    let xb = axis.x_of(vp, d.b.time);
    let (ya, yb) = (y(d.a.price), y(d.b.price));
    let mut out = Projected {
        id: d.id,
        tool: d.tool,
        style: d.style,
        p0: (0.0, 0.0),
        p1: (0.0, 0.0),
        handles: [(0.0, 0.0); 2],
        n_handles: 1,
        prices: [d.a.price, d.b.price],
    };
    let (ext_l, ext_r) = {
        let (l, r) = d.tool.extends();
        (l && d.style.extend_left, r && d.style.extend_right)
    };
    match d.tool {
        Tool::HLine => {
            if !(0.0..=h).contains(&ya) {
                return None;
            }
            out.p0 = (0.0, ya);
            out.p1 = (w, ya);
            out.handles[0] = (w * 0.5, ya);
        }
        Tool::HRay => {
            let x = xa?;
            if !(0.0..=h).contains(&ya) || x > w {
                return None;
            }
            out.p0 = (if ext_l { 0.0 } else { x }, ya);
            out.p1 = (w, ya);
            out.handles[0] = (x, ya);
        }
        Tool::VLine => {
            let x = xa?;
            if !(0.0..=w).contains(&x) {
                return None;
            }
            out.p0 = (x, 0.0);
            out.p1 = (x, h);
            out.handles[0] = (x, h * 0.5);
        }
        Tool::Note => {
            let x = xa?;
            out.p0 = (x, ya);
            out.p1 = (x + NOTE_BOX.0, ya + NOTE_BOX.1);
            out.handles[0] = (x, ya);
            if x > w || x + NOTE_BOX.0 < 0.0 || ya > h || ya + NOTE_BOX.1 < 0.0 {
                return None;
            }
        }
        Tool::Rect | Tool::Measure => {
            let (x0, x1) = (xa?, xb?);
            // An extended zone runs to the plot's edge on that side (its handles stay put).
            let (lo, hi) = (x0.min(x1), x0.max(x1));
            let (lo, hi) = (
                if ext_l { lo.min(0.0) } else { lo },
                if ext_r { hi.max(w) } else { hi },
            );
            let (l, r) = if x0 <= x1 { (lo, hi) } else { (hi, lo) };
            out.p0 = (l, ya);
            out.p1 = (r, yb);
            out.handles = [(x0, ya), (x1, yb)];
            out.n_handles = 2;
            if lo.max(hi) < 0.0 || lo.min(hi) > w || ya.max(yb) < 0.0 || ya.min(yb) > h {
                return None;
            }
        }
        Tool::Trend | Tool::Ray => {
            let (x0, x1) = (xa?, xb?);
            out.handles = [(x0, ya), (x1, yb)];
            out.n_handles = 2;
            let right = ext_r || d.tool == Tool::Ray;
            out.p0 = if ext_l {
                extend_to_edge((x1, yb), (x0, ya), w, h)
            } else {
                (x0, ya)
            };
            out.p1 = if right {
                extend_to_edge((x0, ya), (x1, yb), w, h)
            } else {
                (x1, yb)
            };
            let (lo, hi) = (out.p0.0.min(out.p1.0), out.p0.0.max(out.p1.0));
            let (top, bottom) = (out.p0.1.min(out.p1.1), out.p0.1.max(out.p1.1));
            if hi < 0.0 || lo > w || bottom < 0.0 || top > h {
                return None;
            }
        }
    }
    Some(out)
}

/// From `a` through `b` to where the line leaves the plot `[0, w] × [0, h]` (or `b` itself
/// if the two coincide).
fn extend_to_edge(a: (f32, f32), b: (f32, f32), w: f32, h: f32) -> (f32, f32) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    if dx.abs() < 1e-6 && dy.abs() < 1e-6 {
        return b;
    }
    // The largest t >= 1 that stays inside the box, from each side the direction heads to.
    let mut t = f32::INFINITY;
    if dx > 0.0 {
        t = t.min((w - a.0) / dx);
    } else if dx < 0.0 {
        t = t.min(-a.0 / dx);
    }
    if dy > 0.0 {
        t = t.min((h - a.1) / dy);
    } else if dy < 0.0 {
        t = t.min(-a.1 / dy);
    }
    let t = t.max(1.0);
    (a.0 + dx * t, a.1 + dy * t)
}

// ---- hit-testing ------------------------------------------------------------------------

/// What a pointer is on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    /// Anchor handle `i` (drag to reshape).
    Handle(usize),
    /// The drawing itself (drag to move).
    Body,
}

/// Distance from `p` to the segment `a`–`b`.
pub fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= f32::EPSILON {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    let (cx, cy) = (a.0 + dx * t, a.1 + dy * t);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

/// Whether (`x`, `y`) is on `d`: a handle within `handle_r` (only when `handles` is set, i.e.
/// the drawing is selected) or the drawing within `tol` (zones, measures and notes: anywhere
/// inside).
pub fn hit(d: &Projected, x: f32, y: f32, tol: f32, handles: bool, handle_r: f32) -> Option<Hit> {
    let p = (x, y);
    if handles {
        for i in 0..d.n_handles as usize {
            let (hx, hy) = d.handles[i];
            if (x - hx).abs() <= handle_r && (y - hy).abs() <= handle_r {
                return Some(Hit::Handle(i));
            }
        }
    }
    let inside = |a: (f32, f32), b: (f32, f32)| {
        x >= a.0.min(b.0) - tol
            && x <= a.0.max(b.0) + tol
            && y >= a.1.min(b.1) - tol
            && y <= a.1.max(b.1) + tol
    };
    // Zones and measures are hit on their edges only, so the chart still pans from inside a
    // wide zone; a note (a small text box) anywhere on it.
    let edges = |a: (f32, f32), b: (f32, f32)| {
        let (c, d) = ((a.0, b.1), (b.0, a.1));
        [(a, c), (c, b), (b, d), (d, a)]
            .iter()
            .any(|&(s, e)| segment_distance(p, s, e) <= tol)
    };
    let on = match d.tool {
        Tool::Note => inside(d.p0, d.p1),
        Tool::Rect | Tool::Measure => edges(d.p0, d.p1),
        _ => segment_distance(p, d.p0, d.p1) <= tol,
    };
    on.then_some(Hit::Body)
}

/// The top-most drawing under the pointer: the selected drawing's handles first, then the
/// drawings from the last drawn (on top) back. Allocation-free.
pub fn pick(
    drawn: &[Projected],
    selected: Option<DrawingId>,
    x: f32,
    y: f32,
    tol: f32,
    handle_r: f32,
) -> Option<(DrawingId, Hit)> {
    if let Some(s) = selected
        && let Some(d) = drawn.iter().find(|d| d.id == s)
        && let Some(h @ Hit::Handle(_)) = hit(d, x, y, tol, true, handle_r)
    {
        return Some((s, h));
    }
    drawn
        .iter()
        .rev()
        .find_map(|d| hit(d, x, y, tol, false, handle_r).map(|h| (d.id, h)))
}

// ---- measure ----------------------------------------------------------------------------

/// What a measure from `a` to `b` reads.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MeasureStats {
    /// Price change in units (`b - a`).
    pub units: i64,
    /// In ticks (exact when both anchors are on the grid).
    pub ticks: i64,
    /// Per contract, in cents (rounded half away from zero; display only).
    pub cents: i128,
    /// Bars between the anchors on the chart measured on.
    pub bars: i64,
    pub duration_ms: i64,
}

pub fn measure(
    a: Anchor,
    b: Anchor,
    axis: &TimeAxis<'_>,
    fmt: &PriceFormat,
    point_value: Ratio,
) -> MeasureStats {
    let units = b.price.0 - a.price.0;
    let bars = match (axis.index_of(a.time), axis.index_of(b.time)) {
        (Some(i), Some(j)) => (j - i).round() as i64,
        _ => 0,
    };
    // units × (scale num/den) points × (point value num/den) dollars × 100 cents.
    let num = i128::from(units) * i128::from(fmt.scale.num()) * i128::from(point_value.num) * 100;
    let den = i128::from(fmt.scale.den()) * i128::from(point_value.den);
    let cents = if den == 0 {
        0
    } else {
        let q = num / den;
        let r = num % den;
        if 2 * r.abs() >= den.abs() {
            q + num.signum() * den.signum()
        } else {
            q
        }
    };
    MeasureStats {
        units,
        ticks: units / fmt.tick.max(1),
        cents,
        bars,
        duration_ms: b.time.0 - a.time.0,
    }
}

/// Which amounts a measure shows. The app maps its P&L display setting onto this exactly:
/// off = neither, ticks = ticks, currency = money, both = both (points, bars and duration
/// always show).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MeasureParts {
    pub ticks: bool,
    pub money: bool,
}

impl MeasureParts {
    pub const ALL: MeasureParts = MeasureParts {
        ticks: true,
        money: true,
    };
}

/// `MeasureStats` as one line, e.g.
/// `+12 ticks · +3.00 pts · +$1,500.00/contract · 85 bars · 1h 25m`, without allocating.
/// Points, bars and duration always show; ticks and money as `parts` says.
pub fn write_measure<W: std::fmt::Write>(
    s: &MeasureStats,
    fmt: &PriceFormat,
    parts: MeasureParts,
    out: &mut W,
) -> std::fmt::Result {
    const SEP: &str = " \u{b7} ";
    let sign = |v: i128| match v.signum() {
        1 => "+",
        -1 => "-",
        _ => "",
    };
    if parts.ticks {
        write!(out, "{}{} ticks{SEP}", sign(s.ticks.into()), s.ticks.abs())?;
    }
    out.write_str(sign(s.units.into()))?;
    fmt.write(Price(s.units.abs()), out)?;
    out.write_str(" pts")?;
    if parts.money {
        write!(out, "{SEP}{}$", sign(s.cents))?;
        write_thousands(s.cents.unsigned_abs() / 100, out)?;
        write!(out, ".{:02}/contract", s.cents.unsigned_abs() % 100)?;
    }
    write!(out, "{SEP}{} bars{SEP}", s.bars.abs())?;
    let mins = s.duration_ms.abs() / 60_000;
    match (mins / 1_440, (mins / 60) % 24, mins % 60) {
        (0, 0, m) => write!(out, "{m}m"),
        (0, h, m) => write!(out, "{h}h {m:02}m"),
        (d, h, _) => write!(out, "{d}d {h}h"),
    }
}

/// `n` with US thousands separators (`1,500`), without allocating.
fn write_thousands<W: std::fmt::Write>(n: u128, out: &mut W) -> std::fmt::Result {
    if n >= 1_000 {
        write_thousands(n / 1_000, out)?;
        write!(out, ",{:03}", n % 1_000)
    } else {
        write!(out, "{n}")
    }
}

// ---- the set, with undo --------------------------------------------------------------------

/// One undoable change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Add(Drawing),
    Remove(Drawing),
    Edit { before: Drawing, after: Drawing },
    Clear(Vec<Drawing>),
}

/// A product's drawings, in drawing order (later on top), with undo and redo.
#[derive(Clone, Debug, Default)]
pub struct DrawingSet {
    items: Vec<Drawing>,
    next_id: DrawingId,
    undo: Vec<Change>,
    redo: Vec<Change>,
    revision: u64,
}

/// Undo steps kept per product.
pub const UNDO_DEPTH: usize = 100;

impl DrawingSet {
    /// From saved drawings (ids are kept; new ones continue after the largest).
    pub fn from_drawings(items: Vec<Drawing>) -> DrawingSet {
        let next_id = items.iter().map(|d| d.id).max().unwrap_or(0) + 1;
        DrawingSet {
            items,
            next_id,
            ..DrawingSet::default()
        }
    }

    pub fn items(&self) -> &[Drawing] {
        &self.items
    }

    /// New drawings get ids above `id` (ids held elsewhere, e.g. entries a file kept that
    /// this version can't use).
    pub fn reserve_ids_through(&mut self, id: DrawingId) {
        self.next_id = self.next_id.max(id + 1);
    }

    pub fn get(&self, id: DrawingId) -> Option<&Drawing> {
        self.items.iter().find(|d| d.id == id)
    }

    /// Bumped by every change (including undo and redo).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Add `d` (its id is assigned); returns the id.
    pub fn add(&mut self, mut d: Drawing) -> DrawingId {
        d.id = self.next_id.max(1);
        self.next_id = d.id + 1;
        let id = d.id;
        self.record(Change::Add(d));
        id
    }

    /// Replace drawing `after.id` with `after`. False if it isn't there or nothing changed.
    pub fn edit(&mut self, after: Drawing) -> bool {
        match self.get(after.id) {
            Some(before) if *before != after => {
                let before = before.clone();
                self.record(Change::Edit { before, after });
                true
            }
            _ => false,
        }
    }

    pub fn remove(&mut self, id: DrawingId) -> bool {
        match self.get(id).cloned() {
            Some(d) => {
                self.record(Change::Remove(d));
                true
            }
            None => false,
        }
    }

    /// Remove every drawing (one undo step).
    pub fn clear(&mut self) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let all = self.items.clone();
        self.record(Change::Clear(all));
        true
    }

    pub fn undo(&mut self) -> bool {
        let Some(c) = self.undo.pop() else {
            return false;
        };
        self.apply(&c, true);
        self.redo.push(c);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(c) = self.redo.pop() else {
            return false;
        };
        self.apply(&c, false);
        self.undo.push(c);
        true
    }

    fn record(&mut self, c: Change) {
        self.apply(&c, false);
        self.undo.push(c);
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn apply(&mut self, c: &Change, reverse: bool) {
        self.revision += 1;
        match (c, reverse) {
            (Change::Add(d), false) | (Change::Remove(d), true) => self.insert(d.clone()),
            (Change::Add(d), true) | (Change::Remove(d), false) => {
                self.items.retain(|x| x.id != d.id)
            }
            (Change::Edit { after, .. }, false) => self.replace(after),
            (Change::Edit { before, .. }, true) => self.replace(before),
            (Change::Clear(_), false) => self.items.clear(),
            (Change::Clear(all), true) => self.items = all.clone(),
        }
    }

    /// Insert keeping id order (drawing order is creation order).
    fn insert(&mut self, d: Drawing) {
        let i = self.items.partition_point(|x| x.id < d.id);
        self.items.insert(i, d);
    }

    fn replace(&mut self, d: &Drawing) {
        if let Some(x) = self.items.iter_mut().find(|x| x.id == d.id) {
            *x = d.clone();
        }
    }
}

#[cfg(test)]
mod tests;
