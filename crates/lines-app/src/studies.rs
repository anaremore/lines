//! A chart pane's indicators (roadmap 5c): VWAPs from the shared indicator engine, drawn in the
//! plot, and cumulative delta (roadmap 10e) drawn as candles in a sub-pane below it
//! (`crate::subpane`), with a legend.
//!
//! The pane owns a [`Studies`]: one [`IndicatorConfig`] per indicator (what the workspace
//! saves) and an engine handle for each. Paint reads each handle's cell with `try_lock`, builds
//! the visible columns into a fixed buffer (`lines_indicators::render`, no allocation) and draws
//! a smooth anti-aliased line and band fills through them. Nothing is computed on the UI thread.
//!
//! Styles (decision 0026, `crate::study_style`): each study's parts are resolved into
//! [`Look`]s when its configuration changes; paint only reads them (no allocation of ours).
//!
//! Menu keys: ↑↓ select · Enter add (on "+ VWAP") · R reset (ETH → RTH → week → month →
//! anchored at the bar under the crosshair, else the newest bar) · B bands (off → ±1 → ±1,2 →
//! ±1,2,3) · C colour · Delete remove · Esc / Ctrl+I close.

use std::cell::Cell;

use gpui::{
    AnyElement, Hsla, IntoElement, ParentElement, PathBuilder, Pixels, Styled, Window, div, point,
    px,
};
use lines_core::style::{Color, LineType};
use lines_core::{BarsView, Product, Viewport};
use lines_feed::indicators::{FeedInfo, IndicatorHandle, engine};
use lines_indicators::render::{Column, DeltaCandle, build_columns, build_delta_candles};
use lines_indicators::{Bands, IndicatorConfig, Reset};

use crate::study_style::{self, Look};
use crate::subpane::Scale;
use crate::theme::Theme;

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
    error: Option<String>,
    /// The band fill as drawn under the candles (`Theme::under_candles`) for (theme, fill as
    /// styled, nested bands): computed when one of them changes, not every frame.
    fill_cap: Cell<Option<FillCap>>,
}

/// A fill color from theme tokens or the study's style.
type FillColor = Hsla;

type FillCap = (&'static str, Hsla, u8, FillColor);

impl Study {
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
    }
}

