//! A chart pane's indicators (roadmap 5c): VWAPs from the shared indicator engine, drawn in the
//! plot, cumulative delta (roadmap 10e) drawn as candles in a lower pane below it, and Volume
//! (from the chart's own bars, no engine node) in a lower pane or along the plot's bottom, with a
//! legend.
//!
//! Lower panes (`crate::subpane`): every shown study whose config says `in_subpane` gets one,
//! in the studies' order; the chart asks for pane `k`'s scale, paint, title and newest value
//! here ([`Studies::lower`] and the `lower_*` methods), so a new kind of lower-pane study adds
//! its arms here and nothing in the chart.
//!
//! The pane owns a [`Studies`]: one [`IndicatorConfig`] per indicator (what the workspace
//! saves) and an engine handle for each. Paint reads each handle's cell with `try_lock`, builds
//! the visible columns into a fixed buffer (`lines_indicators::render`, no allocation) and draws
//! a smooth anti-aliased line and band fills through them. Nothing is computed on the UI thread.
//!
//! Styles (decision 0026, `crate::study_style`): each study's parts are resolved into
//! [`Look`]s when its configuration changes; paint only reads them (no allocation of ours).
//!
//! Menu keys: ↑↓ select · Enter add (on "+ VWAP") · R anchor (ETH → RTH → week → month →
//! anchored at the bar under the crosshair, else the newest bar) · B bands (off → ±1 → ±1,2 →
//! ±1,2,3) · C colour · Delete remove · Esc / Ctrl+I close.

use std::cell::Cell;
use std::collections::HashMap;

use gpui::{
    AnyElement, App, Bounds, Hsla, IntoElement, ParentElement, PathBuilder, Pixels, ShapedLine,
    SharedString, Styled, TextAlign, TextRun, Window, div, fill, point, px, size,
};
use lines_core::profile::Footprint;
use lines_core::style::{Color, LineType};
use lines_core::volume::{VolumeBar, VolumeFrame, VolumeOptions};
use lines_core::{BarsView, Product, Viewport};
use lines_feed::indicators::{FeedInfo, IndicatorHandle, engine};
use lines_indicators::render::{Column, DeltaCandle, build_columns, build_delta_candles};
use lines_indicators::{Bands, IndicatorConfig, Reset};

use crate::series_pane::{ExtFrame, ExtStatus};
use crate::study_style::{self, Look};
use crate::subpane::Scale;
use crate::theme::Theme;
use lines_feed::levels::SeriesStudyDef;

struct Study {
    config: IndicatorConfig,
    /// Its parts' looks (`study_style::VWAP` or `DELTA` order), from `config`.
    looks: Vec<Look>,
    handle: Option<IndicatorHandle>,
    columns: Vec<Column>,
    /// Cumulative delta: this frame's candles and their extremes, and the newest value.
    candles: Vec<DeltaCandle>,
    extent: Option<(i64, i64)>,
    newest: Option<i64>,
    /// The newest value is approximate (from bars); shown in the legend.
    approx: bool,
    /// A VWAP's newest point's time (its late start's tag is dated against it) and late start
    /// (the legend's fallback while the engine is writing).
    newest_time: Option<lines_core::UtcMillis>,
    newest_from: Option<lines_core::UtcMillis>,
    /// A VWAP's newest value (the end tag's measure falls back to it while the engine writes).
    newest_vwap: Option<f64>,
    /// A VWAP's end tags in the last paint ([`end_bit`]: its line's, or, off the plot, its band
    /// edges' on it; `None`: not painted yet, every one measured).
    end_tags: Option<u8>,
    /// A VWAP's line was off the scale in the last paint: it comes back only once inside by
    /// more than a tick (the end tags' gutter moving the scale doesn't flip it back and forth).
    off_scale: bool,
    /// Volume: this frame's bars.
    vol: VolumeFrame,
    /// An external series study: its definition (from its source, `None` while the source
    /// isn't registered) and this frame's lines.
    ext_def: Option<&'static SeriesStudyDef>,
    /// `levels::serving_generation()` when `ext_def` was last looked up (a missing source is
    /// looked up again only when the registered sources change: never every frame).
    ext_gen: u64,
    ext: ExtFrame,
    error: Option<String>,
    /// The band fill as drawn under the candles (`Theme::under_candles`) for (theme, fill as
    /// styled, nested bands): computed when one of them changes, not every frame.
    fill_cap: Cell<Option<FillCap>>,
    /// Volume's overlay colors (bars, up, down) as drawn under the candles, for (theme, colors
    /// as styled): computed when one of them changes.
    overlay_cap: Cell<Option<OverlayCap>>,
}

type OverlayCap = (&'static str, [Hsla; 3], [Hsla; 3]);

/// How opaque Volume's overlay bars are (of their pane color) before the under-candles hold.
const OVERLAY_ALPHA: f32 = 0.35;
/// The overlay's share of the plot's height, from its bottom edge.
pub const OVERLAY_SHARE: f32 = 0.2;

/// A fill color from theme tokens or the study's style.
type FillColor = Hsla;
/// A line color as drawn.
type LineColor = Hsla;

type FillCap = (&'static str, Hsla, u8, FillColor);

impl Study {
    /// Volume's colors under the candles (overlay), held back as a band fill is.
    fn overlay_colors(&self, theme: &Theme, cols: [Hsla; 3]) -> [Hsla; 3] {
        if let Some((name, i, out)) = self.overlay_cap.get()
            && name == theme.name
            && i == cols
        {
            return out;
        }
        let out = cols.map(|c| theme.under_candles(c.opacity(c.a * OVERLAY_ALPHA), 1).0);
        self.overlay_cap.set(Some((theme.name, cols, out)));
        out
    }

    /// The band fill `fill` as drawn under the candles with `layers` nested bands, in `theme`.
    fn fill_under_candles(&self, theme: &Theme, fill: Hsla, layers: u8) -> FillColor {
        if let Some((name, f, l, out)) = self.fill_cap.get()
            && name == theme.name
            && f == fill
            && l == layers
        {
            return out;
        }
        let out = theme.under_candles(fill, layers).0;
        self.fill_cap.set(Some((theme.name, fill, layers, out)));
        out
    }
}

/// How many band fills nest at a VWAP's innermost band.
pub fn band_layers(b: Bands) -> u8 {
    (b.0 & 0b111).count_ones() as u8
}

#[derive(Default)]
pub struct Studies {
    list: Vec<Study>,
    feed: Option<FeedInfo>,
    /// The plot's VWAP tags (off-scale markers, late-start tags, end tags), shaped on a miss
    /// only: (first study, tag, value) → (color shaped in, line). Cleared when the display
    /// zone (`display_tz::epoch`) or the UI zoom (`zoom::generation`) changes, or it grows
    /// past [`MAX_TAGS`].
    tags: TagCache,
    tags_epoch: (u64, u32),
    /// This frame's visible bars as plot px boxes (left, right, top, bottom): late-start tags
    /// are placed clear of them. Built in `prepare` into a buffer reserved as the columns are.
    bar_boxes: Vec<[f32; 4]>,
    /// The width the VWAPs' end tags want right of the newest bar, measured in render
    /// ([`Studies::measure_end_tags`]; the chart adds it to the labels' gutter), and the
    /// texts it was measured for.
    end_need: f32,
    end_texts: String,
    /// This frame's newest bar's slot's right edge in plot px: end tags go right of it.
    newest_right: Option<f32>,
    /// A line reaches the newest bar from here ([`reach_from`], [`reaches_newest`]; `None`:
    /// no bars yet).
    reach_from: Option<lines_core::UtcMillis>,
    /// The chart's newest bar's start (`None`: no bars yet).
    newest_bar: Option<lines_core::UtcMillis>,
    /// Whether the newest bar is on the plot: panned back, no end tags (paint and measure).
    newest_on_plot: bool,
    /// This frame's off-scale markers moved to the price axis column ([`axis_mark_x`]):
    /// painted after the axis ([`Studies::paint_axis_marks`]).
    axis_marks: [Option<AxisMark>; MAX_AXIS_MARKS],
}

/// An off-scale marker in the price axis column: its shaped tag's key, its line's color and
/// its box (plot px: x, y, w, h).
#[derive(Copy, Clone, Debug)]
struct AxisMark {
    key: (usize, Tag, i64),
    color: Hsla,
    at: [f32; 4],
}

/// Whether a marker box `[y, y + h)` stays clear of every blocked span (the last-price tag,
/// the drawings' price tags) in the price axis column.
fn clear_of(blocked: &[(f32, f32)], y: f32, h: f32) -> bool {
    blocked.iter().all(|&(t, b)| y + h <= t || y >= b)
}

/// Tags one frame moves out of the plot, at most: off-scale markers to the price axis, and
/// end tags to the axis or the label column (later ones are dropped).
const MAX_AXIS_MARKS: usize = 12;

/// Where this frame's VWAP tags may go outside the plot, from the chart's study label
/// placement ([`crate::levels_layer::LabelPlace`]).
#[derive(Copy, Clone, Debug)]
pub struct TagRoom<'a> {
    /// Everything right of the plot: the label column (if any) and the price axis, px.
    pub axis_w: f32,
    /// The label column's width (0: none).
    pub column: f32,
    /// Where end tags and off-scale markers go: tags in the price axis, in the label column,
    /// or on the chart (a column placement without a column draws them on the chart).
    pub place: crate::levels_layer::LabelPlace,
    /// Rows (top, bottom, plot px) no tag in the price axis may cover: the last-price tag, the
    /// drawings' and the levels' tags.
    pub blocked: &'a [(f32, f32)],
    /// Rows no tag in the label column may cover: the levels' tags there.
    pub column_blocked: &'a [(f32, f32)],
}

/// Where a VWAP tag `w` wide goes outside the plot under `place` (axis or column; a plot `pw`
/// wide): its x and the rows it must keep clear of, or `None` when it doesn't fit that room.
fn out_of_plot<'a>(
    room: &TagRoom<'a>,
    place: crate::levels_layer::LabelPlace,
    pw: f32,
    w: f32,
) -> Option<(f32, &'a [(f32, f32)])> {
    use crate::levels_layer::{AXIS_LEADER, COLUMN_PAD, LabelPlace};
    let (x, room_w, rows) = match place {
        LabelPlace::Axis => (
            pw + room.column + AXIS_LEADER,
            room.axis_w - room.column - AXIS_LEADER - 2.0,
            room.blocked,
        ),
        LabelPlace::Column => (
            pw + COLUMN_PAD,
            room.column - 2.0 * COLUMN_PAD,
            room.column_blocked,
        ),
        LabelPlace::Chart => return None,
    };
    (w <= room_w).then_some((x, rows))
}

/// The tops of the tags already moved out of the plot this frame (each `h` tall).
fn taken_tops(marks: &[Option<AxisMark>]) -> ([f32; MAX_AXIS_MARKS], usize) {
    let mut taken = [0.0f32; MAX_AXIS_MARKS];
    let mut n = 0;
    for m in marks.iter().flatten().take(MAX_AXIS_MARKS) {
        taken[n] = m.at[1];
        n += 1;
    }
    (taken, n)
}

/// The nearest free row for a tag `h` tall that wants top `want` on a plot `ph` tall: its own,
/// else a row up or down, at most two rows away, clear of `blocked` and of `taken` (tops of
/// tags already there, each `h` tall). `None`: none is free (the tag isn't drawn).
fn free_row(want: f32, h: f32, ph: f32, blocked: &[(f32, f32)], taken: &[f32]) -> Option<f32> {
    let top = |y: f32| y.clamp(0.0, (ph - h).max(0.0));
    [0.0f32, -1.0, 1.0, -2.0, 2.0]
        .iter()
        .map(|k| top(want + k * (h + 1.0)))
        .find(|&y| clear_of(blocked, y, h) && taken.iter().all(|&t| y + h <= t || y >= t + h))
}

/// Where an off-scale marker `w` wide goes when the strip right of the newest bar (clear from
/// `x_free`) is narrower than it: right-aligned to the price axis column's right edge (the
/// axis `axis_w` wide right of a plot `pw` wide), over that strip where it is wider than the
/// column. `None` when it would reach over a bar: it isn't drawn (a marker never covers bars).
fn axis_mark_x(w: f32, x_free: f32, pw: f32, axis_w: f32) -> Option<f32> {
    let x = pw + axis_w - w;
    (x >= x_free.min(pw)).then_some(x)
}

type TagCache = HashMap<(usize, Tag, i64), (Hsla, ShapedLine)>;

/// A text tag a VWAP draws in the plot.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum Tag {
    /// The line (band 0) or its ±kσ edge is above (`true`) or below the plot: "ETH 6009.50 ▲"
    /// or "ETH +1σ 6020.25 ▲" at that edge.
    Edge(bool, i8),
    /// Where late runs begin (the studies in the mask, by list index): "ETH = RTH from 12:34
    /// ET", dated when not on the newest bar's day ([`FromDay`]).
    From(FromDay, u8),
    /// At the lines' end, right of the newest bar: "ETH 6004.50", or "ETH = RTH 6004.50" for
    /// the studies in the mask when their values meet.
    End(u8),
    /// An off-scale line's band edge at the end, right of the newest bar: "ETH −1σ 6002.25"
    /// (`+k` / `-k`: the ±kσ edge).
    Band(i8),
}

const MAX_TAGS: usize = 256;
/// End tags one frame looks at: 8 lines, or their band edges.
const MAX_ENDS: usize = 16;

/// Where a line's newest minute must be to reach the newest bar (starting `newest`): that
/// bar's minute or the one before, whatever the bars' length (fast tick bars don't drop the
/// tags while the engine catches up with the minute).
fn reach_from(newest: lines_core::UtcMillis) -> lines_core::UtcMillis {
    let m = lines_core::UtcMillis::MINUTE;
    lines_core::UtcMillis(newest.0.div_euclid(m) * m - m)
}

/// Whether end tags may be drawn at all: the newest bar's slot (right edge `newest_right`)
/// is on a plot `pw` wide (panned back: none, and no gutter for them).
fn newest_on_plot(newest_right: Option<f32>, pw: f32) -> bool {
    newest_right.is_some_and(|n| n <= pw)
}

/// A line whose newest point (its minute's start `t`) is at or after `from` ([`reach_from`];
/// nothing known yet: assumed) reaches the newest bar: only then does it get an end tag.
/// Paint and measure both use this; the minute before counts so the tags (and their gutter)
/// stay while the engine catches up with a new bar.
fn reaches_newest(t: lines_core::UtcMillis, from: Option<lines_core::UtcMillis>) -> bool {
    from.is_none_or(|f| t.0.saturating_add(lines_core::UtcMillis::MINUTE) > f.0)
}

/// Whether a tag's box (window px) is partly or wholly under one of `covers` (the chart's
/// open panels): such a tag isn't drawn (never a partial tag).
fn covered(covers: &[Option<crate::layout::Rect>], x: f32, y: f32, w: f32, h: f32) -> bool {
    covers
        .iter()
        .flatten()
        .any(|r| x < r.x + r.w && r.x < x + w && y < r.y + r.h && r.y < y + h)
}

/// A line's end tag in a study's [`Study::end_tags`] bits: its value (`band` 0), or its `band`
/// edge (±1..3, off-scale lines).
fn end_bit(band: i8) -> u8 {
    match band {
        0 => 1 << 7,
        b => 1 << ((b.unsigned_abs() - 1) * 2 + u8::from(b < 0)),
    }
}

/// The names of the VWAPs in `mask`, in list order: "ETH", "ETH = RTH".
fn write_names<W: std::fmt::Write>(list: &[Study], mask: u8, out: &mut W) {
    for (n, k) in mask_iter(mask).enumerate() {
        if n > 0 {
            let _ = out.write_str(" = ");
        }
        if let Some(IndicatorConfig::Vwap { spec, .. }) = list.get(k).map(|s| &s.config) {
            let _ = out.write_str(spec.reset.label());
        }
    }
}

