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
//! Menu keys: ↑↓ select · Enter add (on "+ VWAP") · R reset (ETH → RTH → week → month →
//! anchored at the bar under the crosshair, else the newest bar) · B bands (off → ±1 → ±1,2 →
//! ±1,2,3) · C colour · Delete remove · Esc / Ctrl+I close.

use std::cell::Cell;

use gpui::{
    AnyElement, Hsla, IntoElement, ParentElement, PathBuilder, Pixels, Styled, Window, div, point,
    px,
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
                    vol: VolumeFrame::default(),
                    ext: ExtFrame::default(),
                    error: None,
                    fill_cap: Cell::new(None),
                    overlay_cap: Cell::new(None),
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
            s.handle = if hidden {
                None
            } else {
                s.config
                    .spec(feed.period_ms)
                    .map(|spec| engine().acquire(feed, spec))
            };
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
    /// `fp`: the store's per-minute footprints (Volume's buy/sell split on time bars).
    pub fn prepare(
        &mut self,
        bars: BarsView<'_>,
        fp: &Footprint,
        levels: &crate::levels_layer::LevelsLayer,
        vp: &Viewport,
        max_columns: usize,
    ) {
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
    pub fn paint(&self, origin: (f32, f32), vp: &Viewport, theme: &Theme, window: &mut Window) {
        let _ = vp; // the overlay paints entirely under the candles (`paint_back`)
        self.paint_layer(origin, theme, window, false);
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
                        vp.width as f32,
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
            // The name in the line's color where that is AA on the legend box (a custom color
            // may not be), else the theme's text; the swatch always shows the color.
            let text = theme.legend_text(c);
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().w(crate::zoom::ui(12.0)).h(crate::zoom::ui(3.0)).bg(c))
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
                id: "speed",
                name: "Speed",
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
        let mut st = Studies::new(vec![volume(false), ext("osc"), ext("speed")]);
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
