//! Footprint chart mode (roadmap 10e): per bar, volume by price row split by aggressor,
//! drawn in place of the candle wherever the bar has trade-level data. A study in the Studies
//! menu, off by default, saved with the chart (`ChartState::footprint`).
//!
//! The cells are `lines_core::footprint` (pure, exact, the merge of each bar's 1-minute
//! footprints on any timeframe), built in `prepare` under the store lock into fixed buffers.
//! `paint` draws quads and numbers from the cached-glyph painter (`crate::text::Glyphs`) with
//! `FixedStr` formatting: nothing allocates per frame once the glyphs are shaped.
//!
//! - **Modes:** sell × buy volume (aggressors: sellers who hit the bid left, buyers who took
//!   the offer right), delta per row, total volume.
//! - **Every drawn cell carries its numbers, whole.** A bar is drawn as cells only when its
//!   whole column is on the plot, every number fits its room (measured from the font's
//!   tabular digit widths, `TextMetrics`; crypto quantities have decimals), rows are tall
//!   enough for the text, and no cell lies under the readout and legend block at the plot's
//!   top-left (`set_legend_block`, measured at render time). Otherwise the bar is a candle and
//!   the legend says why (`Fallbacks`): a cut number (a lost minus sign), a spilled one or a
//!   shaded box without one would read as another value, or as nothing traded.
//! - With the price axis auto-fitted, the range's top is raised (`clear_top`) so cells start
//!   below that block; only a manual price zoom can push bars under it.
//! - Each column has an open → close strip on its left (a candle body: hollow/filled as the
//!   candles) with the open as a small wedge on the strip's left pointing at it (where an
//!   OHLC bar marks its open), so a bar's direction shows by shape and position, never by
//!   color alone; a bar with cells cut by the plot's edge is a candle and the legend says so.
//! - Cells are opaque: the volume profile and study lines (VWAP) are drawn first and pass
//!   under them, never through a number.
//! - Price and crosshair lines break where they cross cells (`hline_segments`).
//! - **Imbalances** are marked by an underline under the imbalanced number (sell: the left
//!   half, buy: the right half) as well as the number's color: a horizontal mark inside the
//!   cell, never mistaken for the column's vertical open → close strip on its left.
//! - **Volume without an aggressor side** (a source that doesn't report it) is never dropped
//!   silently: a cell holding only such volume shows it as `?N` instead of `0 | 0`, and the
//!   legend counts it (`? N without aggressor side`). Mixed cells still show only their sell and buy numbers: the legend count covers their side-less share.
//! - **POC** per bar: the cell is outlined.
//! - **Rows** use the DOM grid and grouping, coarsened until a row is at least 15 px tall.
//! - **Styles** (decision 0026, `crate::study_style::FOOTPRINT`): cell shading, imbalance marks
//!   and the POC outline take a user color and opacity; each mark keeps its side's place and
//!   the strip its shape, so direction never depends on those colors. An imbalanced number
//!   keeps its side's color only with the marks' own color, else it is the main text color.
//!   Every number stays AA (4.5:1) on its cell whatever the cells' color and opacity: a tint
//!   that would go past that is held back at its strongest shade, its weaker shades scaled
//!   with it ([`CellShades`], tested over study colors, sampled custom colors, opacities,
//!   themes, palettes and modes); the style editor says when it is.

use gpui::{App, Bounds, Hsla, Pixels, Window, fill, point, px, size};
use lines_core::footprint::{BarCells, Cell, FootprintFrame, ImbalanceRule, fit_grouping};
use lines_core::style::{Color, Rgb};
use lines_core::{BarsView, FixedStr, Qty, Viewport};
use lines_feed::ChartData;
use serde::{Deserialize, Serialize};

use crate::chart_view::ChartLayer;
use crate::study_style::{self, FOOTPRINT, Look};
use crate::text::Glyphs;
use crate::theme::{Theme, with_alpha};
use lines_indicators::PartStyles;

/// What each cell shows.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FootprintMode {
    #[default]
    Off,
    /// Selling (hit the bid) × buying (took the offer).
    BidAsk,
    /// Buying minus selling.
    Delta,
    /// Total volume.
    Volume,
}

impl FootprintMode {
    pub fn label(self) -> &'static str {
        match self {
            FootprintMode::Off => "off",
            FootprintMode::BidAsk => "sell × buy",
            FootprintMode::Delta => "delta",
            FootprintMode::Volume => "volume",
        }
    }

    /// The narrowest bar (logical px) whose cells can be read in this mode.
    pub fn min_bar_px(self) -> f64 {
        match self {
            FootprintMode::BidAsk => 64.0,
            _ => 36.0,
        }
    }
}

/// A chart's footprint settings. Every field is optional in files.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FootprintConfig {
    pub mode: FootprintMode,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// Imbalance ratio, percent (300 = 3×).
    pub imbalance_pct: u32,
    /// Imbalance minimum volume, in whole units of the product's quantity (contracts; whole
    /// coins for crypto, whose quantities have decimals).
    pub imbalance_min: i64,
    /// Ticks per row (`None`: the finest readable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grouping: Option<u32>,
}

impl Default for FootprintConfig {
    fn default() -> Self {
        let r = ImbalanceRule::default();
        FootprintConfig {
            mode: FootprintMode::Off,
            hidden: false,
            imbalance_pct: r.ratio_pct,
            imbalance_min: r.min_volume.0,
            grouping: None,
        }
    }
}

impl FootprintConfig {
    pub fn is_default(&self) -> bool {
        *self == FootprintConfig::default()
    }

    pub fn shown(&self) -> bool {
        self.mode != FootprintMode::Off && !self.hidden
    }

    /// The rule in the product's quantity units (`decimals` of its `QtyFormat`).
    fn rule(&self, decimals: u8) -> ImbalanceRule {
        let unit = 10i64.saturating_pow(u32::from(decimals));
        ImbalanceRule {
            ratio_pct: self.imbalance_pct.max(100),
            min_volume: Qty(self.imbalance_min.max(1).saturating_mul(unit)),
        }
    }
}

/// Rows shorter than this are coarsened (logical px): the 11 px numbers need ~14.3 px.
pub const MIN_ROW_PX: f64 = 15.0;
/// Shorter rows (after device-pixel rounding) draw no cells at all.
const MIN_TEXT_ROW_PX: f32 = TEXT_PX * 1.3;
/// Most cells drawn in one frame (bounded work; a bar beyond it shows as a candle).
const MAX_CELLS: usize = 40_000;
/// Most rows in one bar.
const MAX_ROWS: usize = 2_048;
/// The type scale's smallest size (docs/design.md §5).
const TEXT_PX: f32 = 11.0;
/// Sell × buy: each number keeps this far from the divider in the cell's middle.
const GUTTER: f32 = 4.0;
/// Each number keeps this far from the cell's side (beyond the imbalance marker).
const PAD: f32 = 2.0;
/// The open/close strip on a column's left (logical px; snapped to device pixels).
const STRIP_W: f32 = 3.0;

/// Widths of the characters cell numbers use (tabular digits), measured from the font.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TextMetrics {
    pub digit: f32,
    pub minus: f32,
    pub dot: f32,
    pub question: f32,
}

impl TextMetrics {
    /// Until the font is measured: wider than an 11 px UI font's digits (~6.5 px).
    pub const ESTIMATE: TextMetrics = TextMetrics {
        digit: 7.0,
        minus: 7.0,
        dot: 4.0,
        question: 7.0,
    };

    /// Width of `v` printed in `qty`'s format. No allocation.
    pub fn width(&self, qty: lines_core::QtyFormat, v: i64) -> f32 {
        let mut s = FixedStr::<32>::new();
        let _ = qty.write(Qty(v), &mut s);
        s.as_str()
            .chars()
            .map(|c| match c {
                '-' => self.minus,
                '.' => self.dot,
                '?' => self.question,
                _ => self.digit,
            })
            .sum()
    }
}

/// A bar's column: the open/close strip on the left, then the cells.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Column {
    pub strip_x: f32,
    pub strip_w: f32,
    pub cell_left: f32,
    pub cell_w: f32,
}

impl Column {
    /// Plot-local, device-pixel snapped, centred on the bar's wick position.
    pub fn of(vp: &Viewport, bar: usize, scale: f32) -> Column {
        let snap = |v: f64| lines_core::geometry::snap(v, scale);
        let x = lines_core::geometry::slot_x(vp, bar as f64, scale);
        let half = snap((vp.bar_px * 0.46).max(2.0));
        let strip_w = snap(f64::from(STRIP_W)).max(1.0 / scale);
        let gap = snap(1.0).max(1.0 / scale);
        let left = x - half;
        Column {
            strip_x: left,
            strip_w,
            cell_left: left + strip_w + gap,
            cell_w: (half * 2.0 - strip_w - gap).max(0.0),
        }
    }