/// A band edge's name after the line's: " +1σ", " −2σ".
fn write_band<W: std::fmt::Write>(band: i8, out: &mut W) {
    let _ = write!(
        out,
        " {}{}σ",
        if band > 0 { '+' } else { '−' },
        band.unsigned_abs()
    );
}

/// How a late start's day is named, against the newest value's day (display zone).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum FromDay {
    /// The same day: the time alone.
    Same,
    /// Within the last six days: its weekday ("Mon").
    Weekday,
    /// Further back (a month's VWAP): its date ("11-02").
    Date,
}

fn from_day(
    from: lines_core::UtcMillis,
    newest: lines_core::UtcMillis,
    zone: &lines_core::display_tz::DisplayTz,
) -> FromDay {
    let day = |t| zone.local(t).0.div_euclid(86_400_000);
    match day(newest) - day(from) {
        0 => FromDay::Same,
        1..=6 => FromDay::Weekday,
        _ => FromDay::Date,
    }
}

/// A late VWAP's words: "from 11:34 CST" on the display zone's clock, with its label then;
/// dated when `from` isn't on the day of the newest value (`newest`): "from Mon 17:00 ET",
/// "from 11-02 17:00 ET" (a week's or month's VWAP that started late).
pub fn from_label(
    from: lines_core::UtcMillis,
    newest: lines_core::UtcMillis,
    zone: &lines_core::display_tz::DisplayTz,
) -> String {
    let mut s = String::new();
    write_from(from, from_day(from, newest, zone), zone, &mut s);
    s
}

fn write_from<W: std::fmt::Write>(
    from: lines_core::UtcMillis,
    day: FromDay,
    zone: &lines_core::display_tz::DisplayTz,
    out: &mut W,
) {
    write_when("from ", from, day, zone, out);
}

/// A closed session's VWAP's words: "to 16:00 ET" (its last minute's end), on the display
/// zone's clock as [`from_label`], dated against `newest` the same way.
pub fn to_label(
    end: lines_core::UtcMillis,
    newest: lines_core::UtcMillis,
    zone: &lines_core::display_tz::DisplayTz,
) -> String {
    let mut s = String::new();
    write_when("to ", end, from_day(end, newest, zone), zone, &mut s);
    s
}

fn write_when<W: std::fmt::Write>(
    word: &str,
    from: lines_core::UtcMillis,
    day: FromDay,
    zone: &lines_core::display_tz::DisplayTz,
    out: &mut W,
) {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let _ = out.write_str(word);
    match day {
        FromDay::Same => {}
        FromDay::Weekday => {
            let _ = out.write_str(WEEKDAYS[zone.local(from).weekday() as usize % 7]);
            let _ = out.write_char(' ');
        }
        FromDay::Date => {
            let (_, m, d, _, _, _) = zone.civil(from);
            let _ = write!(out, "{m:02}-{d:02} ");
        }
    }
    let _ = zone.write_hhmm(from, out);
    let _ = out.write_char(' ');
    let _ = zone.write_label(from, out);
}

/// A VWAP's off-scale marker: "ETH 6009.50 ▲" (direction by the arrow, not color).
fn write_edge<W: std::fmt::Write>(
    reset: Reset,
    band: i8,
    price: lines_core::Price,
    fmt: &lines_core::PriceFormat,
    above: bool,
    out: &mut W,
) {
    let _ = out.write_str(reset.label());
    if band != 0 {
        write_band(band, out);
    }
    let _ = out.write_str(" ");
    let _ = fmt.write(price, out);
    let _ = out.write_str(if above { " ▲" } else { " ▼" });
}

/// A thin edge's color (`c` at its opacity) raised, if need be, until it is 3:1 on the
/// chart's background (a light theme's band edges at their default 50 % aren't), never past
/// `c` itself. No allocation.
fn edge_color(theme: &Theme, c: Hsla) -> LineColor {
    use lines_core::style::{contrast, over};
    let (rgb, bg) = (crate::theme::rgb_of(c), crate::theme::rgb_of(theme.bg));
    let mut pct = (c.a.clamp(0.0, 1.0) * 100.0).round() as u8;
    if pct == 0 {
        return c; // switched off: stays off
    }
    while pct < 100 && contrast(over(rgb, pct, bg), bg) < 3.0 {
        pct = (pct + 5).min(100);
    }
    let mut out = c;
    out.a = f32::from(pct) / 100.0;
    out
}

/// A band fill's hairline edges before [`edge_color`]: the line's color `c` at up to 60 %,
/// scaled by the fill's opacity `pct` (a fill set to 0 % takes its edges with it).
fn hairline_base(c: Hsla, pct: u8) -> LineColor {
    c.opacity(0.6 * (f32::from(pct) / 25.0).min(1.0))
}

/// A VWAP's legend swatch: its line (3 px), between dashed band edges (`edge`, 1 px) when it
/// has bands, as the plot draws them.
fn vwap_swatch(line: (Hsla, LineType), edge: Option<(Hsla, LineType)>) -> gpui::Div {
    let u = crate::zoom::ui;
    // A 13 px stroke `h` high in `c`, solid, dashed or dotted as the plot draws it.
    let stroke = |(c, kind): (Hsla, LineType), h: f32| match kind {
        LineType::Solid => div().w(u(13.0)).h(u(h)).bg(c),
        LineType::Dashed => div()
            .flex()
            .flex_row()
            .gap(u(2.0))
            .children((0..3).map(move |_| div().w(u(3.0)).h(u(h)).bg(c))),
        _ => div()
            .flex()
            .flex_row()
            .gap(u(h.max(1.0)))
            .children((0..swatch_dots(h)).map(move |_| div().w(u(h.max(1.0))).h(u(h)).bg(c))),
    };
    let mut d = div().flex().flex_col().gap(u(1.5)).w(u(13.0));
    if let Some(e) = edge {
        d = d.child(stroke(e, 1.0));
    }
    d = d.child(stroke(line, 3.0));
    if let Some(e) = edge {
        d = d.child(stroke(e, 1.0));
    }
    d
}

/// A dotted swatch stroke `h` high: dots `h` wide with `h` between that fit its 13 px.
fn swatch_dots(h: f32) -> usize {
    let d = h.max(1.0);
    ((13.0 + d) / (2.0 * d)).floor() as usize
}

/// `v` on the tick grid (a display value, never an order price).
fn on_tick(v: f64, tick: i64) -> lines_core::Price {
    let tick = tick.max(1) as f64;
    lines_core::Price(((v / tick).round() * tick) as i64)
}

/// A tag's color bars: one per study it names, 3 px each with 1 px between.
fn bars_w(n: usize) -> f32 {
    n as f32 * 4.0 - 1.0
}

/// A tag's box: an opaque chip of the chart's background (a line or a wick never shows
/// through its words) with each named study's color as a bar on its left, in order.
fn tag_box(window: &mut Window, at: (f32, f32), w: f32, h: f32, theme: &Theme, cs: &[Hsla]) {
    let o = point(px(at.0), px(at.1));
    window.paint_quad(fill(Bounds::new(o, size(px(w), px(h))), theme.bg));
    for (i, c) in cs.iter().enumerate() {
        let x = at.0 + i as f32 * 4.0;
        window.paint_quad(fill(
            Bounds::new(point(px(x), px(at.1)), size(px(3.0), px(h))),
            *c,
        ));
    }
}

/// Where a late start's tag `size` (w, h) goes for a run beginning at `at` (its first bar's
/// left edge, the line's y) on a plot `plot` (w, h) px: right-aligned to the start tick in
/// clear space left of the run's first bar; else above the first bars it would span (and the
/// tick); else below them; never over a bar (`boxes`) or a tag already `placed` (x, y, w).
/// With no clear place: left of the tick (or above it), stepped down off the placed tags.
fn place_from(
    at: (f32, f32),
    size: (f32, f32),
    plot: (f32, f32),
    tick_h: f32,
    boxes: &[[f32; 4]],
    placed: &[(f32, f32, f32)],
) -> (f32, f32) {
    let ((x0, y0), (w, h), (pw, ph)) = (at, size, plot);
    let overlaps = |x: f32, y: f32| {
        placed
            .iter()
            .any(|&(bx, by, bw)| x < bx + bw && bx < x + w && y < by + h && by < y + h)
    };
    let free = |x: f32, y: f32| {
        x >= 0.0
            && x + w <= pw
            && y >= 0.0
            && y + h <= ph
            && !hits_bars(boxes, x, y, w, h)
            && !overlaps(x, y)
    };
    let left = (x0 - 3.0 - w, y0 - h * 0.5);
    let ax = x0.min(pw - w).max(0.0);
    let (top, bottom) = bars_span(boxes, ax, w).unwrap_or((y0, y0));
    let above = (ax, top.min(y0 - tick_h) - h - 2.0);
    let below = (ax, bottom.max(y0 + tick_h) + 2.0);
    let (x, mut y) = [left, above, below]
        .into_iter()
        .find(|&(x, y)| free(x, y))
        .unwrap_or((left.0.max(0.0), above.1.max(0.0)));
    while overlaps(x, y) && y < ph {
        y += h + 1.0;
    }
    (x, y)
}

/// The studies in `mask` (list indices), in list order (the legend's).
fn mask_iter(mask: u8) -> impl Iterator<Item = usize> {
    (0..8).filter(move |k| mask & (1 << k) != 0)
}

/// Whether the box `(x, y, w, h)` overlaps any of the bars' boxes (sorted by x).
fn hits_bars(boxes: &[[f32; 4]], x: f32, y: f32, w: f32, h: f32) -> bool {
    let i = boxes.partition_point(|b| b[1] < x);
    boxes[i..]
        .iter()
        .take_while(|b| b[0] <= x + w)
        .any(|b| b[2] < y + h && y < b[3])
}

/// The highest top and lowest bottom of the bars between `x` and `x + w`, if any.
fn bars_span(boxes: &[[f32; 4]], x: f32, w: f32) -> Option<(f32, f32)> {
    let i = boxes.partition_point(|b| b[1] < x);
    boxes[i..]
        .iter()
        .take_while(|b| b[0] <= x + w)
        .fold(None, |acc, b| {
            Some(acc.map_or((b[2], b[3]), |(t, u): (f32, f32)| {
                (t.min(b[2]), u.max(b[3]))
            }))
        })
}

/// A shaped tag, from the cache (shaped, and allocated, only on a miss).
fn shaped_tag(
    tags: &mut TagCache,
    key: (usize, Tag, i64),
    color: Hsla,
    label_px: f32,
    window: &mut Window,
    text: impl FnOnce(&mut String),
) -> ShapedLine {
    if tags.len() >= MAX_TAGS && !tags.contains_key(&key) {
        tags.clear();
    }
    if tags.get(&key).is_none_or(|(c, _)| *c != color) {
        let mut s = String::new();
        text(&mut s);
        let run = TextRun {
            len: s.len(),
            font: window.text_style().font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line =
            window
                .text_system()
                .shape_line(SharedString::from(s), px(label_px), &[run], None);
        tags.insert(key, (color, line));
    }
    tags[&key].1.clone()
}

fn bands_label(b: Bands) -> &'static str {
    match b.0 & 0b111 {
        0 => "",
        0b001 => "  ±1σ",
        0b011 => "  ±1σ ±2σ",
        0b111 => "  ±1σ ±2σ ±3σ",
        _ => "  bands",
    }
}

/// A study's looks from its configuration.
fn looks(c: &IndicatorConfig) -> Vec<Look> {
    match c {
        IndicatorConfig::Vwap { .. } => study_style::vwap_looks(c).to_vec(),
        IndicatorConfig::CumDelta { style, .. } => {
            study_style::read(style, &study_style::DELTA).to_vec()
        }
        IndicatorConfig::Volume { style, .. } => {
            study_style::read(style, &study_style::VOLUME).to_vec()
        }
        IndicatorConfig::External { style, .. } => match ext_def(c) {
            Some(def) => study_style::read_slice(style, crate::series_pane::part_defs(def)),
            None => Vec::new(),
        },
    }
}

/// An external series study's definition, from its registered source (`None`: the source
/// isn't registered in this build, or no longer offers it).
pub fn ext_def(c: &IndicatorConfig) -> Option<&'static SeriesStudyDef> {
    let IndicatorConfig::External { spec, .. } = c else {
        return None;
    };
    lines_feed::levels::registered()
        .into_iter()
        .find(|s| s.id() == spec.source)?
        .series_studies()
        .iter()
        .find(|d| d.id == spec.study)
}

/// How a lower pane's values print: quantities in the product's format, or plain numbers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ValueFormat {
    Qty,
    Number,
}

/// What a lower pane's title shows.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum LowerTitle {
    /// Cumulative delta: its reset, and whether its values are approximate.
    Delta(lines_indicators::DeltaReset, bool),
    /// Volume: the bar under the crosshair (else the newest), and what it shows.
    Volume {
        bar: Option<VolumeBar>,
        split: bool,
        average: Option<u16>,
    },
    /// An external series study: its lines' values at the crosshair (else the newest), and
    /// whether its data is live, stale or missing.
    Series {
        def: &'static SeriesStudyDef,
        values: [Option<f64>; crate::series_pane::MAX_LINES],
        status: ExtStatus,
        /// Each line's look as styled (the title's samples and value colors follow it).
        styles: [Option<lines_core::style::LineStyle>; crate::series_pane::MAX_LINES],
    },
}

/// A VWAP's line: its study color index and its style's line color (`Theme::study_line`
/// makes the opaque color its legend shows and its bands follow).
fn vwap_line(c: &IndicatorConfig, looks: &[Look]) -> (u8, Color) {
    let index = match c {
        IndicatorConfig::Vwap { color, .. } => *color,
        IndicatorConfig::CumDelta { .. }
        | IndicatorConfig::Volume { .. }
        | IndicatorConfig::External { .. } => 0,
    };
    (index, looks[study_style::VWAP_LINE].color())
}

impl Studies {
    pub fn new(configs: Vec<IndicatorConfig>) -> Studies {
        Studies {
            list: configs
                .into_iter()
                .map(|config| Study {
                    looks: looks(&config),
                    ext_def: ext_def(&config),
                    ext_gen: lines_feed::levels::serving_generation(),
                    config,
                    handle: None,
                    columns: Vec::new(),
                    candles: Vec::new(),
                    extent: None,
                    newest: None,
                    approx: false,
                    newest_time: None,
                    newest_from: None,
                    newest_vwap: None,
                    end_tags: None,
                    off_scale: false,
                    vol: VolumeFrame::default(),
                    ext: ExtFrame::default(),
                    error: None,
                    fill_cap: Cell::new(None),
                    overlay_cap: Cell::new(None),
                })
                .collect(),
            feed: None,
            tags: HashMap::new(),
            tags_epoch: (0, 0),
            bar_boxes: Vec::new(),
            end_need: 0.0,
            end_texts: String::new(),
            newest_right: None,
            reach_from: None,
            newest_bar: None,
            newest_on_plot: false,
            axis_marks: [None; MAX_AXIS_MARKS],
        }
    }

    /// Take `configs` that differ from this chart's only in their styles (`false`, nothing
    /// changed, when anything else differs: the caller builds new studies).
    pub fn restyle(&mut self, configs: &[IndicatorConfig]) -> bool {
        if configs.len() != self.list.len()
            || !self
                .list
                .iter()
                .zip(configs)
                .all(|(s, c)| s.config.same_but_style(c))
        {
            return false;
        }
        for (s, c) in self.list.iter_mut().zip(configs) {
            if s.config != *c {
                s.config = c.clone();
                s.looks = looks(c);
            }
        }
        true
    }

    /// What the workspace saves for this chart.
    pub fn configs(&self) -> Vec<IndicatorConfig> {
        self.list.iter().map(|s| s.config.clone()).collect()
    }

