//! A chart pane's indicators (roadmap 5c): VWAPs from the shared indicator engine, drawn in the
//! plot, with a legend and a small keyboard-first menu (Ctrl+I).
//!
//! The pane owns a [`Studies`]: one [`IndicatorConfig`] per indicator (what the workspace
//! saves) and an engine handle for each. Paint reads each handle's cell with `try_lock`, builds
//! the visible columns into a fixed buffer (`lines_indicators::render`, no allocation) and draws
//! a smooth anti-aliased line and band fills through them. Nothing is computed on the UI thread.
//!
//! Menu keys: ↑↓ select · Enter add (on "+ VWAP") · R reset (ETH → RTH → week → month →
//! anchored at the bar under the crosshair, else the newest bar) · B bands (off → ±1 → ±1,2 →
//! ±1,2,3) · C colour · Delete remove · Esc / Ctrl+I close.

use gpui::{
    AnyElement, IntoElement, ParentElement, PathBuilder, Pixels, Styled, Window, div, point, px,
};
use lines_core::{Product, Viewport};
use lines_feed::ChartData;
use lines_feed::indicators::{FeedInfo, IndicatorHandle, engine};
use lines_indicators::render::{Column, build_columns};
use lines_indicators::{Bands, IndicatorConfig, Reset};

use crate::theme::Theme;

struct Study {
    config: IndicatorConfig,
    handle: Option<IndicatorHandle>,
    columns: Vec<Column>,
    /// The newest value is approximate (from bars); shown in the legend.
    approx: bool,
    error: Option<String>,
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

impl Studies {
    pub fn new(configs: Vec<IndicatorConfig>) -> Studies {
        Studies {
            list: configs
                .into_iter()
                .map(|config| Study {
                    config,
                    handle: None,
                    columns: Vec::new(),
                    approx: false,
                    error: None,
                })
                .collect(),
            feed: None,
        }
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
            let IndicatorConfig::Vwap { display, .. } = &s.config;
            s.handle =
                (!display.hidden).then(|| engine().acquire(feed, s.config.spec(feed.period_ms)));
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
    pub fn prepare(&mut self, d: &ChartData, vp: &Viewport, max_columns: usize) {
        let bars = d.series.bars();
        for s in &mut self.list {
            if s.columns.capacity() < max_columns {
                // Only when the plot grows wider than ever before (as the candle scene).
                s.columns.reserve(max_columns - s.columns.len());
            }
            let Some(h) = &s.handle else { continue };
            let Some(out) = h.cell().try_read() else {
                continue; // the engine is writing: keep last frame's columns
            };
            let IndicatorConfig::Vwap { spec, .. } = &s.config;
            build_columns(&out.points, bars, vp, spec.bands, &mut s.columns);
            s.approx = out.points.last().is_some_and(|p| p.approx);
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
    pub fn paint(&self, origin: (f32, f32), theme: &Theme, window: &mut Window) {
        let (ox, oy) = (origin.0, origin.1);
        let at = |x: f32, y: f32| point(px(ox + x), px(oy + y));
        for s in &self.list {
            let IndicatorConfig::Vwap { display, .. } = &s.config;
            if display.hidden {
                continue;
            }
            let thick = px(match display.width {
                1 => 1.25,
                3 => 2.5,
                _ => 1.75,
            });
            let IndicatorConfig::Vwap { spec, color, .. } = &s.config;
            let base = theme.study[*color as usize % theme.study.len()];
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
                    if !spec.bands.has(k) {
                        continue;
                    }
                    let kk = k as usize - 1;
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
                        window.paint_path(path, base.opacity(0.06));
                    }
                }
                // The line. Approximate values (from bars) are fainter; the legend says so.
                let line = if run[0].approx {
                    base.opacity(0.5)
                } else {
                    base
                };
                let mut b = PathBuilder::stroke(thick);
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

    /// The legend under the readout: one row per study (render time, not paint), on a
    /// `label_bg` box so lines and profile rows never run through the text.
    pub fn legend(&self, theme: &Theme, product: &Product) -> Option<AnyElement> {
        let shown = |s: &&Study| {
            let IndicatorConfig::Vwap { display, .. } = &s.config;
            !display.hidden && display.label
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
            let IndicatorConfig::Vwap { spec, color, .. } = &s.config;
            let c = theme.study[*color as usize % theme.study.len()];
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().w(px(12.0)).h(px(3.0)).bg(c))
                .child(div().text_color(c).child(format!(
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
