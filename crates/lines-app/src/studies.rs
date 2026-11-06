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
    AnyElement, Context, FontWeight, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, PathBuilder, Pixels, Styled, Window, div, point, prelude::*, px,
};
use lines_core::{Product, Viewport};
use lines_feed::ChartData;
use lines_feed::indicators::{FeedInfo, IndicatorHandle, engine};
use lines_indicators::render::{Column, build_columns};
use lines_indicators::{Bands, IndicatorConfig, Reset, VwapSpec};

use crate::chart_view::ChartView;
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
    /// The menu is open on this row (`list.len()` = the "+ VWAP" row).
    menu: Option<usize>,
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

fn next_bands(b: Bands) -> Bands {
    Bands(match b.0 & 0b111 {
        0 => 0b001,
        0b001 => 0b011,
        0b011 => 0b111,
        _ => 0,
    })
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
            menu: None,
        }
    }

    /// What the workspace saves for this chart.
    pub fn configs(&self) -> Vec<IndicatorConfig> {
        self.list.iter().map(|s| s.config.clone()).collect()
    }

    /// (Re)subscribe every study on `feed` (a new chart, or the chart switched symbol).
    pub fn attach(&mut self, feed: FeedInfo) {
        self.feed = Some(feed);
        for s in &mut self.list {
            s.handle = Some(engine().acquire(feed, s.config.spec(feed.period_ms)));
        }
    }

    fn resubscribe(&mut self, i: usize) {
        if let (Some(feed), Some(s)) = (self.feed, self.list.get_mut(i)) {
            s.handle = Some(engine().acquire(feed, s.config.spec(feed.period_ms)));
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
        let thick = px(1.75);
        for s in &self.list {
            let IndicatorConfig::Vwap { spec, color } = &s.config;
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

    /// The legend under the readout: one row per study (render time, not paint).
    pub fn legend(&self, theme: &Theme, product: &Product) -> Option<AnyElement> {
        if self.list.is_empty() {
            return None;
        }
        let no_rth = lines_feed::catalog::sessions(product.id).is_some_and(|c| !c.has_rth());
        let mut col = div()
            .absolute()
            .top(px(30.0))
            .left(px(8.0))
            .flex()
            .flex_col()
            .gap_1()
            .text_size(px(12.0));
        for s in &self.list {
            let IndicatorConfig::Vwap { spec, color } = &s.config;
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

    pub fn menu_open(&self) -> bool {
        self.menu.is_some()
    }

    pub fn toggle_menu(&mut self) {
        self.menu = match self.menu {
            Some(_) => None,
            None => Some(self.list.len()),
        };
    }

    fn add(&mut self) {
        let used: Vec<u8> = self
            .configs()
            .iter()
            .map(|c| match c {
                IndicatorConfig::Vwap { color, .. } => *color,
            })
            .collect();
        let color = (0..4u8).find(|c| !used.contains(c)).unwrap_or(0);
        self.list.push(Study {
            config: IndicatorConfig::Vwap {
                spec: VwapSpec::default(),
                color,
            },
            handle: None,
            columns: Vec::new(),
            approx: false,
            error: None,
        });
        self.resubscribe(self.list.len() - 1);
    }

    /// Change study `i`'s config and resubscribe (a new spec is a new shared node).
    fn edit(&mut self, i: usize, f: impl FnOnce(&mut IndicatorConfig)) {
        if let Some(s) = self.list.get_mut(i) {
            f(&mut s.config);
            s.error = None;
            s.columns.clear();
            self.resubscribe(i);
        }
    }

    /// Menu keys; `anchor`: the bar an anchored VWAP would start at. Returns whether the key
    /// was the menu's.
    pub fn key(&mut self, ev: &KeyDownEvent, anchor: Option<i64>) -> bool {
        let Some(sel) = self.menu else { return false };
        let k = &ev.keystroke;
        let rows = self.list.len() + 1;
        match k.key.as_str() {
            "escape" => self.menu = None,
            "i" if k.modifiers.control => self.menu = None,
            "up" => self.menu = Some((sel + rows - 1) % rows),
            "down" => self.menu = Some((sel + 1) % rows),
            "enter" if sel == self.list.len() => {
                self.add();
                self.menu = Some(self.list.len() - 1);
            }
            "delete" | "backspace" if sel < self.list.len() => {
                self.list.remove(sel);
                self.menu = Some(sel.min(self.list.len()));
            }
            "r" if sel < self.list.len() => self.edit(sel, |c| {
                let IndicatorConfig::Vwap { spec, .. } = c;
                spec.reset = match spec.reset {
                    Reset::Eth => Reset::Rth,
                    Reset::Rth => Reset::Weekly,
                    Reset::Weekly => Reset::Monthly,
                    Reset::Monthly => match anchor {
                        Some(from) => Reset::Anchored { from },
                        None => Reset::Eth,
                    },
                    Reset::Anchored { .. } => Reset::Eth,
                };
            }),
            "b" if sel < self.list.len() => self.edit(sel, |c| {
                let IndicatorConfig::Vwap { spec, .. } = c;
                spec.bands = next_bands(spec.bands);
            }),
            "c" if sel < self.list.len() => {
                if let Some(s) = self.list.get_mut(sel) {
                    let IndicatorConfig::Vwap { color, .. } = &mut s.config;
                    *color = (*color + 1) % 4;
                }
            }
            _ => {}
        }
        true
    }

    /// The open menu (render time). Rows are clickable too.
    pub fn menu_overlay(&self, theme: &Theme, cx: &mut Context<ChartView>) -> Option<AnyElement> {
        let sel = self.menu?;
        let t = theme;
        let mut rows = div().flex().flex_col().py_1();
        for i in 0..=self.list.len() {
            let label = match self.list.get(i) {
                Some(s) => {
                    let IndicatorConfig::Vwap { spec, .. } = &s.config;
                    match spec.reset {
                        Reset::Anchored { from } => format!(
                            "VWAP anchored at {}{}",
                            lines_core::UtcMillis(from),
                            bands_label(spec.bands)
                        ),
                        r => format!("VWAP {}{}", r.label(), bands_label(spec.bands)),
                    }
                }
                None => "+ VWAP".to_owned(),
            };
            let selected = i == sel;
            rows = rows.child(
                div()
                    .id(("study-row", i))
                    .px_3()
                    .py_1()
                    .border_l_2()
                    .border_color(if selected {
                        t.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .when(selected, |d| d.bg(t.accent.opacity(0.16)))
                    .text_color(t.text_main)
                    .when(i < self.list.len(), |d| {
                        let IndicatorConfig::Vwap { color, .. } = &self.list[i].config;
                        d.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .w(px(12.0))
                                        .h(px(3.0))
                                        .bg(t.study[*color as usize % 4]),
                                )
                                .child(label.clone()),
                        )
                    })
                    .when(i == self.list.len(), |d| d.child(label.clone()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            view.studies_click(i, cx);
                        }),
                    ),
            );
        }
        Some(
            div()
                .absolute()
                .top(px(24.0))
                .right(px(90.0))
                .child(
                    div()
                        .id("study-menu")
                        .occlude()
                        .w(px(340.0))
                        .flex()
                        .flex_col()
                        .bg(t.panel)
                        .border_1()
                        .border_color(t.axis_line)
                        .rounded_md()
                        .text_size(px(13.0))
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(t.axis_line)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(t.text_main)
                                .child("Indicators"),
                        )
                        .child(rows)
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .border_t_1()
                                .border_color(t.axis_line)
                                .text_size(px(11.0))
                                .text_color(t.text_dim)
                                .child("↑↓ select  Enter add  R reset  B bands  C colour  Del remove  Esc close"),
                        ),
                )
                .into_any_element(),
        )
    }

    /// A click on menu row `i`: select it; the "+ VWAP" row adds one.
    pub fn click(&mut self, i: usize) {
        if i == self.list.len() {
            self.add();
            self.menu = Some(self.list.len() - 1);
        } else {
            self.menu = Some(i);
        }
    }

    /// Bounds hint for the column buffers: the most bars a plot `width` px wide can show.
    pub fn max_columns(width: Pixels) -> usize {
        f32::from(width) as usize + 2
    }
}