    /// (Re)subscribe every shown study on `feed` (a new chart, or the chart switched symbol).
    /// A hidden study holds no engine node: it is neither computed nor drawn until shown.
    pub fn attach(&mut self, feed: FeedInfo) {
        self.feed = Some(feed);
        for s in &mut self.list {
            let hidden = s.config.display().hidden;
            s.handle = if hidden {
                None
            } else {
                s.config
                    .spec(feed.period_ms)
                    .map(|spec| engine().acquire(feed, spec))
            };
        }
    }

    /// A shown external series study is live: its STALE turns on the clock alone, so the
    /// chart repaints on a timer while this holds (`ChartView::changed`).
    pub fn series_live(&self) -> bool {
        self.list.iter().any(|s| {
            s.ext_def.is_some()
                && !s.config.display().hidden
                && s.ext.status == crate::series_pane::ExtStatus::Live
        })
    }

    /// Sum of the studies' output revisions: changes when any study has new values.
    pub fn revision(&self) -> u64 {
        self.list
            .iter()
            .filter_map(|s| s.handle.as_ref())
            .map(|h| h.cell().revision())
            .fold(0u64, |a, r| a.wrapping_add(r))
            .wrapping_add(self.list.len() as u64)
    }

    /// Under the store lock, after the viewport update: build this frame's columns.
    /// `bars`: the chart's bars (its timeframe's); the engine's points are per 1-minute bar.
    /// `fp`: the store's per-minute footprints (Volume's buy/sell split on time bars).
    pub fn prepare(
        &mut self,
        bars: BarsView<'_>,
        fp: &Footprint,
        levels: &crate::levels_layer::LevelsLayer,
        vp: &Viewport,
        max_columns: usize,
    ) {
        // The newest bar (end tags go only to lines that reach it).
        self.newest_right = bars
            .len()
            .checked_sub(1)
            .map(|i| vp.index_to_x(i as f64) as f32 + (vp.bar_px * 0.5) as f32);
        self.newest_on_plot = newest_on_plot(self.newest_right, vp.width as f32);
        self.newest_bar = bars.bars.last().map(|b| b.time);
        self.reach_from = self.newest_bar.map(reach_from);
        // The visible bars' boxes, for placing late-start tags clear of them (shown VWAPs only;
        // the buffer grows only when the plot grows wider than ever before).
        self.bar_boxes.clear();
        if self
            .list
            .iter()
            .any(|s| matches!(&s.config, IndicatorConfig::Vwap { display, .. } if !display.hidden))
        {
            if self.bar_boxes.capacity() < max_columns {
                self.bar_boxes.reserve(max_columns);
            }
            let half = (vp.bar_px * 0.5) as f32;
            for i in vp.visible_range(bars.len()) {
                if self.bar_boxes.len() == self.bar_boxes.capacity() {
                    break;
                }
                let b = &bars.bars[i];
                let x = vp.index_to_x(i as f64) as f32;
                self.bar_boxes.push([
                    x - half,
                    x + half,
                    vp.price_to_y(b.high.0 as f64) as f32,
                    vp.price_to_y(b.low.0 as f64) as f32,
                ]);
            }
        }
        for s in &mut self.list {
            if let IndicatorConfig::External { spec, display, .. } = &s.config {
                if display.hidden {
                    continue;
                }
                let gen_now = lines_feed::levels::serving_generation();
                if s.ext_def.is_none() && s.ext_gen != gen_now {
                    // The registered sources changed (one registered after this chart was
                    // made): look again, once per change.
                    s.ext_gen = gen_now;
                    s.ext_def = ext_def(&s.config);
                    s.looks = looks(&s.config);
                }
                let Some(def) = s.ext_def else {
                    s.ext.status = ExtStatus::Missing("source not available");
                    continue;
                };
                s.ext.reserve(def.lines.len(), max_columns * 4 + 64);
                let axis = lines_core::drawings::TimeAxis::new(bars);
                s.ext
                    .build(def, levels.source_view(&spec.source), &axis, vp);
                continue;
            }
            if let IndicatorConfig::Volume { spec, display, .. } = &s.config {
                // A hidden Volume does no work (its buffer included).
                if display.hidden {
                    continue;
                }
                // Only when the plot grows wider than ever before (as the candle scene).
                s.vol.reserve(max_columns);
                let opts = VolumeOptions {
                    split: spec.split,
                    average: spec.average.then(|| spec.window()),
                };
                s.vol.build(fp, bars, vp.visible_range(bars.len()), opts);
                continue;
            }
            let delta = s.config.in_subpane();
            let buf_len = if delta {
                s.candles.capacity()
            } else {
                s.columns.capacity()
            };
            if buf_len < max_columns {
                // Only when the plot grows wider than ever before (as the candle scene).
                if delta {
                    s.candles.reserve(max_columns - s.candles.len());
                } else {
                    s.columns.reserve(max_columns - s.columns.len());
                }
            }
            let Some(h) = &s.handle else { continue };
            let Some(out) = h.cell().try_read() else {
                continue; // the engine is writing: keep last frame's columns
            };
            match &s.config {
                IndicatorConfig::Vwap { spec, .. } => {
                    build_columns(&out.points, bars, vp, spec.bands, &mut s.columns);
                    s.approx = out.points.last().is_some_and(|p| p.approx);
                    s.newest_time = out.points.last().map(|p| p.time);
                    s.newest_from = out.points.last().and_then(|p| p.from);
                    s.newest_vwap = out.points.last().map(|p| p.vwap);
                }
                IndicatorConfig::CumDelta { .. } => {
                    let range = vp.visible_range(bars.len());
                    s.extent = build_delta_candles(&out.candles, bars, range, &mut s.candles);
                    s.newest = out.candles.last().map(|p| p.close);
                    s.approx = s.candles.iter().any(|c| c.approx);
                }
                IndicatorConfig::Volume { .. } | IndicatorConfig::External { .. } => {} // above
            }
            if s.error.is_none()
                && let Some(e) = &out.error
            {
                s.error = Some(e.clone()); // once: the spec can't be built
            }
        }
    }