/// A VWAP's line: its study color index and its style's line color (`Theme::study_line`
/// makes the opaque color its legend shows and its bands follow).
fn vwap_line(c: &IndicatorConfig, looks: &[Look]) -> (u8, Color) {
    let index = match c {
        IndicatorConfig::Vwap { color, .. } => *color,
        IndicatorConfig::CumDelta { .. } => 0,
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
                    config,
                    handle: None,
                    columns: Vec::new(),
                    candles: Vec::new(),
                    extent: None,
                    newest: None,
                    approx: false,
                    error: None,
                    fill_cap: Cell::new(None),
                })
                .collect(),
            feed: None,
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
            s.handle = (!hidden).then(|| engine().acquire(feed, s.config.spec(feed.period_ms)));
        }
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
    pub fn prepare(&mut self, bars: BarsView<'_>, vp: &Viewport, max_columns: usize) {
        for s in &mut self.list {
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
                }
                IndicatorConfig::CumDelta { .. } => {
                    let range = vp.visible_range(bars.len());
                    s.extent = build_delta_candles(&out.candles, bars, range, &mut s.candles);
                    s.newest = out.candles.last().map(|p| p.close);
                    s.approx = s.candles.iter().any(|c| c.approx);
                }
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
    pub fn paint(&self, origin: (f32, f32), theme: &Theme, window: &mut Window) {
        self.paint_layer(origin, theme, window, false);
    }

    /// The band fills, painted before the candles (with the level zones).
    pub fn paint_back(&self, origin: (f32, f32), theme: &Theme, window: &mut Window) {
        self.paint_layer(origin, theme, window, true);
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
            let band_color = theme.part_color(band_line.color, base, band_line.opacity);
            let line_dash = dash(line.line, line_px);
            let band_dash = dash(band_line.line, band_px);
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
                    if !back {
                        // Its edges, when shown (styled bands).
                        for edge in [true, false] {
                            let y = |c: &Column| if edge { c.up[kk] } else { c.down[kk] };
                            let mut b = stroke(px(band_px), &band_dash);
                            b.move_to(at(xs(0, col(0)), y(col(0))));
                            for i in 1..n {
                                b.line_to(at(xs(i, col(i)), y(col(i))));
                            }
                            if let Ok(path) = b.build() {
                                window.paint_path(path, band_color);
                            }
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
                // The line. Approximate values (from bars) are fainter; the legend says so.
                let line = if run[0].approx {
                    line_color.opacity(0.5)
                } else {
                    line_color
                };
                let mut b = stroke(thick, &line_dash);
                b.move_to(at(xs(0, col(0)), col(0).y));
                for i in 1..n {
                    b.line_to(at(xs(i, col(i)), col(i).y));
                }
                if let Ok(path) = b.build() {
                    window.paint_path(path, line);
                }
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

    /// The sub-pane's title (its study's reset), and whether its values are approximate; the
    /// title sits in the sub-pane, beside what it names, not in the price plot's legend.
    pub fn subpane_title(&self) -> Option<(lines_indicators::DeltaReset, bool)> {
        self.list.iter().find_map(|s| match &s.config {
            IndicatorConfig::CumDelta { spec, display, .. } if !display.hidden && display.label => {
                Some((spec.reset, s.approx))
            }
            _ => None,
        })
    }

    /// A shown study needs the sub-pane (cumulative delta).
    pub fn has_subpane(&self) -> bool {
        self.list
            .iter()
            .any(|s| s.config.in_subpane() && !s.config.display().hidden)
    }

    /// This frame's sub-pane scale (cumulative delta's visible extremes), for `height` px.
    pub fn subpane_scale(&self, height: f32) -> Option<Scale> {
        let s = self
            .list
            .iter()
            .find(|s| s.config.in_subpane() && !s.config.display().hidden)?;
        let (lo, hi) = s.extent?;
        Some(Scale::fit(lo, hi, height))
    }

    /// The newest sub-pane value (cumulative delta's latest close, wherever the view is), for
    /// its axis tag.
    pub fn subpane_last(&self) -> Option<i64> {
        self.list
            .iter()
            .find(|s| s.config.in_subpane() && !s.config.display().hidden)?
            .newest
    }

    /// Cumulative delta's candles in the sub-pane (`origin`: its top-left; x from the chart's
    /// viewport, so bars line up with the price plot). Quads only: no allocation.
    pub fn paint_subpane(
        &self,
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
        for s in &self.list {
            if !s.config.in_subpane() || s.config.display().hidden {
                continue;
            }
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
                        window.paint_quad(gpui::fill(
                            rect(bx + dx, top, w, hair.max(snap(1.0))),
                            col,
                        ));
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
            .px(px(8.0))
            .py(px(4.0))
            .bg(theme.label_bg)
            .text_size(px(12.0));
        for s in self.list.iter().filter(shown) {
            let IndicatorConfig::Vwap { spec, .. } = &s.config else {
                continue;
            };
            let (index, line_c) = vwap_line(&s.config, &s.looks);
            let c = theme.study_line(index, line_c);
            // The name in the line's color where that is AA on the legend box (a custom color
            // may not be), else the theme's text; the swatch always shows the color.
            let text = theme.legend_text(c);
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().w(px(12.0)).h(px(3.0)).bg(c))
                .child(div().text_color(text).child(format!(
                    "VWAP {}{}",
                    spec.reset.label(),
                    bands_label(spec.bands)
                )));
            if let Some(e) = &s.error {
                row = row.child(div().text_color(theme.text_main).child(e.clone()));
            } else if no_rth && spec.reset == Reset::Rth {
                row = row.child(
                    div()
                        .text_color(theme.text_dim)
                        .child(format!("no RTH for {}", product.symbol)),
                );
            } else if s
                .handle
                .as_ref()
                .and_then(|h| h.cell().try_read())
                .map_or(s.approx, |o| o.points.last().is_some_and(|p| p.approx))
            {
                // Never silently mixed: say when values come from bars, not trades.
                row = row.child(div().text_color(theme.text_dim).child("≈ from bars"));
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