    /// Room for one number: each half in sell × buy, the whole cell otherwise.
    pub fn text_room(&self, mode: FootprintMode) -> f32 {
        let inner = self.cell_w - 2.0 * PAD;
        match mode {
            FootprintMode::BidAsk => (inner * 0.5 - GUTTER).max(0.0),
            _ => inner.max(0.0),
        }
    }
}

/// Every number of a bar fits its room (a bar where one doesn't is a candle, never a number
/// spilling into its neighbor or cut).
pub fn numbers_fit(
    cells: &[Cell],
    mode: FootprintMode,
    qty: lines_core::QtyFormat,
    m: &TextMetrics,
    room: f32,
) -> bool {
    cells.iter().all(|c| match mode {
        // A cell of side-less volume only prints `?N` across the whole cell.
        FootprintMode::BidAsk if only_unknown(c) => {
            m.question + m.width(qty, c.unknown.0) <= 2.0 * (room + GUTTER)
        }
        FootprintMode::Delta if only_unknown(c) => m.question + m.width(qty, c.unknown.0) <= room,
        FootprintMode::BidAsk => m.width(qty, c.sell.0) <= room && m.width(qty, c.buy.0) <= room,
        FootprintMode::Delta => m.width(qty, c.delta()) <= room,
        FootprintMode::Volume => m.width(qty, c.total.0) <= room,
        FootprintMode::Off => true,
    })
}

/// All of a cell's volume came without an aggressor side.
pub fn only_unknown(c: &Cell) -> bool {
    c.buy.0 == 0 && c.sell.0 == 0 && c.unknown.0 > 0
}

/// The imbalance underline under the sell (left) or buy (right) half of a cell: `(x, w)`
/// relative to the cell's left (one-number modes; sell × buy underlines the number itself).
/// A horizontal mark under the number, so it never looks like the column's vertical open →
/// close strip.
pub fn imbalance_underline(cell_w: f32, buy: bool) -> (f32, f32) {
    let half = cell_w * 0.5;
    let inset = (half * 0.18).max(PAD);
    let x = if buy { half + inset } else { inset };
    (x, (half - 2.0 * inset).max(1.0))
}

/// A whole number of device pixels per row of `u` price units.
fn row_height(vp: &Viewport, u: i64, scale: f32) -> f32 {
    let r = u as f64 * vp.height / (vp.price_hi - vp.price_lo).abs().max(1e-9);
    ((r * f64::from(scale)).round().max(1.0) / f64::from(scale)) as f32
}

/// The top of row `row`'s cell (rows are centred on their lowest price's tick).
fn cell_top(vp: &Viewport, row: i64, u: i64, tick: i64, row_h: f32, scale: f32) -> f32 {
    let mid = (row * u) as f64 + (u - tick) as f64 * 0.5;
    lines_core::geometry::snap(vp.price_to_y(mid) - f64::from(row_h) * 0.5, scale)
}

/// With the footprint on and the price axis auto-fitted: raise the range's top so the
/// highest visible prices sit at least `px` below the plot's top (where the readout and the
/// legends are), so no cell starts under them.
pub fn clear_top(vp: &mut Viewport, px: f64) {
    let h = vp.height;
    if !(px > 0.0 && px < h * 0.5) {
        return;
    }
    let (lo, hi) = (vp.price_lo, vp.price_hi);
    vp.price_hi = (hi - px * lo / h) / (1.0 - px / h);
}

/// Why bars show as candles in footprint mode this frame (the legend says so).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Fallbacks {
    /// Bars narrower than the mode needs: every bar.
    pub narrow: bool,
    /// Rows too short for numbers: every bar.
    pub rows_short: bool,
    /// A number wider than its cell.
    pub too_wide: bool,
    /// Cells under the readout and legends (a manual price zoom).
    pub under_legend: bool,
    /// No counted trades (history).
    pub no_trades: bool,
    /// A cell cut by the plot's top or bottom edge (a manual pan or zoom).
    pub at_edge: bool,
    /// More cells than one frame draws (bounded work).
    pub too_many: bool,
    /// Drawn volume without an aggressor side (0: none). A count, so a change re-renders the
    /// legend.
    pub no_side: i64,
    /// Price ticks per row when rows aggregate (0 or 1: one tick a row): the legend says so.
    pub row_ticks: i64,
}

pub struct FootprintLayer {
    config: FootprintConfig,
    frame: FootprintFrame,
    /// Cells are drawn this frame (shown, and bars wide enough).
    active: bool,
    tick: i64,
    /// The product's quantity format (cells print through it).
    qty: lines_core::QtyFormat,
    glyphs: Glyphs,
    /// Character widths of the cell font (measured on the first paint).
    metrics: TextMetrics,
    /// The window's scale, as of the last paint (prepare snaps as paint does).
    scale: f32,
    /// The visible range's first bar (candles are indexed from it).
    first_bar: usize,
    /// The readout and legend block at the plot's top-left: (width, bottom), plot-local.
    legend_block: (f32, f32),
    fallbacks: Fallbacks,
    /// Each drawn bar's open and close (its strip), aligned with `frame.bars`.
    open_close: Vec<(lines_core::Price, lines_core::Price)>,
    /// The cells drawn this frame (x0, x1, y0, y1; plot-local): horizontal lines break there.
    drawn: Vec<[f32; 4]>,
    /// User styles (decision 0026), as saved, and resolved for paint.
    style: PartStyles,
    looks: [Look; 3],
    /// The cell shading for (theme, mode, looks), computed when one of them changes.
    shades: Option<(ShadesKey, CellShades)>,
}

type ShadesKey = (&'static str, FootprintMode, [Look; 3]);

/// A cell's fill color, from theme tokens (`CellShades::color`).
pub type CellFill = Hsla;

/// Which tint a cell takes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Tint {
    Neutral = 0,
    /// Delta mode, more buying.
    Up = 1,
    /// Delta mode, more selling.
    Down = 2,
}

/// The cells' shading in a theme (decision 0026): each tint's color and its alpha at shade 0
/// and at full shade, held back so every number a cell can carry (the main text color; in
/// sell × buy with the marks' own color, the up and down colors too) stays AA on the tint's
/// strongest shade composited over the chart.
///
/// Unstyled, delta and volume cells are never held back; sell × buy cells are, because an
/// imbalanced number in its side's color was below AA on the strongest neutral cells before
/// styles: their strongest alpha goes from 0.28 to 0.22 (dark), 0.27 (dark colorblind), 0.11
/// (light) and 0.14 (light colorblind), never to 0 (`unstyled_sell_buy_caps`). Light sell × buy
/// cells are so about half as shaded as before styles.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct CellShades {
    /// (opaque color, alpha at shade 0, alpha at full shade) per [`Tint`].
    tints: [(Hsla, f32, f32); 3],
    /// Each [`Tint`] was held back.
    pub held: [bool; 3],
}

/// The cells' own shading: alpha at shade 0, and what full shade adds.
const NEUTRAL_SHADE: (f32, f32) = (0.06, 0.22);
const DELTA_SHADE: (f32, f32) = (0.12, 0.4);
/// The contrast every cell number keeps, with a margin for the GPU's blend rounding.
const CELL_TEXT_CONTRAST: f64 = lines_core::style::TEXT_CONTRAST + 0.05;

impl CellShades {
    /// The shading for `looks` (the footprint's parts) in `mode`, in theme `t`.
    pub fn of(t: &Theme, looks: &[Look; 3], mode: FootprintMode) -> CellShades {
        let cells = looks[study_style::FP_CELLS];
        let side =
            mode == FootprintMode::BidAsk && looks[study_style::FP_IMBALANCE].color() == Color::Own;
        let all = [t.text_main, t.up, t.down];
        let texts = &all[..if side { 3 } else { 1 }];
        let bg = crate::theme::rgb_of(t.bg);
        let mut held = [false; 3];
        let mut tint = |i: Tint, base: Hsla, (lo, span): (f32, f32)| {
            // The part's color (its own: `base`) at the part's opacity.
            let c = t.part_color(cells.color(), with_alpha(base, 1.0), cells.opacity());
            let (lo, top) = (lo * c.a, (lo + span) * c.a);
            let cap = readable_alpha(bg, crate::theme::rgb_of(c), top, texts);
            let k = if top > 0.0 { (cap / top).min(1.0) } else { 1.0 };
            held[i as usize] = k < 1.0;
            (with_alpha(c, 1.0), lo * k, top * k)
        };
        let tints = [
            tint(Tint::Neutral, t.text_dim, NEUTRAL_SHADE),
            tint(Tint::Up, t.up, DELTA_SHADE),
            tint(Tint::Down, t.down, DELTA_SHADE),
        ];
        CellShades { tints, held }
    }