    /// Inside the plot's content mask. `origin` is the plot's top-left.
    ///
    /// A smooth line through the bar centres, and band fills as polygons between the ±kσ
    /// edges, broken at reset periods (and where bars have no value). Paths are anti-aliased
    /// by GPUI (MSAA) and tessellated by it each frame (that allocation is GPUI's, like text
    /// shaping); the columns they follow are built without allocating.
    ///
    /// Two layers: [`Studies::paint_back`] (band fills, under the candles, so a heavy fill
    /// never covers a bar) and this (band edges and the line, over them).
    ///
    /// Then, unless study labels are hidden (`labels`), the VWAPs' tags
    /// ([`Studies::paint_tags`]): `fmt` prints their values, `label_px` is the chart's label
    /// size; `axis` is the price axis column's width and the last-price tag's span on it
    /// (top, bottom; plot px), for off-scale markers moved there. Returns whether the end
    /// tags changed (the chart renders again: their room is measured in render).
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &mut self,
        origin: (f32, f32),
        vp: &Viewport,
        theme: &Theme,
        fmt: &lines_core::PriceFormat,
        label_px: f32,
        labels: bool,
        room: TagRoom<'_>,
        covers: &[Option<crate::layout::Rect>],
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        self.axis_marks = [None; MAX_AXIS_MARKS];
        self.paint_layer(origin, theme, window, false);
        labels && self.paint_tags(origin, vp, theme, fmt, label_px, room, covers, window, cx)
    }

    /// After the price axis's labels and last-price tag (outside the plot's mask): the tags
    /// this frame's paint moved out of the plot (off-scale markers and end tags, in the price
    /// axis or the label column), whole, on `label_bg`.
    pub fn paint_axis_marks(
        &self,
        origin: (f32, f32),
        theme: &Theme,
        label_px: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        let line_h = px((label_px * 1.4).round());
        for m in self.axis_marks.iter().flatten() {
            let Some((_, line)) = self.tags.get(&m.key) else {
                continue;
            };
            let [x, y, w, h] = m.at;
            let (x, y) = (origin.0 + x, origin.1 + y);
            window.paint_quad(fill(
                Bounds::new(point(px(x), px(y)), size(px(w), px(h))),
                theme.label_bg,
            ));
            window.paint_quad(fill(
                Bounds::new(point(px(x), px(y)), size(px(3.0), px(h))),
                m.color,
            ));
            let o = point(px(x + bars_w(1) + 4.0), px(y));
            let _ = line.paint(o, line_h, TextAlign::Left, None, window, cx);
            crate::layout_rec::line(window, line, o, line_h, "");
        }
    }

    /// Whether a tag moved out of the plot covers plot px `top..bottom` in the price axis
    /// (whose left edge is at plot x `axis_x`; a tag in the label column covers none of it): a
    /// price label it would cover isn't drawn.
    pub fn axis_marks_cover(&self, top: f32, bottom: f32, axis_x: f32) -> bool {
        self.axis_marks
            .iter()
            .flatten()
            .any(|m| m.at[0] + m.at[2] > axis_x && top < m.at[1] + m.at[3] && m.at[1] < bottom)
    }

    /// The VWAPs' tags, over their lines, each on an opaque chip of the chart's background
    /// with its studies' colors as bars and their names in its words (never color alone):
    /// - a line whose newest visible value is above or below the plot gets a marker at that
    ///   edge, at its end, with its value and an arrow ("ETH 6009.50 ▲"): price is never read
    ///   against a VWAP that isn't on the scale. The scale doesn't stretch to take it in (a
    ///   far week's VWAP would squash the bars);
    /// - where a late run begins on the plot, a tick across the line, and its time in clear
    ///   space left of the run's first bar ("ETH from 12:34 ET"; runs that begin together
    ///   share one: "ETH = RTH from 12:34 ET"), else above or below its first bars;
    /// - at the lines' end (where the chart's study labels go, [`TagRoom`]: right of the
    ///   newest bar, else pinned to the plot's edge; or a tag in the price axis or the label
    ///   column at its price), the value: "ETH 6004.50", or "ETH = RTH 6004.50" where values meet; for a line that is
    ///   off the scale, each band edge on the plot instead ("ETH −1σ 6002.25"), so an edge is
    ///   never read as the VWAP. Only for a line that reaches the newest bar: an RTH VWAP on
    ///   an evening chart (or any line panned back) ends mid-plot and gets none.
    ///
    /// The gutter end tags want is measured in render ([`Self::measure_end_tags`]); this notes
    /// which end tags each line got, and returns whether that changed.
    #[allow(clippy::too_many_arguments)]
    fn paint_tags(
        &mut self,
        origin: (f32, f32),
        vp: &Viewport,
        theme: &Theme,
        fmt: &lines_core::PriceFormat,
        label_px: f32,
        room: TagRoom<'_>,
        covers: &[Option<crate::layout::Rect>],
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let (axis_w, blocked) = (room.axis_w, room.blocked);
        // Where end tags go: a column placement without a column draws them on the chart.
        let place = match room.place {
            crate::levels_layer::LabelPlace::Column if room.column <= 0.0 => {
                crate::levels_layer::LabelPlace::Chart
            }
            p => p,
        };
        let mut n_axis = 0;
        let mut axis_marks = [None; MAX_AXIS_MARKS];
        self.sync_tags((lines_core::display_tz::epoch(), crate::zoom::generation()));
        let (ox, oy) = origin;
        let (pw, ph) = (vp.width as f32, vp.height as f32);
        let pad = 4.0;
        let h = (label_px * 1.4).round();
        let line_h = px(h);
        let tick_h = (h * 0.5).round();
        let zone = lines_core::display_tz::current();
        let words = theme.text_main;
        let newest_right = self.newest_right;
        let reach_from = self.reach_from;
        let on_plot = self.newest_on_plot;
        // A tick in plot px: an off-scale line comes back once inside by more than this.
        let tick_px = (vp.price_to_y(0.0) - vp.price_to_y(fmt.tick.max(1) as f64)).abs() as f32;
        let mut off_now = [None::<bool>; 8];
        // At most 8 VWAPs looked at (fixed arrays: no allocation). Each edge's markers stack
        // inward.
        let mut edge_rows = [0u8; 2];
        // End tags on the plot: (study, y, x after its last bar, value on the tick grid,
        // band: 0 the line, ±k its ±kσ edge).
        let mut ends: [(usize, f32, f32, i64, i8); MAX_ENDS] =
            [(usize::MAX, 0.0, 0.0, 0, 0); MAX_ENDS];
        let mut n_ends = 0;
        // Late starts on the plot: (studies, x, y, from); runs starting on the same bar at
        // the same time share one tag.
        let mut starts: [(u8, f32, f32, lines_core::UtcMillis); 8] =
            [(0, 0.0, 0.0, lines_core::UtcMillis(0)); 8];
        let mut n_starts = 0;
        let mut colors = [Hsla::default(); 8];
        let mut drawn = [None::<u8>; 8];
        let mut newest = lines_core::UtcMillis(0);
        let tags = &mut self.tags;
        let boxes = &self.bar_boxes;
        for (k, s) in self.list.iter().enumerate().take(8) {
            let IndicatorConfig::Vwap { spec, display, .. } = &s.config else {
                continue;
            };
            if display.hidden {
                continue;
            }
            let Some(last) = s.columns.last() else {
                continue;
            };
            newest = newest.max(s.newest_time.unwrap_or_default());
            let (index, line_c) = vwap_line(&s.config, &s.looks);
            let c = theme.study_line(index, line_c);
            // The bar shows the line's color (the legend's swatch).
            colors[k] = c;
            for col in &s.columns {
                let Some(from) = col.from.filter(|_| col.starts) else {
                    continue;
                };
                if !(0.0..ph).contains(&col.y) || col.left < 0.0 || col.left >= pw {
                    continue;
                }
                // The tick across the line where the run begins.
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(ox + col.left - 1.0), px(oy + col.y - tick_h)),
                        size(px(2.0), px(tick_h * 2.0)),
                    ),
                    c,
                ));
                match starts[..n_starts]
                    .iter_mut()
                    .find(|e| e.3 == from && (e.1 - col.left).abs() < 0.5)
                {
                    Some(e) => e.0 |= 1 << k,
                    None if n_starts < 8 => {
                        starts[n_starts] = (1 << k, col.left, col.y, from);
                        n_starts += 1;
                    }
                    None => {}
                }
            }
            let reaches = s.newest_time.is_some_and(|t| reaches_newest(t, reach_from));
            let at_newest = on_plot && reaches;
            // End tags go right of the newest bar, even while the line's last value is still
            // in the bar before it.
            let x_end = if at_newest {
                newest_right.unwrap_or(last.right).max(last.right).min(pw)
            } else {
                last.right.min(pw)
            };
            let price = on_tick(last.vwap, fmt.tick);
            let mut push_end = |y: f32, v: i64, band: i8| {
                if n_ends < MAX_ENDS {
                    ends[n_ends] = (k, y, x_end, v, band);
                    n_ends += 1;
                    true
                } else {
                    false
                }
            };
            let mut bits = 0u8;
            let off = if s.off_scale {
                last.y < tick_px || last.y > ph - tick_px
            } else {
                last.y < 0.0 || last.y > ph
            };
            off_now[k] = Some(off);
            let line_above = last.y < ph * 0.5;
            // Edge markers: (above, band: 0 the line or ±k its edge, value).
            let mut markers = [(false, 0i8, 0i64); 3];
            let mut n_markers = 0;
            if off {
                markers[0] = (line_above, 0, price.0);
                n_markers = 1;
            }
            // A band edge off the plot while its fill shows: the innermost one each side gets
            // a marker too ("RTH +1σ 6020.25 ▲"); the side the line is off has the line's.
            let fill_shows = s.looks[study_style::VWAP_FILL].opacity() > 0;
            for (side, sign) in [(true, 1i8), (false, -1i8)] {
                if !fill_shows || (off && line_above == side) {
                    continue;
                }
                let edge = (1..=3u8).filter(|&b| spec.bands.has(b)).find(|&b| {
                    let i = usize::from(b - 1);
                    let y = if side { last.up[i] } else { last.down[i] };
                    let (lo, hi) = (last.up[i].min(last.down[i]), last.up[i].max(last.down[i]));
                    let fill_on = hi >= 0.0 && lo <= ph;
                    let out = if side { y < 0.0 } else { y > ph };
                    out && fill_on
                });
                if let Some(b) = edge {
                    let d = last.sd * f64::from(b) * f64::from(sign);
                    let v = on_tick(last.vwap + d, fmt.tick).0;
                    markers[n_markers] = (side, sign * b as i8, v);
                    n_markers += 1;
                }
            }
            for &(above, band, v) in &markers[..n_markers] {
                let line = shaped_tag(
                    tags,
                    (k, Tag::Edge(above, band), v),
                    words,
                    label_px,
                    window,
                    |o| write_edge(spec.reset, band, lines_core::Price(v), fmt, above, o),
                );
                let row = &mut edge_rows[usize::from(!above)];
                let w = f32::from(line.width) + pad * 2.0 + bars_w(1);
                let step = f32::from(*row) * (h + 2.0);
                let y = if above {
                    2.0 + step
                } else {
                    ph - h - 2.0 - step
                };
                *row = row.saturating_add(1);
                // A line that runs on to the newest bar: where the chart's study labels go
                // (a tag in the price axis or the label column at that edge, on the nearest
                // free row) when it fits there.
                if reaches
                    && n_axis < MAX_AXIS_MARKS
                    && let Some((x, rows)) = out_of_plot(&room, place, pw, w)
                {
                    let (taken, n_taken) = taken_tops(&axis_marks[..n_axis]);
                    if let Some(ty) = free_row(y, h, ph, rows, &taken[..n_taken])
                        && !covered(covers, ox + x, oy + ty, w, h)
                    {
                        axis_marks[n_axis] = Some(AxisMark {
                            key: (k, Tag::Edge(above, band), v),
                            color: colors[k],
                            at: [x, ty, w, h],
                        });
                        n_axis += 1;
                        continue;
                    }
                }
                // Else right of the newest bar where there is room; a line that runs on to the
                // newest bar (on the plot or panned past), else in the price axis column at
                // that edge (never over the bars, nor the last-price tag); a line that ends
                // mid-plot, at its end where no bar is there.
                let x = if at_newest && x_end + 2.0 + w <= pw {
                    x_end + 2.0
                } else if reaches {
                    // Clear of the last-price tag and the drawings' and levels' price tags (a
                    // half-covered number reads as another price): on the nearest free row at
                    // that edge, else not at all.
                    let (taken, n_taken) = taken_tops(&axis_marks[..n_axis]);
                    if let Some(x) = axis_mark_x(w, x_end + 2.0, pw, axis_w)
                        && n_axis < MAX_AXIS_MARKS
                        && let Some(ty) = free_row(y, h, ph, blocked, &taken[..n_taken])
                        && !covered(covers, ox + x, oy + ty, w, h)
                    {
                        axis_marks[n_axis] = Some(AxisMark {
                            key: (k, Tag::Edge(above, band), v),
                            color: colors[k],
                            at: [x, ty, w, h],
                        });
                        n_axis += 1;
                    }
                    continue;
                } else {
                    (x_end - w).clamp(0.0, (pw - w).max(0.0))
                };
                // Never over the bars (its legend row still says where the line ended).
                if hits_bars(boxes, x, y, w, h) {
                    continue;
                }
                if covered(covers, ox + x, oy + y, w, h) {
                    continue;
                }
                tag_box(window, (ox + x, oy + y), w, h, theme, &colors[k..=k]);
                let o = point(px(ox + x + bars_w(1) + pad), px(oy + y));
                let _ = line.paint(o, line_h, TextAlign::Left, None, window, cx);
                crate::layout_rec::line(window, &line, o, line_h, "");
            }
            if off {
                // The band edges still on the plot say which they are (the only line of the
                // study's color in view is never read as its VWAP).
                if at_newest {
                    for b in 1..=3u8 {
                        if !spec.bands.has(b) {
                            continue;
                        }
                        let i = usize::from(b - 1);
                        let d = last.sd * f64::from(b);
                        for (y, v, sign) in [
                            (last.up[i], last.vwap + d, 1i8),
                            (last.down[i], last.vwap - d, -1i8),
                        ] {
                            if (0.0..=ph).contains(&y)
                                && push_end(y, on_tick(v, fmt.tick).0, sign * b as i8)
                            {
                                bits |= end_bit(sign * b as i8);
                            }
                        }
                    }
                }
            } else if at_newest && push_end(last.y.clamp(0.0, ph), price.0, 0) {
                bits |= end_bit(0);
            }
            drawn[k] = Some(bits);
        }
        // Late-start tags: right-aligned to the tick in clear space left of the run's first
        // bar; else above, else below the first bars it would span; clear of each other.
        let mut placed: [(f32, f32, f32); 8] = [(0.0, 0.0, 0.0); 8];
        let mut n_placed = 0;
        let starts = &mut starts[..n_starts];
        starts.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.2.total_cmp(&b.2)));
        for &(mask, x0, y0, from) in starts.iter() {
            let first = mask.trailing_zeros() as usize;
            let day = from_day(from, newest, &zone);
            let list = &self.list;
            let line = shaped_tag(
                tags,
                (first, Tag::From(day, mask), from.0),
                words,
                label_px,
                window,
                |o| {
                    write_names(list, mask, o);
                    o.push(' ');
                    write_from(from, day, &zone, o);
                },
            );
            let n = mask.count_ones() as usize;
            let w = f32::from(line.width) + pad * 2.0 + bars_w(n);
            let (x, y) = place_from(
                (x0, y0),
                (w, h),
                (pw, ph),
                tick_h,
                &self.bar_boxes,
                &placed[..n_placed],
            );
            if n_placed < placed.len() {
                placed[n_placed] = (x, y, w);
                n_placed += 1;
            }
            let mut cs = [Hsla::default(); 8];
            for (i, k) in mask_iter(mask).enumerate() {
                cs[i] = colors[k];
            }
            if covered(covers, ox + x, oy + y, w, h) {
                continue;
            }
            tag_box(window, (ox + x, oy + y), w, h, theme, &cs[..n]);
            let o = point(px(ox + x + bars_w(n) + pad), px(oy + y));
            let _ = line.paint(o, line_h, TextAlign::Left, None, window, cx);
            crate::layout_rec::line(window, &line, o, line_h, "");
        }
        // End tags: lines whose values meet share one (band edges never merge); tags that
        // would overlap stack down from the top one.
        let ends = &mut ends[..n_ends];
        ends.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        // (studies, y, x, value, band), merged by value.
        let mut groups: [(u8, f32, f32, i64, i8); MAX_ENDS] = [(0, 0.0, 0.0, 0, 0); MAX_ENDS];
        let mut n_groups = 0;
        for &(k, y, x, v, band) in ends.iter() {
            match groups[..n_groups]
                .iter_mut()
                .find(|g| band == 0 && g.4 == 0 && g.3 == v)
            {
                Some(g) => {
                    g.0 |= 1 << k;
                    g.2 = g.2.max(x);
                }
                None => {
                    groups[n_groups] = (1 << k, y, x, v, band);
                    n_groups += 1;
                }
            }
        }
        let mut next_top = f32::MIN;
        for &(mask, y, x_end, v, band) in &groups[..n_groups] {
            let first = mask.trailing_zeros() as usize;
            let list = &self.list;
            let price = lines_core::Price(v);
            let tag = if band == 0 {
                Tag::End(mask)
            } else {
                Tag::Band(band)
            };
            let line = shaped_tag(tags, (first, tag, v), words, label_px, window, |o| {
                write_names(list, mask, o);
                if band != 0 {
                    write_band(band, o);
                }
                o.push(' ');
                let _ = fmt.write(price, o);
            });
            let n = mask.count_ones() as usize;
            let w = f32::from(line.width) + pad * 2.0 + bars_w(n);
            // Outside the plot (the chart's study label placement): a tag in the price axis
            // at its price, or one in the label column, on its own row (never over the
            // last-price tag, a drawing's or a level's tag, or another tag there).
            if place != crate::levels_layer::LabelPlace::Chart {
                let Some((x, rows)) = out_of_plot(&room, place, pw, w) else {
                    continue;
                };
                if n_axis >= MAX_AXIS_MARKS {
                    continue;
                }
                let (taken, n_taken) = taken_tops(&axis_marks[..n_axis]);
                let Some(ty) = free_row(y - h * 0.5, h, ph, rows, &taken[..n_taken]) else {
                    continue;
                };
                if covered(covers, ox + x, oy + ty, w, h) {
                    continue;
                }
                axis_marks[n_axis] = Some(AxisMark {
                    key: (first, tag, v),
                    color: colors[first],
                    at: [x, ty, w, h],
                });
                n_axis += 1;
                continue;
            }
            // On the chart: just right of the newest bar, else pinned to the plot's edge
            // (over the bars, on its chip), as the chart's other study labels are.
            let x = if x_end + 2.0 + w <= pw {
                x_end + 2.0
            } else {
                (pw - w - 6.0).max(0.0)
            };
            let ty = (y - h * 0.5).max(next_top).clamp(0.0, (ph - h).max(0.0));
            next_top = ty + h + 1.0;
            // Never a tag partly under an open panel.
            if covered(covers, ox + x, oy + ty, w, h) {
                continue;
            }
            let mut cs = [Hsla::default(); 8];
            for (i, k) in mask_iter(mask).enumerate() {
                cs[i] = colors[k];
            }
            tag_box(window, (ox + x, oy + ty), w, h, theme, &cs[..n]);
            let o = point(px(ox + x + bars_w(n) + pad), px(oy + ty));
            let _ = line.paint(o, line_h, TextAlign::Left, None, window, cx);
            crate::layout_rec::line(window, &line, o, line_h, "");
        }
        self.axis_marks = axis_marks;
        // A line whose end tags changed (off the plot or back, an edge in or out): so does
        // their room.
        let mut moved = false;
        for (s, o) in self.list.iter_mut().zip(off_now) {
            if let Some(o) = o {
                s.off_scale = o;
            }
        }
        for (s, d) in self.list.iter_mut().zip(drawn) {
            if d.is_some() && s.end_tags != d {
                s.end_tags = d;
                moved = true;
            }
        }
        moved
    }

    /// Shaped tags are dropped when the display zone or the UI zoom (their text size) moved.
    fn sync_tags(&mut self, epoch: (u64, u32)) {
        if epoch != self.tags_epoch {
            self.tags.clear();
            self.tags_epoch = epoch;
        }
    }

    /// The width the VWAPs' end tags want right of the newest bar: the chart adds it to the
    /// labels' gutter. Zero while no end tag is drawn (an RTH VWAP on an evening chart).
    pub fn end_tags_need(&self) -> f32 {
        self.end_need
    }

    /// In render (a hidden pane does none): the end tags' texts from the shown VWAPs' newest
    /// values, as the last paint drew them ("ETH 6004.50", "ETH = RTH 6004.50" where values
    /// meet; an off-scale line's band edges on the plot, "ETH −1σ 6002.25"; a line not painted
    /// yet every way), for
    /// the lines that reach the newest bar as paint decides it ([`reaches_newest`]: a line
    /// that ends before the bar before the newest has no end tag), measured
    /// only when they change; the widest is [`Self::end_tags_need`].
    pub fn measure_end_tags(
        &mut self,
        fmt: &lines_core::PriceFormat,
        label_px: f32,
        theme: &Theme,
        window: &mut Window,
    ) {
        // (studies, value, band): lines merged by value, band edges one each.
        let mut groups: Vec<(u8, i64, i8)> = Vec::new();
        for (k, s) in self.list.iter().enumerate().take(8) {
            let IndicatorConfig::Vwap { spec, display, .. } = &s.config else {
                continue;
            };
            if display.hidden {
                continue;
            }
            let p = s
                .handle
                .as_ref()
                .and_then(|h| h.cell().try_read())
                .map(|o| o.points.last().map(|p| (p.vwap, p.sd, p.time)))
                .unwrap_or_else(|| s.newest_vwap.zip(s.newest_time).map(|(v, t)| (v, 0.0, t)));
            let Some((v, sd, t)) = p else { continue };
            if !self.newest_on_plot || !reaches_newest(t, self.reach_from) {
                continue; // ends before the newest bar, or panned back: no end tag
            }
            let wants = |band: i8| s.end_tags.is_none_or(|b| b & end_bit(band) != 0);
            if wants(0) {
                let v = on_tick(v, fmt.tick).0;
                match groups.iter_mut().find(|g| g.2 == 0 && g.1 == v) {
                    Some(g) => g.0 |= 1 << k,
                    None => groups.push((1 << k, v, 0)),
                }
            }
            for b in (1..=3u8).filter(|&b| spec.bands.has(b)) {
                let d = sd * f64::from(b);
                for (v, band) in [(v + d, b as i8), (v - d, -(b as i8))] {
                    if wants(band) {
                        groups.push((1 << k, on_tick(v, fmt.tick).0, band));
                    }
                }
            }
        }
        let mut texts = String::new();
        for &(mask, v, band) in &groups {
            write_names(&self.list, mask, &mut texts);
            if band != 0 {
                write_band(band, &mut texts);
            }
            texts.push(' ');
            let _ = fmt.write(lines_core::Price(v), &mut texts);
            texts.push('\n');
        }
        if texts == self.end_texts {
            return;
        }
        let mut need = 0.0f32;
        for (line, &(mask, _, _)) in texts.lines().zip(&groups) {
            let run = TextRun {
                len: line.len(),
                font: window.text_style().font(),
                color: theme.text_main,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let w = window
                .text_system()
                .shape_line(
                    SharedString::from(line.to_owned()),
                    px(label_px),
                    &[run],
                    None,
                )
                .width;
            // As paint sizes it (its color bars and padding), and 2 px off the last bar.
            need = need.max(f32::from(w) + 8.0 + bars_w(mask.count_ones() as usize) + 4.0);
        }
        self.end_need = need;
        self.end_texts = texts;
    }

    /// The band fills and Volume's overlay bars, painted before the candles (with the level
    /// zones).
    pub fn paint_back(
        &self,
        origin: (f32, f32),
        vp: &Viewport,
        theme: &Theme,
        window: &mut Window,
    ) {
        self.paint_layer(origin, theme, window, true);
        self.paint_overlay(origin, vp, theme, window);
    }

    /// Volume as an overlay: short bars along the plot's bottom [`OVERLAY_SHARE`] (`back`:
    /// under the candles, translucent), its average line over the candles.
    fn paint_overlay(&self, origin: (f32, f32), vp: &Viewport, theme: &Theme, window: &mut Window) {
        for s in &self.list {
            let IndicatorConfig::Volume { spec, display, .. } = &s.config else {
                continue;
            };
            if display.hidden || !spec.overlay {
                continue;
            }
            let ph = vp.height as f32;
            let band = (ph * OVERLAY_SHARE).round();
            let scale = Scale::from_zero(s.vol.max, band);
            let at = (origin.0, origin.1 + ph - band);
            // All of it under the candles: faint fills, with each bar's top edge and hollow
            // outlines in the opaque colors (3:1 on the chart), so the bars read as data
            // without competing with price; the average line under the candles too.
            {
                let cols = volume_colors(s, theme);
                paint_volume(
                    s,
                    spec,
                    at,
                    &scale,
                    vp,
                    s.overlay_colors(theme, cols),
                    Some(cols),
                    theme.hollow,
                    window,
                );
                paint_average(s, spec, at, &scale, vp, theme, window);
            }
        }
    }

    fn paint_layer(&self, origin: (f32, f32), theme: &Theme, window: &mut Window, back: bool) {
        let (ox, oy) = (origin.0, origin.1);
        let at = |x: f32, y: f32| point(px(ox + x), px(oy + y));
        for s in &self.list {
            let IndicatorConfig::Vwap { spec, display, .. } = &s.config else {
                continue; // sub-pane studies paint in `paint_subpane`
            };
            if display.hidden {
                continue;
            }
            let line = s.looks[study_style::VWAP_LINE].line();
            let bands = s.looks[study_style::VWAP_BANDS];
            let band_line = bands.line();
            let fill = s.looks[study_style::VWAP_FILL];
            let (index, line_c) = vwap_line(&s.config, &s.looks);
            let base = theme.study_line(index, line_c);
            let line_px = study_style::vwap_px(line.width);
            let thick = px(line_px);
            let line_color = theme.part_color(Color::Own, base, line.opacity);
            // Held back so the candles over it keep 3:1 (as many layers as bands nest).
            let fill_color = s.fill_under_candles(
                theme,
                theme.part_color(
                    fill.color(),
                    base.opacity(study_style::VWAP_FILL_ALPHA),
                    fill.opacity(),
                ),
                band_layers(spec.bands),
            );
            let band_px = study_style::vwap_px(band_line.width);
            let band_color = edge_color(
                theme,
                theme.part_color(band_line.color, base, band_line.opacity),
            );
            let line_dash = dash(line.line, line_px);
            let band_dash = dash(band_line.line, band_px);
            // The fill's hairline edges: one device pixel, the line's color at up to 60 %,
            // scaled by the fill's opacity (a fill set to 0 % takes its edges with it).
            let hair_px = 1.0 / window.scale_factor();
            let hairline = edge_color(theme, hairline_base(base, fill.opacity()));
            // Dashed, as the styled edges are by default: an edge is never read as the line.
            let hair_dash = dash(LineType::Dashed, hair_px);
            let cols = &s.columns;
            // Runs of columns that continue one another (same period, adjacent bars, same
            // exactness): each run is one path.
            let mut start = 0;
            while start < cols.len() {
                let mut end = start + 1;
                while end < cols.len() && cols[end].joins && cols[end].approx == cols[start].approx
                {
                    end += 1;
                }
                let run = &cols[start..end];
                let mid = |c: &Column| (c.left + c.right) * 0.5;
                // A single bar's value spans its slot; longer runs go centre to centre.
                let xs = |i: usize, c: &Column| {
                    if run.len() == 1 {
                        if i == 0 { c.left } else { c.right }
                    } else {
                        mid(c)
                    }
                };
                let pts: &[Column] = run;
                let n = if run.len() == 1 { 2 } else { run.len() };
                let col = |i: usize| &pts[i.min(pts.len() - 1)];
                // Bands: widest first, translucent, nested.
                for k in (1..=3u8).rev() {
                    if !spec.bands.has(k) || (!back && !bands.shown) {
                        continue;
                    }
                    let kk = k as usize - 1;
                    if back && !bands.shown && hairline.a > 0.0 {
                        // The fill's own thin edges (the styled edges are off): where each
                        // band ends reads, from the line's first bar. Under the candles.
                        for edge in [true, false] {
                            let y = |c: &Column| if edge { c.up[kk] } else { c.down[kk] };
                            let mut b = stroke(px(hair_px), &hair_dash);
                            b.move_to(at(xs(0, col(0)), y(col(0))));
                            for i in 1..n {
                                b.line_to(at(xs(i, col(i)), y(col(i))));
                            }
                            if let Ok(path) = b.build() {
                                window.paint_path(path, hairline);
                            }
                        }
                    }
                    if !back {
                        // Its edges, when shown (styled bands).
                        for edge in [true, false] {
                            let y = |c: &Column| if edge { c.up[kk] } else { c.down[kk] };
                            let mut b = crate::eyedropper::Stroke::new(
                                stroke(px(band_px), &band_dash),
                                band_px,
                            );
                            b.move_to(at(xs(0, col(0)), y(col(0))));
                            for i in 1..n {
                                b.line_to(at(xs(i, col(i)), y(col(i))));
                            }
                            b.paint(window, band_color, "VWAP band");
                        }
                        continue;
                    }
                    let mut b = PathBuilder::fill();
                    b.move_to(at(xs(0, col(0)), col(0).up[kk]));
                    for i in 1..n {
                        b.line_to(at(xs(i, col(i)), col(i).up[kk]));
                    }
                    for i in (0..n).rev() {
                        b.line_to(at(xs(i, col(i)), col(i).down[kk]));
                    }
                    b.close();
                    if let Ok(path) = b.build() {
                        window.paint_path(path, fill_color);
                    }
                }
                if back {
                    start = end;
                    continue;
                }
                // The line, in its full color where its values are approximate (from bars)
                // too: the legend says so ("≈ est."), never a fade alone.
                let line = line_color;
                let mut b =
                    crate::eyedropper::Stroke::new(stroke(thick, &line_dash), f32::from(thick));
                b.move_to(at(xs(0, col(0)), col(0).y));
                for i in 1..n {
                    b.line_to(at(xs(i, col(i)), col(i).y));
                }
                b.paint(window, line, "VWAP");
                start = end;
            }
        }
    }

    /// Text rows the legend shows (one per shown, labeled study).
    pub fn legend_rows(&self) -> usize {
        self.list
            .iter()
            .filter(|s| {
                let d = s.config.display();
                !d.hidden && d.label && !s.config.in_subpane()
            })
            .count()
    }

    /// The shown studies drawn in lower panes, top to bottom (their indices in the list).
    fn lower(&self) -> impl Iterator<Item = &Study> + '_ {
        self.list
            .iter()
            .filter(|s| s.config.in_subpane() && !s.config.display().hidden)
    }

    /// How many lower panes the shown studies ask for.
    pub fn lower_count(&self) -> usize {
        self.lower().count()
    }

    /// Lower pane `k`'s title (`None`: its study's label is off). `hovered`: the bar under
    /// the crosshair (Volume reads that bar, else the newest).
    pub fn lower_title(
        &self,
        k: usize,
        hovered: Option<usize>,
        hovered_time: Option<lines_core::UtcMillis>,
    ) -> Option<LowerTitle> {
        let s = self.lower().nth(k)?;
        if !s.config.display().label {
            return None;
        }
        match &s.config {
            IndicatorConfig::CumDelta { spec, .. } => Some(LowerTitle::Delta(spec.reset, s.approx)),
            IndicatorConfig::Volume { spec, .. } => Some(LowerTitle::Volume {
                bar: hovered.and_then(|i| s.vol.at(i).copied()).or(s.vol.newest),
                split: spec.split,
                average: spec.average.then_some(spec.window() as u16),
            }),
            IndicatorConfig::External { .. } => s.ext_def.map(|def| {
                let mut styles = [None; crate::series_pane::MAX_LINES];
                for (i, l) in s.looks.iter().enumerate().take(styles.len()) {
                    styles[i] = Some(l.line());
                }
                LowerTitle::Series {
                    def,
                    values: s.ext.values(def, hovered_time),
                    status: s.ext.status,
                    styles,
                }
            }),
            IndicatorConfig::Vwap { .. } => None,
        }
    }

    /// Lower pane `k`'s study: its index among the chart's studies and its configuration (the
    /// pane's × removes it).
    pub fn lower_study(&self, k: usize) -> Option<(usize, &IndicatorConfig)> {
        self.list
            .iter()
            .enumerate()
            .filter(|(_, s)| s.config.in_subpane() && !s.config.display().hidden)
            .nth(k)
            .map(|(i, s)| (i, &s.config))
    }

    /// How lower pane `k`'s values print.
    pub fn lower_format(&self, k: usize) -> ValueFormat {
        match self.lower().nth(k).map(|s| &s.config) {
            Some(IndicatorConfig::External { .. }) => ValueFormat::Number,
            _ => ValueFormat::Qty,
        }
    }

    /// Lower pane `k`'s title box height from its top: room the bars keep clear of. Zero when
    /// no title is drawn (study labels hidden on the chart, or the study's own label off).
    /// `label_px`: the chart's label text size. The chart uses this one value for the one scale
    /// its bars, range labels, newest tag and crosshair value all read.
    pub fn lower_title_h(&self, k: usize, labels: bool, label_px: f32) -> f32 {
        if labels && self.lower_title(k, None, None).is_some() {
            4.0 + label_px * 1.6 + 4.0
        } else {
            0.0
        }
    }

    /// Lower pane `k`'s scale this frame, for `height` px: cumulative delta's visible
    /// extremes; Volume from zero at the bottom edge to its tallest visible bar.
    /// `title_h`: the pane's title box from its top (0 when titles are hidden): Volume's
    /// tallest bar stays below it, so the readout never covers a bar's top.
    pub fn lower_scale(&self, k: usize, height: f32, title_h: f32) -> Option<Scale> {
        let s = self.lower().nth(k)?;
        match &s.config {
            IndicatorConfig::Volume { .. } => {
                (!s.vol.bars.is_empty()).then(|| Scale::from_zero_below(s.vol.max, height, title_h))
            }
            IndicatorConfig::External { .. } => {
                let (lo, hi) = s.ext.lo_hi?;
                Some(Scale::fit_below(lo, hi, height, title_h))
            }
            _ => {
                let (lo, hi) = s.extent?;
                Some(Scale::fit(lo, hi, height))
            }
        }
    }

    /// Lower pane `k`'s range labels on its axis (top, bottom): the visible range's ends for
    /// cumulative delta; the tallest visible bar for Volume (zero is its bottom edge).
    /// (A study with a zero line also marks 0, so the dashed line is named.)
    pub fn lower_marks(&self, k: usize, sc: &Scale) -> [Option<f64>; 3] {
        match self.lower().nth(k) {
            Some(s) if matches!(s.config, IndicatorConfig::Volume { .. }) => [
                Some(round_down(s.vol.max))
                    .filter(|v| *v > 0)
                    .map(|v| v as f64),
                None,
                None,
            ],
            Some(s) if matches!(s.config, IndicatorConfig::External { .. }) => {
                let zero = s.ext_def.is_some_and(|d| d.zero_line) && sc.contains(0.0);
                s.ext.lo_hi.map_or([None, None, None], |(lo, hi)| {
                    [Some(hi), Some(lo), zero.then_some(0.0)]
                })
            }
            Some(_) => [
                Some((sc.hi - (sc.hi - sc.lo) * 0.08).round()),
                Some((sc.lo + (sc.hi - sc.lo) * 0.08).round()),
                None,
            ],
            None => [None, None, None],
        }
    }

    /// Lower pane `k` draws a zero line when zero is in its range (cumulative delta; Volume's
    /// zero is its bottom edge).
    pub fn lower_zero_line(&self, k: usize) -> bool {
        matches!(
            self.lower().nth(k).map(|s| &s.config),
            Some(IndicatorConfig::CumDelta { .. })
        )
    }

    /// Lower pane `k`'s newest value (wherever the view is), for its axis tag.
    pub fn lower_last(&self, k: usize) -> Option<f64> {
        let s = self.lower().nth(k)?;
        match &s.config {
            IndicatorConfig::Volume { .. } => s.vol.newest.map(|v| v.total.0 as f64),
            // Several lines: their values are in the title, not one tag.
            IndicatorConfig::External { .. } => None,
            _ => s.newest.map(|v| v as f64),
        }
    }

    /// Lower pane `k`'s study (`origin`: the pane's top-left; x from the chart's viewport, so
    /// bars line up with the price plot). Quads (and Volume's average line) only.
    pub fn paint_lower(
        &self,
        k: usize,
        origin: (f32, f32),
        scale: &Scale,
        vp: &Viewport,
        theme: &Theme,
        window: &mut Window,
    ) {
        let Some(s) = self.lower().nth(k) else {
            return;
        };
        match &s.config {
            IndicatorConfig::Volume { spec, .. } => {
                paint_volume(
                    s,
                    spec,
                    origin,
                    scale,
                    vp,
                    volume_colors(s, theme),
                    None,
                    theme.hollow,
                    window,
                );
                paint_average(s, spec, origin, scale, vp, theme, window);
            }
            IndicatorConfig::External { .. } => {
                if let Some(def) = s.ext_def {
                    crate::series_pane::paint(
                        &s.ext,
                        def,
                        &s.looks,
                        origin,
                        scale,
                        vp.edge() as f32,
                        theme,
                        window,
                    );
                }
            }
            _ => paint_delta(s, origin, scale, vp, theme, window),
        }
    }

    /// The legend under the readout: one row per study (render time, not paint), on a
    /// `label_bg` box so lines and profile rows never run through the text.
    pub fn legend(&self, theme: &Theme, product: &Product) -> Option<AnyElement> {
        // Sub-pane studies are titled in their sub-pane.
        let shown = |s: &&Study| {
            let d = s.config.display();
            !d.hidden && d.label && !s.config.in_subpane()
        };
        if !self.list.iter().any(|s| shown(&s)) {
            return None;
        }
        let no_rth = lines_feed::catalog::sessions(product.id).is_some_and(|c| !c.has_rth());
        let mut col = div()
            .flex()
            .flex_col()
            .gap_1()
            .px(crate::zoom::ui(8.0))
            .py(crate::zoom::ui(4.0))
            .bg(theme.label_bg)
            .text_size(crate::zoom::ui(12.0));
        for s in self.list.iter().filter(shown) {
            if let IndicatorConfig::Volume { spec, .. } = &s.config {
                // Volume as an overlay: named here (its pane title is gone with its pane).
                let c =
                    theme.part_color(s.looks[study_style::VOLUME_BARS].color(), theme.volume, 100);
                let mut name = "Volume".to_owned();
                if spec.average {
                    name.push_str(&format!(" · average {}", spec.window()));
                }
                col = col.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(div().w(crate::zoom::ui(12.0)).h(crate::zoom::ui(8.0)).bg(c))
                        .child(div().text_color(theme.text_main).child(name)),
                );
                continue;
            }
            let IndicatorConfig::Vwap { spec, .. } = &s.config else {
                continue;
            };
            let (index, line_c) = vwap_line(&s.config, &s.looks);
            let c = theme.study_line(index, line_c);
            // While the engine is writing, the values `prepare` saved last.
            let newest = s
                .handle
                .as_ref()
                .and_then(|h| h.cell().try_read())
                .map(|o| o.points.last().map(|p| (p.from, p.approx, p.time)))
                .unwrap_or_else(|| s.newest_time.map(|t| (s.newest_from, s.approx, t)));
            let approx = newest.map_or(s.approx, |(_, approx, _)| approx);
            // The name in the line's color where that is AA on the legend box (a custom color
            // may not be), else the theme's text; the swatch shows the line as drawn, between
            // dashed band edges when it has bands.
            let text = theme.legend_text(c);
            // The swatch as the plot draws it: the line's type and opacity, and its band
            // edges' (styled, else the fill's dashed hairlines; none when both are off).
            let line_look = s.looks[study_style::VWAP_LINE].line();
            let bands_look = s.looks[study_style::VWAP_BANDS];
            let band_line = bands_look.line();
            let fill = s.looks[study_style::VWAP_FILL];
            let line_drawn = theme.part_color(Color::Own, c, line_look.opacity);
            let edge = if spec.bands.0 & 0b111 == 0 {
                None
            } else if bands_look.shown {
                let e = edge_color(
                    theme,
                    theme.part_color(band_line.color, c, band_line.opacity),
                );
                (e.a > 0.0).then_some((e, band_line.line))
            } else {
                let e = edge_color(theme, hairline_base(c, fill.opacity()));
                (e.a > 0.0).then_some((e, LineType::Dashed))
            };
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(vwap_swatch((line_drawn, line_look.line), edge))
                .child(div().text_color(text).child(format!(
                    "VWAP {}{}",
                    spec.reset.label(),
                    bands_label(spec.bands)
                )));
            // A run that starts late says from when, in the display zone (never a partial
            // VWAP read as the session's); the bands are the same run's. Rows that start
            // together say it once, on the first (the chart's start tag names them). Then, in
            // the secondary color, a closed session's "to 16:00 ET" and, from bars, "≈ est.":
            // "from 12:34 ET · ≈ est.".
            let zone = lines_core::display_tz::current();
            let mut words = String::new();
            if let Some((Some(from), _, time)) = newest {
                let same = |o: &Study| {
                    matches!(o.config, IndicatorConfig::Vwap { .. })
                        && shown(&o)
                        && o.newest_from == Some(from)
                };
                let first = self.list.iter().find(|o| same(o));
                let sharing = self.list.iter().filter(|o| same(o)).count();
                if sharing < 2
                    || first.is_some_and(|o| std::ptr::eq(o, s))
                    || s.newest_from != Some(from)
                {
                    words = from_label(from, time, &zone);
                }
            }
            let mut notes = String::new();
            if let Some((_, _, time)) = newest
                && !reaches_newest(time, self.reach_from)
                && let Some(newest_bar) = self.newest_bar
            {
                let end = lines_core::UtcMillis(time.0 + lines_core::UtcMillis::MINUTE);
                notes.push_str(&to_label(end, newest_bar, &zone));
            }
            if approx
                && s.error.is_none()
                && (!words.is_empty() || !(no_rth && spec.reset == Reset::Rth))
            {
                if !notes.is_empty() || !words.is_empty() {
                    notes.push_str(" · ");
                }
                notes.push_str("≈ est.");
            }
            if !words.is_empty() {
                row = row.child(div().text_color(theme.text_main).child(words));
            }
            if !notes.is_empty() && s.error.is_none() {
                row = row.child(div().text_color(theme.text_dim).child(notes));
            }
            if let Some(e) = &s.error {
                row = row.child(div().text_color(theme.text_main).child(e.clone()));
            } else if no_rth && spec.reset == Reset::Rth {
                row = row.child(
                    div()
                        .text_color(theme.text_dim)
                        .child(format!("no RTH for {}", product.symbol)),
                );
            }
            col = col.child(row);
        }
        Some(col.into_any_element())
    }

    /// Column buffers reserved so far (tests: hidden studies do no work).
    #[cfg(test)]
    pub fn reserved_columns(&self) -> usize {
        self.list.iter().map(|s| s.columns.capacity()).sum()
    }

    /// Bounds hint for the column buffers: the most bars a plot `width` px wide can show.
    pub fn max_columns(width: Pixels) -> usize {
        f32::from(width) as usize + 2
    }
}

/// An overlay bar's half width for a candle body of half width `body` at `dpi`: the odd number
/// of device pixels nearest 60 % of the body (centered on the candle's device-pixel centre, as
/// the body is), and narrower than the body whenever the body is wider than one device pixel.
pub fn overlay_half_width(body: f32, dpi: f32) -> f32 {
    let body_dev = (body * 2.0 * dpi).round() as i32;
    // The odd number of device pixels nearest 60 % of the body, at most the body less one
    // device pixel each side.
    let target = body_dev as f32 * 0.6;
    let odd = ((target - 1.0) * 0.5).round() as i32 * 2 + 1;
    let dev = odd.clamp(1, (body_dev - 2).max(1));
    dev as f32 / dpi * 0.5
}

/// `v` rounded down to two significant digits (an axis label that reads as a round number).
fn round_down(v: i64) -> i64 {
    let mut step = 1i64;
    while v / step >= 100 {
        step *= 10;
    }
    v / step * step
}

/// Volume's colors (plain bars, up/buy, down/sell) as styled, opaque as the pane draws them.
fn volume_colors(s: &Study, theme: &Theme) -> [Hsla; 3] {
    let l = &s.looks;
    let (b, u, d) = (
        l[study_style::VOLUME_BARS],
        l[study_style::VOLUME_UP],
        l[study_style::VOLUME_DOWN],
    );
    [
        theme.part_color(b.color(), theme.volume, b.opacity()),
        theme.part_color(u.color(), theme.up, u.opacity()),
        theme.part_color(d.color(), theme.down, d.opacity()),
    ]
}