    /// The tints `mode` draws (delta: all three; sell × buy and volume: neutral).
    pub fn tints(mode: FootprintMode) -> &'static [Tint] {
        if mode == FootprintMode::Delta {
            &[Tint::Neutral, Tint::Up, Tint::Down]
        } else {
            &[Tint::Neutral]
        }
    }

    /// A tint `mode` draws was held back.
    #[cfg(test)]
    pub fn held_in(&self, mode: FootprintMode) -> bool {
        CellShades::tints(mode)
            .iter()
            .any(|t| self.held[*t as usize])
    }

    /// A tint's alpha at full shade (0: held back to no shading at all).
    pub fn strongest(&self, tint: Tint) -> f32 {
        self.tints[tint as usize].2
    }

    /// A cell's fill at `shade` (0..=1: its volume against the largest cell's).
    pub fn color(&self, tint: Tint, shade: f32) -> CellFill {
        let (c, lo, hi) = self.tints[tint as usize];
        with_alpha(c, lo + (hi - lo) * shade.clamp(0.0, 1.0))
    }
}

/// The strongest alpha, at most `top`, at which `tint` over `bg` keeps each of `texts` at
/// [`CELL_TEXT_CONTRAST`], and so does every weaker alpha (steps of 1/64; no allocation).
fn readable_alpha(bg: Rgb, tint: Rgb, top: f32, texts: &[Hsla]) -> f32 {
    const STEPS: u16 = 64;
    let reads = |a: f32| {
        let seen = blend(tint, a, bg);
        texts.iter().all(|x| {
            lines_core::style::contrast(crate::theme::rgb_of(*x), seen) >= CELL_TEXT_CONTRAST
        })
    };
    let mut ok = 0.0;
    for i in 1..=STEPS {
        let a = top * f32::from(i) / f32::from(STEPS);
        if !reads(a) {
            break;
        }
        ok = a;
    }
    ok
}

/// `top` at alpha `a` over opaque `bottom`, as drawn.
pub fn blend(top: Rgb, a: f32, bottom: Rgb) -> Rgb {
    let a = f64::from(a.clamp(0.0, 1.0));
    let mix = |x: u8, y: u8| (f64::from(x) * a + f64::from(y) * (1.0 - a)).round() as u8;
    Rgb {
        r: mix(top.r, bottom.r),
        g: mix(top.g, bottom.g),
        b: mix(top.b, bottom.b),
    }
}

/// The style editor's warning on the footprint's Cells tab when the color and opacity picked
/// are held back in the chart's `mode` (off: in any mode, each named) in some theme or palette.
/// Holds unstyled cells already have (sell × buy, the cells' own color) aren't warned about;
/// a tint held back to no shading at all says so. `themes` in the order of `ctxs`.
pub fn cells_warning(
    looks: &[Look; 3],
    mode: FootprintMode,
    themes: &[Theme],
    ctxs: &[crate::style_editor::ThemeCtx],
) -> Option<crate::style_editor::Warning> {
    let cells = study_style::FP_CELLS;
    if looks[cells].style == FOOTPRINT[cells].style {
        return None;
    }
    let own = looks[cells].color() == Color::Own;
    let plain = Look::defaults(&FOOTPRINT);
    let all = [
        FootprintMode::BidAsk,
        FootprintMode::Delta,
        FootprintMode::Volume,
    ];
    let modes: &[FootprintMode] = if mode == FootprintMode::Off {
        &all
    } else {
        std::slice::from_ref(&mode)
    };
    let mut gone = false;
    let mut named: Vec<&str> = Vec::new();
    let mut hit = vec![false; themes.len()];
    for m in modes {
        for (t, h) in themes.iter().zip(hit.iter_mut()) {
            let s = CellShades::of(t, looks, *m);
            let before = CellShades::of(t, &plain, *m);
            let new_hold = CellShades::tints(*m)
                .iter()
                .any(|i| s.held[*i as usize] && !(own && before.held[*i as usize]));
            if new_hold {
                *h = true;
                gone |= CellShades::tints(*m).iter().any(|i| s.strongest(*i) <= 0.0);
                if !named.contains(&m.label()) {
                    named.push(m.label());
                }
            }
        }
    }
    if !hit.iter().any(|h| *h) {
        return None;
    }
    let what = if gone { "Not shaded" } else { "Shown fainter" };
    let in_mode = format!(" in {},", named.join(" and "));
    Some(crate::style_editor::Warning {
        text: format!(
            "{what}{in_mode}{}, so the counts on the cells stay readable (4.5:1).",
            crate::style_editor::where_phrase(ctxs, &hit)
        ),
        swatch: None,
    })
}

/// The style editor's warning on the footprint's Imbalances tab when a color is picked: both
/// sides' marks then share it in every theme and palette, and only their place (each under its
/// side's number) tells a buy imbalance from a sell one.
pub fn imbalance_warning(looks: &[Look; 3]) -> Option<crate::style_editor::Warning> {
    (looks[study_style::FP_IMBALANCE].color() != Color::Own).then(|| crate::style_editor::Warning {
        text: "Buy and sell imbalance marks share this color in every theme and palette; only their place tells them apart (each under its side's number), and the numbers take the main text color.".to_owned(),
        swatch: None,
    })
}

impl FootprintLayer {
    pub fn new(config: FootprintConfig) -> FootprintLayer {
        FootprintLayer {
            config,
            // Buffers are reserved the first time the footprint is shown (`ensure_capacity`):
            // a chart that never shows it holds none.
            frame: FootprintFrame::with_capacity(0, 0, 0),
            active: false,
            tick: 1,
            qty: lines_core::QtyFormat::WHOLE,
            glyphs: Glyphs::new(TEXT_PX),
            metrics: TextMetrics::ESTIMATE,
            scale: 1.0,
            first_bar: 0,
            legend_block: (0.0, 0.0),
            fallbacks: Fallbacks::default(),
            style: PartStyles::new(),
            looks: Look::defaults(&FOOTPRINT),
            shades: None,
            open_close: Vec::new(),
            drawn: Vec::new(),
        }
    }

    /// The readout and legend block at the plot's top-left (plot-local width and bottom),
    /// measured at render time: cells never sit under it (set before `prepare`).
    pub fn set_legend_block(&mut self, width: f32, bottom: f32) {
        self.legend_block = (width, bottom);
    }

    pub fn config(&self) -> FootprintConfig {
        self.config
    }

    pub fn set_config(&mut self, c: FootprintConfig) {
        self.config = c;
    }

    /// Its user styles, as saved.
    pub fn style(&self) -> &PartStyles {
        &self.style
    }

    pub fn set_style(&mut self, style: &PartStyles) {
        if *style != self.style {
            self.style = style.clone();
            self.looks = study_style::read(style, &FOOTPRINT);
        }
    }

    /// Why bars are candles this frame (the chart repaints its legend when this changes).
    pub fn fallbacks(&self) -> Fallbacks {
        self.fallbacks
    }

    /// The legend line, if the footprint is on: its mode, and every reason bars show as
    /// candles this frame.
    pub fn legend(&self) -> Option<(String, Option<String>)> {
        if !self.config.shown() {
            return None;
        }
        let f = self.fallbacks;
        // Volume without a side first, then why some bars are candles, then the key (the line
        // is capped at the legend's width: what matters most comes first).
        let mut notes: Vec<String> = Vec::new();
        if f.no_side > 0 {
            let mut n = String::new();
            let _ = self.qty.write(Qty(f.no_side), &mut n);
            notes.push(format!("? {n} without aggressor side"));
        }
        for (on, text) in [
            (f.narrow || f.rows_short, "zoom in to see cells"),
            (f.too_wide, "some bars: numbers too wide, zoom in"),
            (f.under_legend, "bars under the legend shown as candles"),
            // Short, so the capped legend line keeps the key word ("candles").
            (f.at_edge, "edge bars as candles"),
            (f.too_many, "too many cells: some bars as candles"),
            (f.no_trades, "footprint from counted trades only"),
        ] {
            if on {
                notes.push(text.to_owned());
            }
        }
        let rows = if f.row_ticks > 1 {
            format!(" · {} ticks/row", f.row_ticks)
        } else {
            String::new()
        };
        Some((
            format!("Footprint · {}{rows}", self.config.mode.label()),
            Some({
                // The key last: if the capped line is cut, the count and reasons stay.
                notes.push("underline = imbalance".to_owned());
                notes.join(" · ")
            }),
        ))
    }

    /// Whether bar `i` (an index into the chart's bars) is drawn as cells this frame (its
    /// candle is not drawn).
    pub fn covers(&self, i: usize) -> bool {
        self.active && self.frame.bars.binary_search_by_key(&i, |b| b.bar).is_ok()
    }

    /// Cells are drawn this frame.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The frame built this frame (tests).
    #[cfg(test)]
    pub fn frame(&self) -> &FootprintFrame {
        &self.frame
    }

    /// The first visible bar index (the candle scene starts there).
    pub fn first_bar(&self) -> usize {
        self.first_bar
    }