/// Volume's bars from `scale`'s zero (its bottom edge, `origin` its top-left): plain in the
/// bars color; colored by candle; or split (sell volume at the base, buy volume on it, volume
/// without a side in the bars color on top). Direction is never by color alone, and its shape
/// is the candles' own: a rising bar and buy volume are hollow exactly when rising candles are
/// (`hollow`, the chart's setting), falling bars and sell volume when falling candles are
/// (bars under 3 device pixels each way stay filled, as the candles). Quads only.
#[allow(clippy::too_many_arguments)]
fn paint_volume(
    s: &Study,
    spec: &lines_indicators::VolumeSpec,
    origin: (f32, f32),
    scale: &Scale,
    vp: &Viewport,
    [bars_c, up_c, down_c]: [Hsla; 3],
    edges: Option<[Hsla; 3]>,
    hollow: crate::theme::HollowBodies,
    window: &mut Window,
) {
    // The overlay's edges: outlines and top edges in these (opaque) colors.
    let edge = |c: Hsla| match edges {
        Some([b, u, d]) => {
            if c == up_c {
                u
            } else if c == down_c {
                d
            } else {
                b
            }
        }
        None => c,
    };
    let (up_h, down_h) = (hollow.is_hollow(true), hollow.is_hollow(false));
    let dpi = window.scale_factor();
    let hair = 1.0 / dpi;
    let rect = |x: f32, y: f32, w: f32, h: f32| {
        gpui::Bounds::new(
            point(px(origin.0 + x), px(origin.1 + y)),
            gpui::size(px(w), px(h)),
        )
    };
    let snap = |v: f32| lines_core::geometry::snap(f64::from(v), dpi);
    // Overlay bars (`edges`) are narrower than the candles over them, so a hollow volume bar
    // never reads as a candle body: about 60 % of the body, an odd number of device pixels.
    let body = lines_core::geometry::candle_half_width(vp.bar_px, dpi);
    let half = if edges.is_some() {
        overlay_half_width(body, dpi)
    } else {
        body
    };
    // A segment from value `lo` to `hi` (stacked), at least a device pixel when it has volume.
    let seg = |lo: i64, hi: i64| -> Option<(f32, f32)> {
        if hi <= lo {
            return None;
        }
        let mut bot = snap(scale.y(lo as f64));
        let top = snap(scale.y(hi as f64)).min(bot - hair);
        // A stacked segment leaves a device pixel's gap above the one under it, so where one
        // side ends shows by shape too, whatever the colors.
        if lo > 0 && bot - top >= 2.0 * hair {
            bot -= hair;
        }
        Some((top, bot))
    };
    let draw = |window: &mut Window, x: f32, (top, bot): (f32, f32), col: Hsla, hollow: bool| {
        let (bx, bw, bh) = (x - half, half * 2.0, bot - top);
        if hollow && bw >= 3.0 * hair && bh >= 3.0 * hair {
            for q in [
                rect(bx, top, bw, hair),
                rect(bx, bot - hair, bw, hair),
                rect(bx, top, hair, bh),
                rect(bx + bw - hair, top, hair, bh),
            ] {
                window.paint_quad(gpui::fill(q, col));
            }
        } else {
            window.paint_quad(gpui::fill(rect(bx, top, bw, bh), col));
        }
        // The overlay: an opaque cap on each bar's top (3:1 on the chart), never a whole
        // opaque outline that could pass for a candle body.
        let e = edge(col);
        if e != col {
            window.paint_quad(gpui::fill(rect(bx, top, bw, hair), e));
        }
    };
    for v in &s.vol.bars {
        let x = lines_core::geometry::slot_x(vp, v.bar as f64, dpi);
        let total = v.total.0;
        if spec.split {
            // Sides never past the bar's total (it is the candle's own volume).
            let sell = v.sell.0.min(total);
            let buy = v.buy.0.min(total - sell);
            if let Some(r) = seg(0, sell) {
                draw(window, x, r, down_c, down_h);
            }
            if let Some(r) = seg(sell, sell + buy) {
                draw(window, x, r, up_c, up_h);
            }
            if let Some(r) = seg(sell + buy, total) {
                draw(window, x, r, bars_c, false);
            }
        } else if let Some(r) = seg(0, total) {
            match (spec.color_by_candle, v.up) {
                (true, true) => draw(window, x, r, up_c, up_h),
                (true, false) => draw(window, x, r, down_c, down_h),
                (false, _) => draw(window, x, r, bars_c, false),
            }
        }
    }
}

/// Volume's average line through the bar centres where it is defined (GPUI tessellates the
/// path, its allocation; ours: none).
fn paint_average(
    s: &Study,
    spec: &lines_indicators::VolumeSpec,
    origin: (f32, f32),
    scale: &Scale,
    vp: &Viewport,
    theme: &Theme,
    window: &mut Window,
) {
    if !spec.average {
        return;
    }
    let dpi = window.scale_factor();
    let line = s.looks[study_style::VOLUME_AVERAGE].line();
    // Its own color is the theme's main text: neutral, clear of up and down in every palette.
    let color = theme.part_color(line.color, theme.text_main, line.opacity);
    let w = study_style::vwap_px(line.width);
    let d = dash(line.line, w);
    let mut b = stroke(px(w), &d);
    let mut n = 0;
    for v in &s.vol.bars {
        let Some(a) = v.avg else { continue };
        let at = point(
            px(origin.0 + lines_core::geometry::slot_x(vp, v.bar as f64, dpi)),
            px(origin.1 + scale.y(a)),
        );
        if n == 0 {
            b.move_to(at);
        } else {
            b.line_to(at);
        }
        n += 1;
    }
    if n > 1
        && let Ok(path) = b.build()
    {
        window.paint_path(path, color);
    }
}

/// Cumulative delta's candles in a lower pane. Quads only: no allocation.
fn paint_delta(
    s: &Study,
    origin: (f32, f32),
    scale: &Scale,
    vp: &Viewport,
    theme: &Theme,
    window: &mut Window,
) {
    let dpi = window.scale_factor();
    let hair = 1.0 / dpi;
    let rect = |x: f32, y: f32, w: f32, h: f32| {
        gpui::Bounds::new(
            point(px(origin.0 + x), px(origin.1 + y)),
            gpui::size(px(w), px(h)),
        )
    };
    // The price candles' geometry (device-pixel snapping, decision in `geometry`): wicks
    // on device-pixel centres (the crosshair's line runs through them), bodies an odd
    // number of device pixels wide, edges on device-pixel boundaries.
    let half = lines_core::geometry::candle_half_width(vp.bar_px, dpi);
    let snap = |v: f32| lines_core::geometry::snap(f64::from(v), dpi);
    {
        let (rise, fall) = (
            s.looks[study_style::DELTA_UP],
            s.looks[study_style::DELTA_DOWN],
        );
        let rise = theme.part_color(rise.color(), theme.up, rise.opacity());
        let fall = theme.part_color(fall.color(), theme.down, fall.opacity());
        for c in &s.candles {
            let x = lines_core::geometry::slot_x(vp, c.bar as f64, dpi);
            let up = c.close >= c.open;
            // No change at all is neutral (dashed, below): neither direction's color.
            let col = if c.close == c.open {
                theme.text_dim
            } else if up {
                rise
            } else {
                fall
            };
            // Approximate (bar-derived) candles are fainter; the legend says so.
            let col = if c.approx { col.opacity(0.45) } else { col };
            let (yo, yc) = (snap(scale.y(c.open as f64)), snap(scale.y(c.close as f64)));
            let (yh, yl) = (snap(scale.y(c.high as f64)), snap(scale.y(c.low as f64)));
            let (top, bot) = (yo.min(yc), yo.max(yc));
            // A change is at least 6 px tall, so its hollow body shows a clear gap (direction
            // never by color alone), grown evenly about the real body; no change at all is
            // drawn dashed below.
            let flat = c.close == c.open;
            let min_body = if flat {
                hair
            } else {
                snap(6.0).max(6.0 * hair)
            };
            let (top, bot) = if bot - top < min_body {
                let t = snap((top + bot) * 0.5 - min_body * 0.5);
                (t, t + min_body)
            } else {
                (top, bot)
            };
            // The wick reaches at least the (grown) body, never a negative stub.
            let (yh, yl) = (yh.min(top), yl.max(bot));
            let (bx, bw, bh) = (x - half, half * 2.0, (bot - top).max(hair));
            if flat {
                window.paint_quad(gpui::fill(rect(x - hair * 0.5, yh, hair, yl - yh), col));
                // Dashes 2 px on, 2 px off across the body's width: no change, by shape.
                let dash = snap(2.0).max(hair);
                let mut dx = 0.0;
                while dx < bw {
                    let w = dash.min(bw - dx);
                    window.paint_quad(gpui::fill(rect(bx + dx, top, w, hair.max(snap(1.0))), col));
                    dx += dash * 2.0;
                }
                continue;
            }
            // Direction by shape too (docs/design.md): one direction's bodies are hollow,
            // as the price candles (bodies are at least 6 px, so hollow always reads when
            // the bar is 3 device pixels wide).
            let hollow = theme.hollow.is_hollow(up) && bw >= 3.0 * hair && bh >= 3.0 * hair;
            let wx = x - hair * 0.5;
            if hollow {
                // The wick above and below the body only.
                window.paint_quad(gpui::fill(rect(wx, yh, hair, top - yh), col));
                window.paint_quad(gpui::fill(rect(wx, top + bh, hair, yl - top - bh), col));
            } else {
                window.paint_quad(gpui::fill(rect(wx, yh, hair, yl - yh), col));
            }
            if hollow {
                for q in [
                    rect(bx, top, bw, hair),
                    rect(bx, top + bh - hair, bw, hair),
                    rect(bx, top, hair, bh),
                    rect(bx + bw - hair, top, hair, bh),
                ] {
                    window.paint_quad(gpui::fill(q, col));
                }
            } else {
                window.paint_quad(gpui::fill(rect(bx, top, bw, bh), col));
            }
        }
    }
}

/// A dash pattern as GPUI stroke lengths (fixed size; `n` 0 = solid).
fn dash(line: LineType, width: f32) -> ([Pixels; 4], usize) {
    let d = crate::theme::drawing_dash(line, width);
    (d.seg.map(px), d.n)
}

/// A stroke `w` wide, dashed when the pattern has segments.
fn stroke(w: Pixels, dash: &([Pixels; 4], usize)) -> PathBuilder {
    let b = PathBuilder::stroke(w);
    if dash.1 == 0 {
        b
    } else {
        b.dash_array(&dash.0[..dash.1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_indicators::{Bands, Reset, VwapSpec};

    /// Out of the plot: in the axis right of the label column (past its leader) against the
    /// axis's rows, in the column against the levels' rows only, never on the chart, and never
    /// when the tag is wider than that room.
    #[test]
    fn vwap_tags_out_of_the_plot_follow_the_placement() {
        use crate::levels_layer::{AXIS_LEADER, COLUMN_PAD, LabelPlace};
        let axis_rows = [(10.0, 26.0)];
        let level_rows = [(50.0, 66.0)];
        let room = TagRoom {
            axis_w: 120.0 + 90.0,
            column: 120.0,
            place: LabelPlace::Column,
            blocked: &axis_rows,
            column_blocked: &level_rows,
        };
        let (x, rows) = out_of_plot(&room, LabelPlace::Axis, 800.0, 60.0).unwrap();
        assert_eq!((x, rows), (800.0 + 120.0 + AXIS_LEADER, &axis_rows[..]));
        let (x, rows) = out_of_plot(&room, LabelPlace::Column, 800.0, 60.0).unwrap();
        assert_eq!((x, rows), (800.0 + COLUMN_PAD, &level_rows[..]));
        assert!(out_of_plot(&room, LabelPlace::Chart, 800.0, 60.0).is_none());
        assert!(
            out_of_plot(&room, LabelPlace::Column, 800.0, 140.0).is_none(),
            "too wide"
        );
    }

    /// An end tag outside the plot takes its own row, else the nearest one up or down within
    /// two rows, clear of the blocked rows (the last-price tag, drawings' and levels' tags)
    /// and of the tags already there; none free: not drawn.
    #[test]
    fn an_end_tag_takes_the_nearest_free_row() {
        let (h, ph) = (16.0, 400.0);
        assert_eq!(free_row(100.0, h, ph, &[], &[]), Some(100.0));
        // The last-price tag on its row: one row up.
        assert_eq!(free_row(100.0, h, ph, &[(100.0, 116.0)], &[]), Some(83.0));
        // That too taken by a tag: one row down.
        assert_eq!(
            free_row(100.0, h, ph, &[(100.0, 116.0)], &[83.0]),
            Some(117.0)
        );
        // Inside the plot at its edges.
        assert_eq!(free_row(-10.0, h, ph, &[], &[]), Some(0.0));
        // Everything near it blocked: none.
        assert_eq!(free_row(100.0, h, ph, &[(40.0, 170.0)], &[]), None);
    }

    /// A late VWAP's legend names the first time it covers, on the display zone's clock.
    #[test]
    fn a_late_start_tag_sits_left_of_the_run_else_above_its_bars() {
        // Bars from x 100 on, 10 px wide, spanning y 100..140; the run starts at x 100, y 120.
        let boxes: Vec<[f32; 4]> = (0..20)
            .map(|i| {
                let l = 100.0 + i as f32 * 10.0;
                [l, l + 8.0, 100.0, 140.0]
            })
            .collect();
        let (w, h) = (90.0, 16.0);
        // Clear space on the left: right-aligned to the tick, centred on the line.
        assert_eq!(
            place_from((100.0, 120.0), (w, h), (800.0, 400.0), 8.0, &boxes, &[]),
            (7.0, 112.0)
        );
        // Bars on the left too (the run starts after earlier data): above the first bars.
        let mut busy = boxes.clone();
        busy.insert(0, [0.0, 95.0, 110.0, 130.0]);
        let (x, y) = place_from((100.0, 120.0), (w, h), (800.0, 400.0), 8.0, &busy, &[]);
        assert_eq!(x, 100.0);
        assert!(y + h <= 100.0, "over no bar: {y}");
        // No room above either (bars reach the top): below them.
        let tall: Vec<[f32; 4]> = busy.iter().map(|b| [b[0], b[1], 0.0, b[3]]).collect();
        let (_, y) = place_from((100.0, 120.0), (w, h), (800.0, 400.0), 8.0, &tall, &[]);
        assert!(y >= 140.0, "{y}");
        // Never over a tag already placed.
        let (x, y) = place_from(
            (100.0, 120.0),
            (w, h),
            (800.0, 400.0),
            8.0,
            &boxes,
            &[(7.0, 112.0, w)],
        );
        assert!(!(x < 7.0 + w && 7.0 < x + w && y < 112.0 + h && 112.0 < y + h));
    }

    /// Band edges and the fill's hairlines are 3:1 on the chart in every theme and palette,
    /// each study color (light themes' 50 % edges weren't), and never stronger than the line
    /// itself; both are dashed by default (an edge is never read as the line).
    #[test]
    fn vwap_edges_are_3_to_1_dashed_and_under_the_line() {
        use crate::theme::rgb_of;
        use lines_core::style::{contrast, over};
        for t in crate::theme::Theme::all() {
            let bg = rgb_of(t.bg);
            for c in t.study {
                let shown =
                    |h: Hsla| contrast(over(rgb_of(h), (h.a * 100.0).round() as u8, bg), bg);
                for (a, what) in [(0.5, "edge"), (0.6, "hairline")] {
                    let mut e = c;
                    e.a = a;
                    let e = edge_color(&t, e);
                    assert!(shown(e) >= 3.0, "{} {what}", t.name);
                    assert!(
                        shown(e) <= shown(c) + 1e-3,
                        "{} {what} under the line",
                        t.name
                    );
                    assert!(
                        e.a < 1.0,
                        "{} {what} at a lower alpha than the line",
                        t.name
                    );
                }
                let mut off = c;
                off.a = 0.0;
                assert_eq!(edge_color(&t, off).a, 0.0, "off stays off");
            }
        }
        let bands = &study_style::VWAP[study_style::VWAP_BANDS].style;
        assert!(
            matches!(bands, crate::style_editor::PartStyle::Line(l) if l.line == LineType::Dashed),
            "band edges dashed by default"
        );
    }

    /// End tags go only to a line that reaches the newest bar: an RTH VWAP on an evening chart
    /// (its last column 14:59) or a view panned back gets none, and reserves no gutter.
    #[test]
    fn an_end_tag_needs_the_line_to_reach_the_newest_bar() {
        let t = |s: &str| lines_core::UtcMillis::parse_iso(s).unwrap();
        // 1-minute bars, newest 20:03 CT: the bar before it starts 20:02. The ETH line's 20:03
        // point reaches it, and so does its 20:02 point while the engine catches up with the
        // new bar (the tag and its gutter stay); an RTH line's last (14:59 CT) doesn't.
        let from = Some(t("2026-11-11T02:02:00Z"));
        assert!(reaches_newest(t("2026-11-11T02:03:00Z"), from));
        assert!(
            reaches_newest(t("2026-11-11T02:02:00Z"), from),
            "the engine a bar behind"
        );
        assert!(
            !reaches_newest(t("2026-11-11T02:01:00Z"), from),
            "two bars behind"
        );
        assert!(
            !reaches_newest(t("2026-11-10T20:59:00Z"), from),
            "ends mid-plot"
        );
        assert!(
            reaches_newest(t("2026-11-10T20:59:00Z"), None),
            "no bars yet"
        );
        // Tick bars shorter than a minute: the line's newest minute still overlaps them.
        let from = Some(t("2026-11-11T02:03:40Z"));
        assert!(reaches_newest(t("2026-11-11T02:03:00Z"), from));
        // A daily chart after the close: RTH's 14:59 point is in today's bar.
        let from = Some(t("2026-11-09T23:00:00Z"));
        assert!(reaches_newest(t("2026-11-10T20:59:00Z"), from));
    }

    /// Fast tick bars: a line reaches the newest bar from that bar's minute or the one before
    /// (not from the bar before it, seconds earlier), so tags and gutter hold and no "to
    /// HH:MM" shows while the engine catches up with the minute; 1-minute bars as before.
    #[test]
    fn tick_bars_reach_the_newest_from_its_minute_or_the_one_before() {
        let t = |s: &str| lines_core::UtcMillis::parse_iso(s).unwrap();
        let from = Some(reach_from(t("2026-11-11T02:03:40Z")));
        assert_eq!(from, Some(t("2026-11-11T02:02:00Z")));
        assert!(reaches_newest(t("2026-11-11T02:03:00Z"), from));
        assert!(
            reaches_newest(t("2026-11-11T02:02:00Z"), from),
            "the engine a minute behind"
        );
        assert!(!reaches_newest(t("2026-11-11T02:01:00Z"), from));
        // 1-minute bars: the bar before the newest, as it was.
        let from = Some(reach_from(t("2026-11-11T02:03:00Z")));
        assert!(reaches_newest(t("2026-11-11T02:02:00Z"), from));
        assert!(!reaches_newest(t("2026-11-11T02:01:00Z"), from));
    }

    /// An off-scale marker moved to the price axis never lands on the last-price tag or on a
    /// drawing's price tag: touching edges are clear, any overlap is not.
    #[test]
    fn axis_markers_stay_clear_of_price_and_drawing_tags() {
        let last = (100.0, 116.0);
        let drawing = (0.0, 16.0);
        let blocked = [last, drawing];
        assert!(clear_of(&blocked, 40.0, 14.0));
        assert!(
            clear_of(&blocked, 16.0, 14.0),
            "right under the drawing's tag"
        );
        assert!(
            clear_of(&blocked, 86.0, 14.0),
            "right above the last-price tag"
        );
        assert!(
            !clear_of(&blocked, 2.0, 14.0),
            "on the drawing's tag at the top edge"
        );
        assert!(!clear_of(&blocked, 110.0, 14.0), "on the last-price tag");
        assert!(clear_of(&[], 2.0, 14.0));
    }

    /// Panned back (the newest bar's slot past the plot's right edge): no end tags, so no
    /// gutter for them; paint and measure both read this.
    #[test]
    fn panned_back_there_are_no_end_tags() {
        assert!(newest_on_plot(Some(396.0), 400.0));
        assert!(newest_on_plot(Some(400.0), 400.0));
        assert!(!newest_on_plot(Some(404.0), 400.0), "panned back");
        assert!(!newest_on_plot(None, 400.0), "no bars");
        let b = bars();
        let fp = Footprint::new(60_000, 10);
        let none = crate::levels_layer::LevelsLayer::new(Default::default());
        let mut st = Studies::new(vec![vwap(Reset::Eth, 0)]);
        // Following live: the newest bar (index 59) three bars in from the edge.
        let mut vp = viewport();
        vp.right_index = 59.0 + lines_core::viewport::RIGHT_MARGIN_BARS;
        st.prepare(BarsView::fixed(&b, 60_000), &fp, &none, &vp, 200);
        assert!(st.newest_on_plot, "the newest bar in view");
        vp.right_index = 30.0;
        st.prepare(BarsView::fixed(&b, 60_000), &fp, &none, &vp, 200);
        assert!(!st.newest_on_plot, "panned back");
        assert_eq!(st.end_tags_need(), 0.0, "no gutter");
    }

    /// An off-scale marker with no room right of the newest bar never covers bars: it goes
    /// right-aligned in the price axis column (over the clear strip where it is wider than
    /// the column), else not at all.
    #[test]
    fn an_off_scale_marker_without_room_goes_to_the_price_axis() {
        let (pw, axis) = (400.0, 78.0);
        // No strip right of the newest bar: fits the column, or isn't drawn.
        assert_eq!(axis_mark_x(70.0, pw, pw, axis), Some(408.0));
        assert_eq!(axis_mark_x(95.0, pw, pw, axis), None, "would cover bars");
        // A strip 20 px wide: the marker reaches into it, never past it.
        assert_eq!(axis_mark_x(95.0, 380.0, pw, axis), Some(383.0));
        assert_eq!(axis_mark_x(110.0, 380.0, pw, axis), None);
        // Panned past the newest bar (its edge past the plot): the column only.
        assert_eq!(axis_mark_x(95.0, 520.0, pw, axis), None);
        assert_eq!(axis_mark_x(78.0, 520.0, pw, axis), Some(400.0));
    }

    /// The legend's dotted swatch fits its 13 px column (dots and gaps `h` each).
    #[test]
    fn the_dotted_swatch_fits_its_column() {
        for h in [1.0f32, 1.5, 2.0, 3.0] {
            let n = swatch_dots(h) as f32;
            let d = h.max(1.0);
            assert!(n >= 1.0 && n * d + (n - 1.0) * d <= 13.0, "{h}: {n} dots");
        }
        assert_eq!(swatch_dots(3.0), 2);
        assert_eq!(swatch_dots(1.0), 7);
    }

    /// A tag partly under an open panel isn't drawn; one clear of it is.
    #[test]
    fn a_tag_under_a_panel_is_not_drawn() {
        let panel = Some(crate::layout::Rect::new(1200.0, 100.0, 310.0, 400.0));
        assert!(
            covered(&[panel], 1500.0, 262.0, 80.0, 16.0),
            "its last digit outside"
        );
        assert!(covered(&[None, panel], 1250.0, 300.0, 80.0, 16.0));
        assert!(!covered(&[panel], 1510.0, 262.0, 80.0, 16.0), "right of it");
        assert!(!covered(&[panel], 1300.0, 500.0, 80.0, 16.0), "below it");
        assert!(!covered(&[None, None], 0.0, 0.0, 80.0, 16.0));
    }

    /// The tags' words: names, then the value; band edges named by their σ.
    #[test]
    fn vwap_tags_name_their_lines() {
        let list = Studies::new(vec![vwap(Reset::Eth, 0), vwap(Reset::Rth, 1)]).list;
        let mut s = String::new();
        write_names(&list, 0b11, &mut s);
        assert_eq!(s, "ETH = RTH");
        s.clear();
        write_names(&list, 0b01, &mut s);
        write_band(-1, &mut s);
        assert_eq!(s, "ETH −1σ");
        s.clear();
        write_band(2, &mut s);
        assert_eq!(s, " +2σ");
        let fmt = lines_core::PriceFormat::new(
            lines_core::PriceScale::decimal(2),
            lines_core::DisplayStyle::Decimal { digits: 2 },
            25,
        );
        s.clear();
        write_edge(Reset::Eth, 0, lines_core::Price(601250), &fmt, true, &mut s);
        assert_eq!(s, "ETH 6012.50 ▲");
        s.clear();
        write_edge(Reset::Rth, 1, lines_core::Price(602025), &fmt, true, &mut s);
        assert_eq!(s, "RTH +1σ 6020.25 ▲");
        s.clear();
        write_edge(
            Reset::Rth,
            -2,
            lines_core::Price(598000),
            &fmt,
            false,
            &mut s,
        );
        assert_eq!(s, "RTH −2σ 5980.00 ▼");
    }

    #[test]
    fn shaped_tags_go_with_the_zone_or_the_ui_zoom() {
        let mut s = Studies::new(vec![]);
        s.sync_tags((1, 1));
        let put = |s: &mut Studies| {
            s.tags
                .insert((0, Tag::End(1), 0), (Hsla::default(), Default::default()));
        };
        put(&mut s);
        s.sync_tags((1, 1));
        assert_eq!(s.tags.len(), 1, "kept while nothing moved");
        s.sync_tags((1, 2));
        assert!(s.tags.is_empty(), "the UI zoom changed");
        put(&mut s);
        s.sync_tags((2, 2));
        assert!(s.tags.is_empty(), "the display zone changed");
    }

    /// A closed session's VWAP says until when, as "from" does: "to 16:00 ET" on the chart's
    /// clock (its last minute's end), dated when not on the newest bar's day.
    #[test]
    fn a_closed_vwap_says_to_when_in_the_display_zone() {
        use lines_core::display_tz::{DisplayTz, ZoneChoice};
        let end = lines_core::UtcMillis::parse_iso("2026-11-10T21:00:00Z").unwrap();
        let evening = lines_core::UtcMillis::parse_iso("2026-11-11T01:00:00Z").unwrap();
        assert_eq!(
            to_label(end, evening, &DisplayTz::exchange()),
            "to 16:00 ET"
        );
        let central = DisplayTz::resolve(&ZoneChoice::Named("America/Chicago".into())).unwrap();
        assert_eq!(to_label(end, evening, &central), "to 15:00 CST");
        let later = lines_core::UtcMillis::parse_iso("2026-11-12T15:00:00Z").unwrap();
        assert_eq!(
            to_label(end, later, &DisplayTz::exchange()),
            "to Tue 16:00 ET"
        );
    }

    #[test]
    fn a_late_vwap_says_from_when_in_the_display_zone() {
        use lines_core::display_tz::{DisplayTz, ZoneChoice};
        let t = lines_core::UtcMillis::parse_iso("2026-11-10T17:34:00Z").unwrap();
        let central = DisplayTz::resolve(&ZoneChoice::Named("America/Chicago".into())).unwrap();
        assert_eq!(from_label(t, t, &central), "from 11:34 CST");
        assert_eq!(from_label(t, t, &DisplayTz::exchange()), "from 12:34 ET");
        assert_eq!(from_label(t, t, &DisplayTz::utc()), "from 17:34 UTC");
        // A week's or month's VWAP that started on another day says which.
        let newest = lines_core::UtcMillis::parse_iso("2026-11-12T15:00:00Z").unwrap();
        let mon = lines_core::UtcMillis::parse_iso("2026-11-09T23:00:00Z").unwrap();
        assert_eq!(from_label(mon, newest, &central), "from Mon 17:00 CST");
        assert_eq!(
            from_label(mon, newest, &DisplayTz::exchange()),
            "from Mon 18:00 ET"
        );
        let month = lines_core::UtcMillis::parse_iso("2026-11-01T23:00:00Z").unwrap();
        assert_eq!(from_label(month, newest, &central), "from 11-01 17:00 CST");
    }

    fn vwap(reset: Reset, color: u8) -> IndicatorConfig {
        IndicatorConfig::Vwap {
            spec: VwapSpec {
                reset,
                bands: Bands(0b001),
            },
            color,
            display: Default::default(),
            style: Default::default(),
        }
    }

    fn volume(overlay: bool) -> IndicatorConfig {
        IndicatorConfig::Volume {
            spec: lines_indicators::VolumeSpec {
                overlay,
                average: true,
                ..Default::default()
            },
            display: Default::default(),
            style: Default::default(),
        }
    }

    fn delta() -> IndicatorConfig {
        IndicatorConfig::CumDelta {
            spec: Default::default(),
            display: Default::default(),
            style: Default::default(),
        }
    }

    fn bars() -> Vec<lines_core::Bar> {
        (0..60i64)
            .map(|i| lines_core::Bar {
                time: lines_core::UtcMillis(i * 60_000),
                open: lines_core::Price(100),
                high: lines_core::Price(110),
                low: lines_core::Price(90),
                close: lines_core::Price(100 + (i % 3) - 1),
                volume: lines_core::Qty(10 + i),
            })
            .collect()
    }

    fn viewport() -> Viewport {
        let mut vp = Viewport::default();
        vp.set_size(400.0, 300.0);
        vp.bar_px = 8.0;
        vp.right_index = 59.0;
        vp
    }

    /// Overlay bars: about 60 % of the candle body, an odd number of device pixels (so their
    /// edges sit on device pixels like the body's), and always narrower than a body wider than
    /// one device pixel, at DPI 1.0 and 2.0 and every bar width.
    #[test]
    fn overlay_bars_are_narrower_than_the_candles_on_device_pixels() {
        for dpi in [1.0f32, 2.0] {
            for bar_px in [2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 20.0, 40.0] {
                let body = lines_core::geometry::candle_half_width(bar_px, dpi);
                let half = overlay_half_width(body, dpi);
                let (b, o) = (body * 2.0 * dpi, half * 2.0 * dpi);
                assert!(
                    (o - o.round()).abs() < 1e-4,
                    "{dpi} {bar_px}: {o} device px"
                );
                assert_eq!(o.round() as i32 % 2, 1, "{dpi} {bar_px}: odd, {o}");
                if b.round() > 1.0 {
                    assert!(o < b, "{dpi} {bar_px}: {o} not under the body's {b}");
                    // The nearest odd width to 60 %, unless the body leaves no room for it.
                    let want = (b * 0.6).min(b - 2.0);
                    assert!((o - want).abs() <= 1.0, "{dpi} {bar_px}: {o} of {b}");
                } else {
                    assert_eq!(o, 1.0);
                }
            }
        }
        // Device pixels: a 9-px body gets a 5-px bar, a 7-px body 5 (4.2 to the nearest odd), a
        // 3-px body 1 (never as wide as the body), a 13-px body 7.
        for (body_dev, bar_dev) in [(9.0, 5.0), (7.0, 5.0), (3.0, 1.0), (13.0, 7.0), (1.0, 1.0)] {
            assert_eq!(
                overlay_half_width(body_dev / 4.0, 2.0) * 4.0,
                bar_dev,
                "{body_dev}"
            );
        }
    }

    /// The pane's one scale: with study labels hidden (or the study's own label off) there is
    /// no title room, and the bars and the crosshair's value read the same scale either way: a
    /// value drawn at a height reads back as that value.
    #[test]
    fn title_room_follows_the_title_and_bars_and_readout_agree() {
        let b = bars();
        let view = BarsView::fixed(&b, 60_000);
        let fp = Footprint::new(60_000, 10);
        let vp = viewport();
        let mut st = Studies::new(vec![volume(false)]);
        st.prepare(
            view,
            &fp,
            &crate::levels_layer::LevelsLayer::new(Default::default()),
            &vp,
            200,
        );
        let label_px = 12.0;
        assert_eq!(st.lower_title_h(0, false, label_px), 0.0, "labels hidden");
        let on = st.lower_title_h(0, true, label_px);
        assert!(on > label_px, "{on}");
        let mut unlabeled = volume(false);
        unlabeled.display_mut().label = false;
        let mut st2 = Studies::new(vec![unlabeled]);
        st2.prepare(
            view,
            &fp,
            &crate::levels_layer::LevelsLayer::new(Default::default()),
            &vp,
            200,
        );
        assert_eq!(
            st2.lower_title_h(0, true, label_px),
            0.0,
            "its own label off"
        );
        for labels in [false, true] {
            let title_h = st.lower_title_h(0, labels, label_px);
            let sc = st.lower_scale(0, 120.0, title_h).unwrap();
            let max = st.list[0].vol.max as f64;
            // The tallest bar's top: clear of the title when there is one, and the crosshair
            // there reads that bar's volume.
            let top = sc.y(max);
            assert!(top >= title_h - 1e-3, "{labels}: {top} under {title_h}");
            assert!((sc.value(top) - max).abs() < 1e-3, "{labels}");
            if !labels {
                assert_eq!(sc, st.lower_scale(0, 120.0, 0.0).unwrap(), "no title room");
            }
        }
    }

    /// An external series study takes a lower pane like the others; with its source not on
    /// the chart it says so (never zeros), and a source's study resolves from the registry.
    #[test]
    fn external_series_studies_take_a_lower_pane() {
        use lines_feed::levels::{
            BoxFuture, ChartPrices, LevelContext, LevelFrame, LevelMap, LevelSource,
            LevelTransport, RefreshMode, SeriesColor, SeriesLineDef,
        };
        struct TestSource;
        const LINE: SeriesLineDef = SeriesLineDef {
            key: "osc.a",
            label: "A",
            color: SeriesColor::Foreground,
            line: lines_core::style::LineType::Solid,
        };
        const STUDIES: &[SeriesStudyDef] = &[
            SeriesStudyDef {
                id: "osc",
                name: "Osc",
                lines: &[LINE, LINE, LINE],
                zero_line: false,
                stale_after_secs: 300,
            },
            SeriesStudyDef {
                id: "swing",
                name: "Swing",
                lines: &[LINE],
                zero_line: true,
                stale_after_secs: 300,
            },
        ];
        impl LevelSource for TestSource {
            fn id(&self) -> &'static str {
                "series-test"
            }
            fn label(&self) -> &'static str {
                "Series test"
            }
            fn serves(&self, _: &Product) -> bool {
                false
            }
            fn refresh(&self) -> RefreshMode {
                RefreshMode::OneShot
            }
            fn stale_after(&self) -> std::time::Duration {
                std::time::Duration::from_secs(60)
            }
            fn series_studies(&self) -> &'static [SeriesStudyDef] {
                STUDIES
            }
            fn connect(
                &self,
                _: &LevelContext,
            ) -> BoxFuture<'static, anyhow::Result<Box<dyn LevelTransport>>> {
                Box::pin(async { Err(anyhow::anyhow!("test source")) })
            }
            fn decode(
                &self,
                _: &LevelContext,
                _: &dyn ChartPrices,
                _: Option<&LevelMap>,
                _: &[LevelFrame],
            ) -> Result<Option<LevelMap>, String> {
                Ok(None)
            }
        }
        let _ = lines_feed::levels::register(std::sync::Arc::new(TestSource));
        let ext = |study: &str| IndicatorConfig::External {
            spec: lines_indicators::ExternalSpec {
                source: "series-test".into(),
                study: study.into(),
            },
            display: Default::default(),
            style: Default::default(),
        };
        let mut st = Studies::new(vec![volume(false), ext("osc"), ext("swing")]);
        assert_eq!(st.lower_count(), 3);
        assert_eq!(st.list[1].looks.len(), 3, "a part per line");
        let b = bars();
        let fp = Footprint::new(60_000, 10);
        let none = crate::levels_layer::LevelsLayer::new(Default::default());
        st.prepare(BarsView::fixed(&b, 60_000), &fp, &none, &viewport(), 200);
        let Some(LowerTitle::Series {
            def,
            values,
            status,
            ..
        }) = st.lower_title(1, None, None)
        else {
            panic!("a series title")
        };
        assert_eq!((def.id, def.lines.len()), ("osc", 3));
        assert_eq!(status, ExtStatus::Missing("source not on this chart"));
        assert!(values.iter().all(Option::is_none), "no values, never zeros");
        assert_eq!(st.lower_scale(1, 100.0, 0.0), None, "nothing to scale");
        assert_eq!(st.lower_format(1), ValueFormat::Number);
        assert_eq!(st.lower_format(0), ValueFormat::Qty);
        assert_eq!(st.lower_study(2).map(|(i, _)| i), Some(2));
        // An unknown study of a known source: no definition, and it says so.
        let st = Studies::new(vec![ext("nope")]);
        assert!(st.list[0].ext_def.is_none());
    }

    /// The lower-pane mechanism: every shown study that declares `in_subpane` gets a pane,
    /// in the studies' order; an overlay or hidden study gets none.
    #[test]
    fn lower_panes_follow_the_studies_that_declare_them() {
        let st = Studies::new(vec![vwap(Reset::Eth, 0), volume(false), delta()]);
        assert_eq!(st.lower_count(), 2);
        assert!(matches!(
            st.lower_title(0, None, None),
            Some(LowerTitle::Volume { bar: None, .. })
        ));
        assert!(matches!(
            st.lower_title(1, None, None),
            Some(LowerTitle::Delta(..))
        ));
        assert!(st.lower_zero_line(1) && !st.lower_zero_line(0));
        let st = Studies::new(vec![volume(true), delta()]);
        assert_eq!(st.lower_count(), 1, "an overlay draws in the price plot");
        assert!(matches!(
            st.lower_title(0, None, None),
            Some(LowerTitle::Delta(..))
        ));
        let mut hidden = volume(false);
        hidden.display_mut().hidden = true;
        assert_eq!(Studies::new(vec![hidden]).lower_count(), 0);
    }

    /// Volume's pane reads the chart's own bars: the bar under the crosshair, else the newest;
    /// its scale starts at zero; a hidden Volume builds nothing.
    #[test]
    fn volume_reads_the_chart_bars_and_hidden_does_no_work() {
        let b = bars();
        let view = BarsView::fixed(&b, 60_000);
        let fp = Footprint::new(60_000, 10);
        let vp = viewport();
        let mut st = Studies::new(vec![volume(false)]);
        st.prepare(
            view,
            &fp,
            &crate::levels_layer::LevelsLayer::new(Default::default()),
            &vp,
            200,
        );
        let Some(LowerTitle::Volume {
            bar: Some(newest),
            average: Some(20),
            ..
        }) = st.lower_title(0, None, None)
        else {
            panic!("a readout")
        };
        assert_eq!((newest.bar, newest.total), (59, lines_core::Qty(69)));
        let Some(LowerTitle::Volume {
            bar: Some(hovered), ..
        }) = st.lower_title(0, Some(40), None)
        else {
            panic!("a readout")
        };
        assert_eq!(hovered.total, lines_core::Qty(50));
        assert_eq!(st.lower_last(0), Some(69.0));
        let sc = st.lower_scale(0, 100.0, 0.0).unwrap();
        assert_eq!(sc.y(0.0), 100.0, "zero on the bottom edge");
        assert_eq!(st.lower_marks(0, &sc), [Some(69.0), None, None]);
        // Below the title box: the tallest bar's top clears it.
        let sc = st.lower_scale(0, 100.0, 30.0).unwrap();
        assert!(sc.y(69.0) >= 30.0, "{}", sc.y(69.0));
        assert_eq!(round_down(2_373), 2_300);
        assert_eq!(round_down(69), 69);
        assert_eq!(round_down(0), 0);
        let mut hidden = volume(false);
        hidden.display_mut().hidden = true;
        let mut st = Studies::new(vec![hidden]);
        st.prepare(
            view,
            &fp,
            &crate::levels_layer::LevelsLayer::new(Default::default()),
            &vp,
            200,
        );
        assert!(st.list[0].vol.bars.is_empty() && st.list[0].vol.newest.is_none());
        assert_eq!(
            st.list[0].vol.bars.capacity(),
            0,
            "no buffer reserved either"
        );
    }

    /// N5: a style-only change keeps each study (its engine handle and built columns stay)
    /// and re-resolves its looks; any other change is refused, so the caller rebuilds.
    #[test]
    fn restyle_keeps_studies_for_a_style_only_change() {
        let mut st = Studies::new(vec![vwap(Reset::Eth, 0), vwap(Reset::Rth, 1)]);
        // Stand-ins for what the engine built (no engine in a unit test).
        st.list[0].columns = vec![Column::default(); 3];
        st.list[1].columns = vec![Column::default(); 2];
        let mut styled = st.configs();
        styled[0]
            .style_mut()
            .insert("fill".into(), serde_json::json!({"opacity": 75}));
        assert!(st.restyle(&styled));
        assert_eq!(st.configs(), styled);
        assert_eq!(st.list[0].columns.len(), 3, "kept, not rebuilt");
        assert_eq!(st.list[1].columns.len(), 2);
        assert_eq!(st.list[0].looks[study_style::VWAP_FILL].opacity(), 75);
        // A real change (another reset, another study, one fewer): not a restyle.
        let mut other = styled.clone();
        other[1] = vwap(Reset::Weekly, 1);
        assert!(!st.restyle(&other));
        assert!(!st.restyle(&styled[..1]));
        assert_eq!(st.configs(), styled, "a refused restyle changes nothing");
    }

    /// N1: a VWAP's line color (palette index) and width live in its config's `color` and
    /// `display.width`; changing only those is a restyle too (nodes kept, looks re-resolved).
    #[test]
    fn restyle_keeps_studies_for_a_color_or_width_change() {
        let mut st = Studies::new(vec![vwap(Reset::Eth, 0)]);
        st.list[0].columns = vec![Column::default(); 3];
        let mut c = st.configs();
        if let IndicatorConfig::Vwap { color, display, .. } = &mut c[0] {
            *color = 2;
            display.width = 4;
        }
        assert!(st.restyle(&c));
        assert_eq!(st.list[0].columns.len(), 3, "kept, not rebuilt");
        let line = st.list[0].looks[study_style::VWAP_LINE].line();
        assert_eq!(line.color, Color::Palette(2));
        assert_eq!(line.width, 4);
    }

    /// B1: the band fill under the candles is held back with as many layers as bands nest,
    /// and cached for (theme, fill, layers).
    #[test]
    fn band_fill_is_held_back_by_its_nesting() {
        let st = Studies::new(vec![vwap(Reset::Eth, 0)]);
        let t = Theme::dark(crate::theme::Palette::Standard);
        let heavy = t.study[0].opacity(study_style::VWAP_FILL_ALPHA);
        let one = st.list[0].fill_under_candles(&t, heavy, 1);
        let three = st.list[0].fill_under_candles(&t, heavy, 3);
        assert!(three.a < heavy.a, "three nested heavy fills are held back");
        assert!(three.a <= one.a);
        assert_eq!(st.list[0].fill_cap.get().map(|c| c.3), Some(three));
        assert_eq!(band_layers(Bands(0b111)), 3);
        assert_eq!(band_layers(Bands(0b001)), 1);
        assert_eq!(band_layers(Bands(0)), 0);
    }
}