    /// Reserve buffers for a plot `width` px wide: on first showing, then only when the plot
    /// grows (before `prepare`, like the candle scene's buffers).
    pub fn ensure_capacity(&mut self, width: f32) {
        if !self.config.shown() {
            return;
        }
        if self.frame.cells.capacity() == 0 {
            self.frame = FootprintFrame::with_capacity(64, MAX_CELLS, MAX_ROWS);
            self.drawn = Vec::with_capacity(MAX_CELLS);
        }
        let need = (width as f64 / 30.0) as usize + 4;
        if self.frame.bars.capacity() < need {
            self.frame.bars.reserve_exact(need - self.frame.bars.len());
        }
        if self.open_close.capacity() < self.frame.bars.capacity() {
            self.open_close
                .reserve_exact(self.frame.bars.capacity() - self.open_close.len());
        }
    }

    /// Draw nothing and break no lines (the chart's studies are hidden: `prepare` and `paint`
    /// don't run then).
    pub fn reset(&mut self) {
        self.active = false;
        self.drawn.clear();
    }

    /// Horizontal line segments across `[x0, x1)` at plot-local `y` that don't cross a drawn
    /// cell (price and crosshair lines break where they would strike through numbers).
    pub fn hline_segments(&self, y: f32, x0: f32, x1: f32, mut f: impl FnMut(f32, f32)) {
        let mut cur = x0;
        if self.active {
            for r in &self.drawn {
                if !(r[2] <= y && y < r[3]) || r[1] <= cur {
                    continue;
                }
                if r[0] > cur {
                    f(cur, r[0].min(x1));
                }
                cur = cur.max(r[1]);
                if cur >= x1 {
                    return;
                }
            }
        }
        if cur < x1 {
            f(cur, x1);
        }
    }

    /// Vertical line segments across `[y0, y1)` at plot-local `x` that don't cross the drawn
    /// cell column there (a bar's cells are consecutive rows: one span). The crosshair and
    /// the synced-time line break there instead of striking through centred numbers.
    pub fn vline_segments(&self, x: f32, y0: f32, y1: f32, mut f: impl FnMut(f32, f32)) {
        let (mut top, mut bottom) = (f32::INFINITY, f32::NEG_INFINITY);
        if self.active {
            for r in &self.drawn {
                if r[0] <= x && x < r[1] {
                    top = top.min(r[2]);
                    bottom = bottom.max(r[3]);
                }
            }
        }
        if top > bottom {
            f(y0, y1);
            return;
        }
        if top > y0 {
            f(y0, top.min(y1));
        }
        if bottom < y1 {
            f(bottom.max(y0), y1);
        }
    }

    /// The decisions paint follows, checked (tests): every drawn cell's numbers fit its room,
    /// rows are tall enough for text, and no drawn cell lies under the legend block.
    #[cfg(test)]
    pub fn check_drawable(&self, vp: &Viewport) -> Result<(), String> {
        if !self.active {
            return Ok(());
        }
        let u = self.frame.row_units;
        let row_h = row_height(vp, u, self.scale);
        if row_h < MIN_TEXT_ROW_PX {
            return Err(format!("rows {row_h} px"));
        }
        for b in &self.frame.bars {
            let col = Column::of(vp, b.bar, self.scale);
            let cells = self.frame.cells_of(b);
            let room = col.text_room(self.config.mode);
            if !numbers_fit(cells, self.config.mode, self.qty, &self.metrics, room) {
                return Err(format!("bar {}: a number is wider than {room} px", b.bar));
            }
            let (ex_w, ex_h) = self.legend_block;
            for c in cells {
                let top = cell_top(vp, c.row, u, self.tick, row_h, self.scale);
                if col.strip_x < ex_w && top < ex_h && top + row_h > 0.0 {
                    return Err(format!("bar {}: a cell under the legend", b.bar));
                }
                let on = top + row_h > 0.0 && f64::from(top) < vp.height;
                if on && (top < 0.0 || f64::from(top + row_h) > vp.height) {
                    return Err(format!("bar {}: a cell cut by the plot's edge", b.bar));
                }
            }
        }
        Ok(())
    }
}

impl ChartLayer for FootprintLayer {
    fn prepare(&mut self, data: &ChartData, bars: BarsView<'_>, vp: &Viewport) {
        self.active = false;
        self.fallbacks = Fallbacks::default();
        let range = vp.visible_range(bars.len());
        self.first_bar = range.start;
        if !self.config.shown() {
            return;
        }
        if vp.bar_px < self.config.mode.min_bar_px() {
            self.fallbacks.narrow = true;
            return; // too narrow to read: candles (the legend asks to zoom in)
        }
        self.tick = data.product.price.tick.max(1);
        self.qty = data.product.qty;
        let scale = self.scale;
        // Only bars whose whole column is on the plot: one cut by an edge is a candle.
        let whole = |i: usize| {
            let c = Column::of(vp, i, scale);
            c.strip_x >= 0.0 && f64::from(c.cell_left + c.cell_w) <= vp.width
        };
        let (mut a, mut b) = (range.start, range.end);
        while a < b && !whole(a) {
            a += 1;
        }
        while b > a && !whole(b - 1) {
            b -= 1;
        }
        let px_per_unit = vp.height / (vp.price_hi - vp.price_lo).abs().max(1e-9);
        let g = fit_grouping(
            self.config.grouping.unwrap_or(1),
            self.tick,
            px_per_unit,
            MIN_ROW_PX,
        );
        let u = (g as i64).saturating_mul(self.tick);
        self.fallbacks.row_ticks = g as i64;
        let row_h = row_height(vp, u, scale);
        if row_h < MIN_TEXT_ROW_PX {
            self.fallbacks.rows_short = true;
            return;
        }
        self.frame.build(
            data.profiles.footprint(),
            bars,
            a..b,
            u,
            self.config.rule(self.qty.decimals),
        );
        self.fallbacks.no_trades = self.frame.with_data < b - a;
        // Keep only bars whose every number fits and no cell lies under the legend block;
        // the rest are candles (never a cut number, a spilled one or a box without one).
        let (mode, qty, m, tick) = (self.config.mode, self.qty, self.metrics, self.tick);
        let (ex_w, ex_h) = self.legend_block;
        // A bar with cells cut by the plot's left or right edge is a candle too: say so (a
        // covered bar must never read as one without trades).
        let edge_cut = (range.start..a).chain(b..range.end).any(|i| {
            data.profiles
                .footprint()
                .range(bars.bars[i].time, bars.end(i))
                .next()
                .is_some()
        });
        let (mut too_wide, mut under, mut at_edge) = (false, false, edge_cut);
        let cells = &self.frame.cells;
        self.frame.bars.retain(|bc| {
            let col = Column::of(vp, bc.bar, scale);
            let cs = &cells[bc.first..bc.first + bc.len];
            if !numbers_fit(cs, mode, qty, &m, col.text_room(mode)) {
                too_wide = true;
                return false;
            }
            if col.strip_x < ex_w
                && cs.iter().any(|c| {
                    let top = cell_top(vp, c.row, u, tick, row_h, scale);
                    top < ex_h && top + row_h > 0.0
                })
            {
                under = true;
                return false;
            }
            // A cell cut by the plot's top or bottom edge (a manual pan or zoom) would show
            // half its digits: the bar is a candle.
            if cs.iter().any(|c| {
                let top = cell_top(vp, c.row, u, tick, row_h, scale);
                let on = top + row_h > 0.0 && f64::from(top) < vp.height;
                on && (top < 0.0 || f64::from(top + row_h) > vp.height)
            }) {
                at_edge = true;
                return false;
            }
            true
        });
        self.fallbacks.too_wide = too_wide;
        self.fallbacks.under_legend = under;
        self.fallbacks.at_edge = at_edge;
        self.fallbacks.too_many = self.frame.truncated;
        // Count only cells on the plot (what the trader can find as `?N` or in a mixed cell).
        // Volume mode has no sides to miss: side-less volume is in its totals, with no `?N`.
        self.fallbacks.no_side = if self.config.mode == FootprintMode::Volume {
            0
        } else {
            self.frame
                .bars
                .iter()
                .flat_map(|b| self.frame.cells_of(b))
                .filter(|c| {
                    let top = cell_top(vp, c.row, u, tick, row_h, scale);
                    top + row_h > 0.0 && f64::from(top) < vp.height
                })
                .fold(0i64, |a, c| a.saturating_add(c.unknown.0))
        };
        self.open_close.clear();
        for bc in &self.frame.bars {
            if self.open_close.len() == self.open_close.capacity() {
                break;
            }
            let bar = bars.bars[bc.bar];
            self.open_close.push((bar.open, bar.close));
        }
        self.active = true;
    }

    fn paint(
        &mut self,
        plot: Bounds<Pixels>,
        vp: &Viewport,
        t: &Theme,
        window: &mut Window,
        cx: &mut App,
    ) {
        let scale = window.scale_factor();
        if self.scale != scale || self.metrics == TextMetrics::ESTIMATE {
            // Measured once (and on a scale change): glyph widths are cached by `Glyphs`.
            self.scale = scale;
            let c = t.text_main;
            self.metrics = TextMetrics {
                digit: self.glyphs.width("0", c, false, window),
                minus: self.glyphs.width("-", c, false, window),
                dot: self.glyphs.width(".", c, false, window),
                question: self.glyphs.width("?", c, false, window),
            };
        }
        self.drawn.clear();
        if !self.active {
            return;
        }
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        let hair = 1.0 / scale;
        let snap = |v: f64| lines_core::geometry::snap(v, scale);
        let rect = |x: f32, y: f32, w: f32, h: f32| {
            Bounds::new(point(px(ox + x), px(oy + y)), size(px(w), px(h)))
        };
        let u = self.frame.row_units;
        let row_h = row_height(vp, u, scale);
        let max = self.frame.max_cell.0.max(1) as f32;
        let mode = self.config.mode;
        let key = (t.name, mode, self.looks);
        let shades = match self.shades {
            Some((k, s)) if k == key => s,
            _ => {
                let s = CellShades::of(t, &self.looks, mode);
                self.shades = Some((key, s));
                s
            }
        };
        let imb = self.looks[study_style::FP_IMBALANCE];
        let (imb_sell, imb_buy) = (
            t.part_color(imb.color(), t.down, imb.opacity()),
            t.part_color(imb.color(), t.up, imb.opacity()),
        );
        // Imbalanced numbers keep their side's color only with the marks' own color.
        let own_marks = imb.color() == lines_core::style::Color::Own;
        let poc = self.looks[study_style::FP_POC];
        let poc_color = t.part_color(poc.color(), t.text_main, poc.opacity());
        let mut s = FixedStr::<32>::new();
        for bi in 0..self.frame.bars.len() {
            let b: BarCells = self.frame.bars[bi];
            let col = Column::of(vp, b.bar, scale);
            let (left, w) = (col.cell_left, col.cell_w);
            // The bar's open → close, as a candle body: its direction by shape too.
            if let Some(&(open, close)) = self.open_close.get(bi) {
                let up = close >= open;
                let color = if up { t.up } else { t.down };
                let (yo, yc) = (
                    snap(vp.price_to_y(open.0 as f64)),
                    snap(vp.price_to_y(close.0 as f64)),
                );
                let (top, h) = (yo.min(yc), (yo - yc).abs().max(hair));
                let sw = col.strip_w;
                if t.hollow.is_hollow(up) && sw >= 3.0 * hair && h >= 3.0 * hair {
                    for q in [
                        rect(col.strip_x, top, sw, hair),
                        rect(col.strip_x, top + h - hair, sw, hair),
                        rect(col.strip_x, top, hair, h),
                        rect(col.strip_x + sw - hair, top, hair, h),
                    ] {
                        window.paint_quad(fill(q, color));
                    }
                } else {
                    window.paint_quad(fill(rect(col.strip_x, top, sw, h), color));
                }
                // The open as a wedge left of the strip, pointing at it: which end the bar
                // opened at, by position and by a shape no study line has (a line crossing
                // the gap is uniform; the wedge tapers).
                let step = snap(1.0).max(1.0 / scale);
                // The gap to the previous column, less a pixel: the wedge never touches it.
                let room = vp.bar_px as f32 - (col.cell_left + col.cell_w - col.strip_x) - step;
                let cols = (room / step).floor().clamp(0.0, 3.0) as usize;
                for &(k, half) in [(1.0f32, 3.0f64), (2.0, 2.0), (3.0, 1.0)].iter().take(cols) {
                    let hh = snap(half).max(step * 0.5);
                    window.paint_quad(fill(
                        rect(col.strip_x - step * k, yo - hh, step, hh * 2.0),
                        color,
                    ));
                }
            }
            for k in 0..b.len {
                let c = self.frame.cells[b.first + k];
                let top = cell_top(vp, c.row, u, self.tick, row_h, scale);
                if top + row_h <= 0.0 || f64::from(top) >= vp.height {
                    continue; // wholly off the plot: nothing of it is drawn
                }
                if self.drawn.len() < self.drawn.capacity() {
                    self.drawn.push([left, left + w, top, top + row_h]);
                }
                let shade = (c.total.0 as f32 / max).clamp(0.08, 1.0);
                let tint = match mode {
                    FootprintMode::Delta if c.delta() > 0 => Tint::Up,
                    FootprintMode::Delta if c.delta() < 0 => Tint::Down,
                    _ => Tint::Neutral,
                };
                let bg = shades.color(tint, shade);
                // Opaque: what was drawn under the cell (profile, VWAP) never shows through it.
                let cell = rect(left, top, w, (row_h - hair).max(hair));
                window.paint_quad(fill(cell, t.bg));
                window.paint_quad(fill(cell, bg));
                // Imbalance markers: an underline under the imbalanced number (shape and
                // position, not only color). Sell × buy: exactly under the number (drawn with
                // the text below); one-number modes: under the side's half.
                let uh = snap(2.0).max(2.0 / scale); // 2 logical px, at least 2 device px
                let uy = snap(f64::from(top + row_h - uh - 2.0));
                if mode != FootprintMode::BidAsk {
                    for (on, buy, color) in [
                        (c.imbalance.sell, false, imb_sell),
                        (c.imbalance.buy, true, imb_buy),
                    ] {
                        if on {
                            let (x, uw) = imbalance_underline(w, buy);
                            window.paint_quad(fill(rect(left + x, uy, uw, uh), color));
                        }
                    }
                }
                if b.poc == Some(k) {
                    // Outlined, one device pixel.
                    let (r, bot) = (left + w - hair, top + row_h - hair);
                    for q in [
                        rect(left, top, w, hair),
                        rect(left, bot, w, hair),
                        rect(left, top, hair, row_h),
                        rect(r, top, hair, row_h),
                    ] {
                        window.paint_quad(fill(q, poc_color));
                    }
                }
                // Every drawn cell carries its numbers: `prepare` kept only bars whose
                // numbers fit and rows tall enough for them.
                let y = oy + top;
                let qty = self.qty;
                let num = |s: &mut FixedStr<32>, v: i64| {
                    s.clear();
                    let _ = qty.write(Qty(v), s);
                };
                match mode {
                    FootprintMode::BidAsk | FootprintMode::Delta if only_unknown(&c) => {
                        // Side-less volume only: `?N`, neutral, across the cell (never 0 | 0).
                        s.clear();
                        let _ = std::fmt::Write::write_char(&mut s, '?');
                        let _ = qty.write(c.unknown, &mut s);
                        // Main text color (AA on any cell shade): the `?` marks it neutral.
                        let wtxt = self.glyphs.width(s.as_str(), t.text_main, false, window);
                        self.glyphs.paint(
                            s.as_str(),
                            ox + left + (w - wtxt) * 0.5,
                            y,
                            row_h,
                            t.text_main,
                            false,
                            window,
                            cx,
                        );
                    }
                    FootprintMode::BidAsk => {
                        // Two columns: sell right-aligned to the divider, buy left-aligned.
                        let mid_x = snap(f64::from(left + w * 0.5));
                        window.paint_quad(fill(
                            rect(mid_x, top + 2.0, hair, (row_h - 4.0).max(hair)),
                            t.text_dim.opacity(0.35),
                        ));
                        let sell = if c.imbalance.sell && own_marks {
                            t.down
                        } else {
                            t.text_main
                        };
                        let buy = if c.imbalance.buy && own_marks {
                            t.up
                        } else {
                            t.text_main
                        };
                        num(&mut s, c.sell.0);
                        if c.imbalance.sell {
                            let tw = self.glyphs.width(s.as_str(), sell, true, window);
                            let x = mid_x - GUTTER - tw - 1.0;
                            window.paint_quad(fill(rect(x, uy, tw + 2.0, uh), imb_sell));
                        }
                        self.glyphs.paint_right(
                            s.as_str(),
                            ox + mid_x - GUTTER,
                            y,
                            row_h,
                            sell,
                            c.imbalance.sell,
                            window,
                            cx,
                        );
                        num(&mut s, c.buy.0);
                        if c.imbalance.buy {
                            let tw = self.glyphs.width(s.as_str(), buy, true, window);
                            let x = mid_x + GUTTER - 1.0;
                            window.paint_quad(fill(rect(x, uy, tw + 2.0, uh), imb_buy));
                        }
                        self.glyphs.paint(
                            s.as_str(),
                            ox + mid_x + GUTTER,
                            y,
                            row_h,
                            buy,
                            c.imbalance.buy,
                            window,
                            cx,
                        );
                    }
                    FootprintMode::Delta | FootprintMode::Volume => {
                        let v = if mode == FootprintMode::Delta {
                            c.delta()
                        } else {
                            c.total.0
                        };
                        num(&mut s, v);
                        let wtxt = self.glyphs.width(s.as_str(), t.text_main, false, window);
                        self.glyphs.paint(
                            s.as_str(),
                            ox + left + (w - wtxt) * 0.5,
                            y,
                            row_h,
                            t.text_main,
                            false,
                            window,
                            cx,
                        );
                    }
                    FootprintMode::Off => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{Aggressor, Bar, Capabilities, ContractKey, Month, Price, UtcMillis};
    use lines_feed::catalog;
    use lines_feed::provider::MarketEvent;
    use lines_feed::store::ChartStore;

    const MIN: i64 = 60_000;

    /// ES, a history bar at minute 0, then trades in minutes 1..=3.
    fn store() -> ChartStore {
        let es = catalog::get(catalog::ES).unwrap();
        let z26 = ContractKey::monthly(es.id, 2026, Month::new(12).unwrap());
        let s = ChartStore::new(es, Some(z26), Capabilities::default(), "test", MIN, 100);
        let t0 = UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0;
        s.update(|d| {
            d.apply(&[MarketEvent::History {
                bars: vec![Bar {
                    time: UtcMillis(t0),
                    open: Price(600_000),
                    high: Price(600_100),
                    low: Price(599_900),
                    close: Price(600_050),
                    volume: Qty(50),
                }],
                as_of: Some(UtcMillis(t0 + MIN - 1)),
            }]);
            for m in 1..=3i64 {
                for (k, side) in [Aggressor::Buy, Aggressor::Sell, Aggressor::Buy]
                    .into_iter()
                    .enumerate()
                {
                    d.apply(&[MarketEvent::Trade {
                        time: UtcMillis(t0 + m * MIN + k as i64 * 1_000),
                        price: Price(600_000 + 25 * k as i64),
                        qty: Qty(2 + k as i64),
                        aggressor: Some(side),
                    }]);
                }
            }
        });
        s
    }

    fn view(bar_px: f64) -> Viewport {
        let mut vp = Viewport::default();
        vp.set_size(800.0, 400.0);
        vp.bar_px = bar_px;
        vp.right_index = 3.0;
        vp.price_lo = 599_800.0;
        vp.price_hi = 600_200.0;
        vp.auto_price = false;
        vp
    }

    #[test]
    fn cells_where_there_are_trades_candles_elsewhere_and_when_narrow() {
        let s = store();
        let d = s.read();
        let bars = BarsView::fixed(d.series.bars(), MIN);
        assert_eq!(bars.len(), 4);
        let mut fp = FootprintLayer::new(FootprintConfig {
            mode: FootprintMode::BidAsk,
            ..Default::default()
        });
        // No buffers until shown (the chart reserves them before prepare, as here).
        assert_eq!(fp.frame().cells.capacity(), 0);
        fp.ensure_capacity(800.0);
        assert!(fp.frame().cells.capacity() > 0);
        // Wide bars: minutes 1..=3 are cells, the history minute stays a candle.
        let vp = view(80.0);
        fp.prepare(&d, bars, &vp);
        assert!(fp.is_active());
        // Bar 3 is centred on the plot's right edge: cut, so a candle (no half numbers).
        assert!(!fp.covers(0) && fp.covers(1) && fp.covers(2) && !fp.covers(3));
        let f = fp.frame();
        assert_eq!(f.bars.len(), 2);
        let b = f.bars[0];
        assert_eq!(
            f.cells_of(&b)
                .iter()
                .map(|c| (c.buy.0, c.sell.0))
                .collect::<Vec<_>>(),
            [(2, 0), (0, 3), (4, 0)]
        );
        let (label, note) = fp.legend().unwrap();
        assert_eq!(label, "Footprint · sell × buy");
        // Bar 3 has trades but is cut by the plot's edge: the legend says why it's a candle.
        assert_eq!(
            note.as_deref(),
            Some(
                "edge bars as candles · footprint from counted trades only · underline = imbalance"
            )
        );
        fp.check_drawable(&vp).unwrap();
        // Narrow bars: no cells at all; the legend asks to zoom in.
        let narrow = view(20.0);
        fp.prepare(&d, bars, &narrow);
        assert!(!fp.is_active() && !fp.covers(1));
        assert_eq!(
            fp.legend().unwrap().1.as_deref(),
            Some("zoom in to see cells · underline = imbalance")
        );
        // Off: nothing, and no legend line.
        fp.set_config(FootprintConfig::default());
        fp.prepare(&d, bars, &vp);
        assert!(!fp.is_active());
        assert!(fp.legend().is_none());
    }

    /// A store of `product` with `qty` traded per trade, three sides at three prices, every
    /// minute for `minutes` minutes from 2026-11-10 15:00Z.
    fn traded(product: lines_core::ProductId, qty: i64, minutes: i64) -> (ChartStore, i64) {
        let p = catalog::get(product).unwrap();
        let tick = p.price.tick;
        let s = ChartStore::new(p, None, Capabilities::default(), "test", MIN, 10_000);
        let t0 = UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0;
        let base = 600_000 / tick * tick;
        s.update(|d| {
            for m in 0..minutes {
                for (k, side) in [Aggressor::Buy, Aggressor::Sell, Aggressor::Buy]
                    .into_iter()
                    .enumerate()
                {
                    d.apply(&[MarketEvent::Trade {
                        time: UtcMillis(t0 + m * MIN + k as i64 * 1_000),
                        price: Price(base + tick * k as i64),
                        qty: Qty(qty),
                        aggressor: Some(side),
                    }]);
                }
            }
        });
        (s, base)
    }

    fn around(base: i64, tick: i64, bar_px: f64, right_index: f64) -> Viewport {
        let mut vp = Viewport::default();
        vp.set_size(800.0, 400.0);
        vp.bar_px = bar_px;
        vp.right_index = right_index;
        vp.price_lo = (base - 8 * tick) as f64;
        vp.price_hi = (base + 12 * tick) as f64;
        vp.auto_price = false;
        vp
    }

    /// The owner's rule: a cell with volume is never drawn without its numbers, and no number
    /// is wider than its room. Crypto quantities have 8 decimals ("0.01234567" is ~67 px);
    /// ES hour and day bars sum to 5-6 digits. Too narrow: the bar is a candle, with a reason.
    #[test]
    fn no_number_is_wider_than_its_cell() {
        // Kraken BTC: 0.01234567 per trade.
        let (s, base) = traded(catalog::BTC_USD_KR, 1_234_567, 3);
        let d = s.read();
        let tick = d.product.price.tick;
        let bars = BarsView::fixed(d.series.bars(), MIN);
        for (mode, bar_px, cells) in [
            (FootprintMode::BidAsk, 80.0, false),
            (FootprintMode::Volume, 80.0, false),
            (FootprintMode::BidAsk, 200.0, true),
            (FootprintMode::Delta, 120.0, true),
        ] {
            let mut fp = FootprintLayer::new(FootprintConfig {
                mode,
                ..Default::default()
            });
            fp.ensure_capacity(800.0);
            let vp = around(base, tick, bar_px, 3.0);
            fp.prepare(&d, bars, &vp);
            fp.check_drawable(&vp).unwrap();
            assert_eq!(fp.covers(1), cells, "{mode:?} at {bar_px} px");
            assert_eq!(fp.fallbacks().too_wide, !cells, "{mode:?} at {bar_px} px");
            if !cells {
                let note = fp.legend().unwrap().1.unwrap();
                assert!(note.contains("numbers too wide"), "{note}");
            }
        }
        drop(d);
        // ES: 4000 lots a trade, every minute for two days: hour and day bars.
        let (s, base) = traded(catalog::ES, 4_000, 2 * 24 * 60);
        let d = s.read();
        for tf in [lines_core::Timeframe::Hours(1), lines_core::Timeframe::Day] {
            let mut dv = lines_core::Derived::new(tf, 10_000);
            dv.update(&d.series, None);
            let view = dv.view();
            for (bar_px, cells) in [(80.0, false), (240.0, true)] {
                let mut fp = FootprintLayer::new(FootprintConfig {
                    mode: FootprintMode::BidAsk,
                    ..Default::default()
                });
                fp.ensure_capacity(800.0);
                let vp = around(base, 25, bar_px, 1.0);
                fp.prepare(&d, view, &vp);
                fp.check_drawable(&vp).unwrap();
                assert_eq!(fp.covers(0), cells, "{tf} at {bar_px} px");
            }
        }
    }

    /// Cells never sit under the readout and legend block: auto-fit raises the range's top,
    /// and bars a manual zoom pushes under it are candles (never shaded boxes without numbers).
    #[test]
    fn nothing_under_the_legend() {
        let (s, base) = traded(catalog::ES, 3, 3);
        let d = s.read();
        let bars = BarsView::fixed(d.series.bars(), MIN);
        let mut fp = FootprintLayer::new(FootprintConfig {
            mode: FootprintMode::BidAsk,
            ..Default::default()
        });
        fp.ensure_capacity(800.0);
        // The rows reach the top of the plot; the block covers its top-left.
        // Bars 0..=2 at x 120, 200, 280: under the block's 400 px width.
        let mut vp = around(base, 25, 80.0, 8.5);
        vp.price_hi = (base + 3 * 25) as f64;
        fp.set_legend_block(400.0, 120.0);
        fp.prepare(&d, bars, &vp);
        fp.check_drawable(&vp).unwrap();
        assert!(!fp.covers(0) && !fp.covers(1), "under the block: candles");
        assert!(fp.fallbacks().under_legend);
        // A manual pan that puts rows across the plot's top edge: those bars are candles too.
        let mut cut = around(base, 25, 80.0, 3.0);
        cut.price_hi = (base + 2 * 25) as f64 + 5.0; // the top row straddles y = 0
        fp.set_legend_block(0.0, 0.0);
        fp.prepare(&d, bars, &cut);
        fp.check_drawable(&cut).unwrap();
        assert!(!fp.covers(1) && fp.fallbacks().at_edge);
        fp.set_legend_block(400.0, 120.0);
        // Auto-fit: the top is raised clear of the block.
        clear_top(&mut vp, 120.0 + MIN_ROW_PX);
        fp.prepare(&d, bars, &vp);
        fp.check_drawable(&vp).unwrap();
        assert!(fp.covers(1) && !fp.fallbacks().under_legend);
    }

    /// Volume without an aggressor side is never dropped silently: a cell of only such volume
    /// prints `?N`, and the legend counts it.
    #[test]
    fn side_less_volume_is_flagged() {
        let es = catalog::get(catalog::ES).unwrap();
        let st = ChartStore::new(es, None, Capabilities::default(), "test", MIN, 100);
        let t0 = UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0;
        st.update(|d| {
            for m in 0..3i64 {
                d.apply(&[
                    MarketEvent::Trade {
                        time: UtcMillis(t0 + m * MIN),
                        price: Price(600_000),
                        qty: Qty(7),
                        aggressor: None,
                    },
                    MarketEvent::Trade {
                        time: UtcMillis(t0 + m * MIN + 1),
                        price: Price(600_025),
                        qty: Qty(2),
                        aggressor: Some(Aggressor::Buy),
                    },
                ]);
            }
        });
        let d = st.read();
        let bars = BarsView::fixed(d.series.bars(), MIN);
        let mut fp = FootprintLayer::new(FootprintConfig {
            mode: FootprintMode::BidAsk,
            ..Default::default()
        });
        fp.ensure_capacity(800.0);
        let vp = around(600_000, 25, 80.0, 3.0);
        fp.prepare(&d, bars, &vp);
        fp.check_drawable(&vp).unwrap();
        assert!(fp.covers(1));
        let cells = fp.frame().cells_of(&fp.frame().bars[0]);
        assert!(only_unknown(&cells[0]) && !only_unknown(&cells[1]));
        assert_eq!(fp.fallbacks().no_side, 21);
        let note = fp.legend().unwrap().1.unwrap();
        assert!(note.starts_with("? 21 without aggressor side"), "{note}");
        // With a reason too, the count comes first and the reason before the key, so the
        // capped legend line cuts the key, never the count.
        fp.fallbacks.under_legend = true;
        let note = fp.legend().unwrap().1.unwrap();
        assert_eq!(
            note,
            "? 21 without aggressor side · bars under the legend shown as candles · underline = imbalance"
        );
        // Bars cut at the plot's top: none drawn, nothing counted.
        let mut cut = around(600_000, 25, 80.0, 3.0);
        cut.price_hi = 600_000.0 + 5.0;
        fp.prepare(&d, bars, &cut);
        let note = fp.legend().unwrap().1.unwrap();
        assert!(note.contains("edge bars as candles"), "{note}");
        assert!(
            !note.contains("? "),
            "{note}: nothing drawn, nothing to count"
        );
        let mut wide = vp;
        wide.right_index = 2.0; // bar 2 cut at the right edge: a candle; bars 0, 1 cells
        fp.prepare(&d, bars, &wide);
        let note = fp.legend().unwrap().1.unwrap();
        assert!(note.starts_with("? 14 without aggressor side"), "{note}");
        assert!(
            note.ends_with("underline = imbalance"),
            "the key last: {note}"
        );
        // Delta mode prints `?N` for side-less cells too (never a balanced-looking 0).
        let mut dm = FootprintLayer::new(FootprintConfig {
            mode: FootprintMode::Delta,
            ..Default::default()
        });
        dm.ensure_capacity(800.0);
        dm.prepare(&d, bars, &vp);
        dm.check_drawable(&vp).unwrap();
        assert!(dm.covers(1));
        assert_eq!(dm.fallbacks().no_side, 21);
        // Volume mode shows side-less volume in its totals: nothing to flag.
        let mut vm = FootprintLayer::new(FootprintConfig {
            mode: FootprintMode::Volume,
            ..Default::default()
        });
        vm.ensure_capacity(800.0);
        vm.prepare(&d, bars, &vp);
        vm.check_drawable(&vp).unwrap();
        assert_eq!(vm.fallbacks().no_side, 0);
        assert_eq!(
            vm.legend().unwrap().1.as_deref(),
            Some("underline = imbalance")
        );
    }

    /// The imbalance mark is a horizontal underline inside the cell, under its side's half:
    /// never at the column's left edge where the vertical open/close strip is.
    #[test]
    fn imbalance_marks_sit_under_their_number() {
        for w in [55.0f32, 60.0, 120.0, 200.0] {
            let (sx, sw) = imbalance_underline(w, false);
            let (bx, bw) = imbalance_underline(w, true);
            assert!(sx >= PAD && sx + sw <= w * 0.5, "sell under the left half");
            assert!(
                bx >= w * 0.5 && bx + bw <= w - PAD,
                "buy under the right half"
            );
            assert!(
                sw >= 8.0 && bw >= 8.0,
                "a horizontal mark, unlike the 3 px strip"
            );
        }
    }

    #[test]
    fn clear_top_puts_the_old_top_below_the_block() {
        let mut vp = Viewport::default();
        vp.set_size(800.0, 600.0);
        (vp.price_lo, vp.price_hi) = (1_000.0, 2_000.0);
        clear_top(&mut vp, 60.0);
        assert!((vp.price_to_y(2_000.0) - 60.0).abs() < 1e-9);
        assert!(
            (vp.price_to_y(1_000.0) - 600.0).abs() < 1e-9,
            "the bottom stays"
        );
        let before = vp;
        clear_top(&mut vp, 400.0); // more than half the plot: no
        assert_eq!(vp, before);
    }

    #[test]
    fn lines_break_across_cells() {
        let mut fp = FootprintLayer::new(FootprintConfig {
            mode: FootprintMode::BidAsk,
            ..Default::default()
        });
        fp.drawn = vec![
            [100.0, 160.0, 10.0, 30.0],
            [200.0, 260.0, 10.0, 30.0],
            [300.0, 360.0, 40.0, 60.0],
        ];
        let segs = |fp: &FootprintLayer, y: f32, x0: f32, x1: f32| {
            let mut v = Vec::new();
            fp.hline_segments(y, x0, x1, |a, b| v.push((a, b)));
            v
        };
        // Inactive: the whole line.
        assert_eq!(segs(&fp, 20.0, 0.0, 500.0), [(0.0, 500.0)]);
        fp.active = true;
        assert_eq!(
            segs(&fp, 20.0, 0.0, 500.0),
            [(0.0, 100.0), (160.0, 200.0), (260.0, 500.0)]
        );
        assert_eq!(segs(&fp, 50.0, 0.0, 500.0), [(0.0, 300.0), (360.0, 500.0)]);
        assert_eq!(
            segs(&fp, 20.0, 120.0, 230.0),
            [(160.0, 200.0)],
            "starts inside a cell"
        );
        assert_eq!(segs(&fp, 35.0, 0.0, 500.0), [(0.0, 500.0)], "between rows");
        // Vertical: the column at x 120 spans y 10..30.
        let mut v = Vec::new();
        fp.vline_segments(120.0, 0.0, 100.0, |a, b| v.push((a, b)));
        assert_eq!(v, [(0.0, 10.0), (30.0, 100.0)]);
        v.clear();
        fp.vline_segments(180.0, 0.0, 100.0, |a, b| v.push((a, b)));
        assert_eq!(v, [(0.0, 100.0)], "between columns");
    }

    /// The minimum is in whole units of the product's quantity: 5 contracts on ES, 5 BTC on a
    /// Kraken pair (8 decimals), never 5e-8 BTC.
    #[test]
    fn the_imbalance_minimum_is_in_whole_units() {
        let c = FootprintConfig::default();
        assert_eq!(c.rule(0).min_volume, Qty(5));
        assert_eq!(c.rule(8).min_volume, Qty(500_000_000));
        let kraken = catalog::get(catalog::BTC_USD_KR).unwrap();
        let mut s = FixedStr::<32>::new();
        kraken.qty.write(Qty(50_000_000), &mut s).unwrap();
        assert_eq!(s.as_str(), "0.5", "cells print through the quantity format");
    }

    #[test]
    fn config_is_optional_and_round_trips() {
        let c = FootprintConfig::default();
        assert!(c.is_default() && !c.shown());
        let json = serde_json::to_string(&FootprintConfig {
            mode: FootprintMode::BidAsk,
            ..c
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"mode":"bid_ask","imbalance_pct":300,"imbalance_min":5}"#
        );
        let back: FootprintConfig = serde_json::from_str(r#"{"mode":"delta"}"#).unwrap();
        assert_eq!(back.mode, FootprintMode::Delta);
        assert_eq!(back.imbalance_pct, 300, "missing fields take the defaults");
    }

    /// A cells' color and opacity as styled (`looks` otherwise unstyled).
    fn styled(color: Color, opacity: u8, own_marks: bool) -> [Look; 3] {
        use crate::style_editor::PartStyle;
        let mut looks = Look::defaults(&FOOTPRINT);
        if let PartStyle::Fill(f) = &mut looks[study_style::FP_CELLS].style {
            f.color = color;
            f.opacity = opacity;
        }
        if !own_marks && let PartStyle::Line(l) = &mut looks[study_style::FP_IMBALANCE].style {
            l.color = Color::Palette(1);
        }
        looks
    }

    /// B1 (decision 0026): every number a cell carries (the main text color; in sell × buy
    /// with the marks' own color, the up and down colors) stays AA on every shade of its
    /// cell, whatever the cells' color and opacity, in every theme, palette and mode; a tint
    /// that isn't held back is exactly the control's.
    #[test]
    fn cell_numbers_stay_readable_on_any_cell_color() {
        let mut colors = vec![Color::Own];
        colors.extend((0..4).map(Color::Palette));
        for hex in [
            0xffffff, 0x000000, 0xc4c8d6, 0x808080, 0xf719cb, 0x3fcf9a, 0xff6b7f, 0x4a4f63,
            0xe2c46a, 0x1a1a1a, 0xfff7c0,
        ] {
            colors.push(Color::Custom(Rgb::hex(hex)));
        }
        let modes = [
            FootprintMode::BidAsk,
            FootprintMode::Delta,
            FootprintMode::Volume,
        ];
        for t in Theme::all() {
            let bg = crate::theme::rgb_of(t.bg);
            for mode in modes {
                // Unstyled, only sell × buy holds back (an imbalanced number in its side's
                // color on the strongest neutral cells was below AA before styles).
                let plain = CellShades::of(&t, &Look::defaults(&FOOTPRINT), mode);
                if mode != FootprintMode::BidAsk {
                    assert!(
                        !plain.held_in(mode),
                        "{} {mode:?}: unstyled cells held back",
                        t.name
                    );
                }
                for &color in &colors {
                    for opacity in [10, 25, 50, 75, 100] {
                        for own_marks in [true, false] {
                            let looks = styled(color, opacity, own_marks);
                            let s = CellShades::of(&t, &looks, mode);
                            let mut texts = vec![t.text_main];
                            if mode == FootprintMode::BidAsk && own_marks {
                                texts.extend([t.up, t.down]);
                            }
                            let tints: &[Tint] = if mode == FootprintMode::Delta {
                                &[Tint::Neutral, Tint::Up, Tint::Down]
                            } else {
                                &[Tint::Neutral]
                            };
                            for &tint in tints {
                                for shade in [0.0, 0.08, 0.5, 1.0] {
                                    let c = s.color(tint, shade);
                                    let seen = blend(crate::theme::rgb_of(c), c.a, bg);
                                    for x in &texts {
                                        let r = lines_core::style::contrast(
                                            crate::theme::rgb_of(*x),
                                            seen,
                                        );
                                        assert!(
                                            r >= 4.5,
                                            "{} {mode:?} {color:?} {opacity}% {tint:?} \
                                             shade {shade}: {r:.2}:1",
                                            t.name
                                        );
                                    }
                                }
                            }
                            if !s.held[Tint::Neutral as usize] {
                                // Not held back: the control's opacity exactly.
                                let (lo, span) = NEUTRAL_SHADE;
                                let want = (lo + span) * f32::from(opacity) / 100.0;
                                let got = s.color(Tint::Neutral, 1.0).a;
                                assert!((got - want).abs() < 1e-4, "{got} vs {want}");
                            }
                        }
                    }
                }
            }
        }
    }

    /// A light custom cell color on dark is held back and the editor says where, naming the
    /// theme and palette.
    #[test]
    fn a_held_cell_color_is_warned_about() {
        let themes = Theme::all();
        let ctxs: Vec<_> = themes.iter().map(Theme::study_ctx).collect();
        for mode in [
            FootprintMode::Off,
            FootprintMode::BidAsk,
            FootprintMode::Delta,
        ] {
            assert_eq!(
                cells_warning(&Look::defaults(&FOOTPRINT), mode, &themes, &ctxs),
                None
            );
        }
        let white = styled(Color::Custom(Rgb::hex(0xffffff)), 100, true);
        let w = cells_warning(&white, FootprintMode::Delta, &themes, &ctxs)
            .expect("white cells on dark are held");
        assert!(w.text.contains("in delta,"), "{}", w.text);
        assert!(w.text.contains("dark theme, both palettes"), "{}", w.text);
        assert!(w.text.contains("4.5:1"), "{}", w.text);
        assert!(w.text.starts_with("Shown fainter"), "{}", w.text);
        let dark = themes.iter().find(|t| t.is_dark()).unwrap();
        assert!(CellShades::of(dark, &white, FootprintMode::Delta).held_in(FootprintMode::Delta));
    }

    /// N4: the warning is for the chart's mode (off: each mode held, named), and holds the
    /// unstyled cells already have (sell × buy, own color) aren't warned about.
    #[test]
    fn cells_warning_follows_the_charts_mode() {
        let themes = Theme::all();
        let ctxs: Vec<_> = themes.iter().map(Theme::study_ctx).collect();
        use crate::style_editor::PartStyle;
        // Own color, only fainter: sell × buy's unstyled hold isn't the user's doing.
        for op in [50, 75] {
            let mut looks = Look::defaults(&FOOTPRINT);
            if let PartStyle::Fill(f) = &mut looks[study_style::FP_CELLS].style {
                f.opacity = op;
            }
            for mode in [
                FootprintMode::BidAsk,
                FootprintMode::Volume,
                FootprintMode::Off,
            ] {
                let w = cells_warning(&looks, mode, &themes, &ctxs);
                assert_eq!(w, None, "own {op}% {mode:?}");
            }
        }
        // White on dark: held in sell × buy and delta, not in volume (main-color numbers keep
        // AA on it); off names each mode held, a mode only itself.
        let white = styled(Color::Custom(Rgb::hex(0xffffff)), 100, true);
        let off = cells_warning(&white, FootprintMode::Off, &themes, &ctxs).unwrap();
        assert!(
            off.text.contains("in sell × buy and delta,"),
            "{}",
            off.text
        );
        assert_eq!(
            cells_warning(&white, FootprintMode::Volume, &themes, &ctxs),
            None
        );
        let bid = cells_warning(&white, FootprintMode::BidAsk, &themes, &ctxs).unwrap();
        assert!(bid.text.contains("in sell × buy,"), "{}", bid.text);
        assert!(!bid.text.contains("delta"), "{}", bid.text);
    }

    /// N2/N5: unstyled sell × buy cells are held back (the side-colored numbers on the
    /// strongest neutral cells), by exactly what `CellShades`' doc says; every unstyled tint
    /// in every mode keeps some shading (never collapsed to 0).
    #[test]
    fn unstyled_sell_buy_caps() {
        let plain = Look::defaults(&FOOTPRINT);
        let want = [
            ("dark", 0.223),
            ("dark-colorblind", 0.267),
            ("light", 0.109),
            ("light-colorblind", 0.140),
        ];
        for t in Theme::all() {
            for mode in [
                FootprintMode::BidAsk,
                FootprintMode::Delta,
                FootprintMode::Volume,
            ] {
                let s = CellShades::of(&t, &plain, mode);
                for tint in CellShades::tints(mode) {
                    assert!(s.strongest(*tint) > 0.0, "{} {mode:?} {tint:?}", t.name);
                }
            }
            let got = CellShades::of(&t, &plain, FootprintMode::BidAsk).strongest(Tint::Neutral);
            println!("{}: {got:.3}", t.name);
            let w = want.iter().find(|(n, _)| *n == t.name).unwrap().1;
            assert!((got - w).abs() < 0.006, "{}: {got:.3} vs doc {w}", t.name);
        }
    }
}
