//! The chart's external levels (decision 0022): every registered level source that serves the
//! chart's product draws its map here. Lines and zones use the **source's colours** (the one
//! exception to "theme tokens only"); labels use theme text on the legend box.
//!
//! - Paint order (see `chart_view`): zone fills behind the candles ([`LevelsLayer::paint_back`]),
//!   lines and zone edges above them ([`LevelsLayer::paint_front`]), labels last
//!   ([`LevelsLayer::paint_labels`]).
//! - `prepare` (under the store lock) copies each source's current map handle and places its
//!   visible levels into fixed buffers (`lines_core::levels::place`): no allocation in steady
//!   state. Labels are shaped once per distinct text (a cache miss allocates).
//! - Freshness (docs/design.md §7): a **stale** map is drawn dimmed and says so in the legend;
//!   a **missing** one draws nothing and says "missing". Never shown as current.
//! - Labels never cover the last-price row: they move off it (`place_labels`); the last-price
//!   line and tag are drawn after every layer.
//! - Per pane ([`LevelsConfig`], saved with the workspace): all levels, per source, per kind,
//!   and labels. The chart's study visibility applies on top: "hide studies" hides levels,
//!   "hide study labels" and Clean chart hide their labels and legend rows.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    App, Bounds, Hsla, Pixels, ShapedLine, SharedString, TextAlign, TextRun, Window, fill, point,
    px, size,
};
use lines_core::geometry::snap;
use lines_core::levels::{LevelMap, LevelShape, MAX_LEVELS, Placed, place, place_labels};
use lines_core::{Product, UtcMillis, Viewport};
use lines_feed::ChartData;
use lines_feed::indicators::{FeedInfo, IndicatorHandle, engine};
use lines_feed::levels::runtime::{self, LevelHandle, LevelStatus};
use lines_feed::levels::{LevelContext, StorePrices};
use lines_feed::store::ChartStore;
use lines_indicators::render::{Column, build_columns};
use lines_indicators::{Bands, Reset, Spec, VwapSpec};
use serde::{Deserialize, Serialize};

use crate::theme::Theme;

const LABEL_PX: f32 = 11.0;
/// Most labels shaped and kept (cleared when exceeded).
const MAX_SHAPED: usize = 512;

/// A chart pane's level settings. Hidden things are listed (so a new source or kind shows by
/// default); unknown ids are kept, so a source missing from this build doesn't lose its
/// settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct LevelsConfig {
    /// All levels hidden on this chart.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// No level labels (lines and zones still drawn).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hide_labels: bool,
    /// Source ids hidden.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hidden_sources: Vec<String>,
    /// `source/kind` pairs hidden.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hidden_kinds: Vec<String>,
}

impl LevelsConfig {
    pub fn is_default(&self) -> bool {
        *self == LevelsConfig::default()
    }

    pub fn source_shown(&self, source: &str) -> bool {
        !self.hidden && !self.hidden_sources.iter().any(|s| s == source)
    }

    pub fn kind_shown(&self, source: &str, kind: &str) -> bool {
        !self.hidden_kinds.iter().any(|k| {
            k.split_once('/')
                .is_some_and(|(s, kd)| s == source && kd == kind)
        })
    }

    fn toggle(list: &mut Vec<String>, item: String) {
        if let Some(i) = list.iter().position(|s| *s == item) {
            list.remove(i);
        } else {
            list.push(item);
            list.sort();
        }
    }

    pub fn toggle_source(&mut self, source: &str) {
        Self::toggle(&mut self.hidden_sources, source.to_owned());
    }

    pub fn toggle_kind(&mut self, source: &str, kind: &str) {
        Self::toggle(&mut self.hidden_kinds, format!("{source}/{kind}"));
    }
}

/// One source on this chart.
struct Slot {
    handle: LevelHandle,
    label: &'static str,
    map: Option<Arc<LevelMap>>,
    status: LevelStatus,
    error: Option<String>,
    /// Kind visibility bit per kind index (maps have at most 64 kinds).
    kinds: u64,
    placed: Vec<Placed>,
}

/// One legend row: source label, freshness text, and whether it is a warning.
pub struct LegendRow {
    pub label: &'static str,
    pub text: String,
    pub warn: bool,
}

pub struct LevelsLayer {
    pub config: LevelsConfig,
    slots: Vec<Slot>,
    shaped: HashMap<String, ShapedLine>,
    /// The chart's feed, for the ETH VWAP node.
    feed: Option<FeedInfo>,
    /// The chart's ETH VWAP from the shared indicator engine, acquired only while some map
    /// has VWAP-relative levels.
    vwap: Option<IndicatorHandle>,
    /// This frame's VWAP, one column per visible bar (fixed capacity).
    vwap_cols: Vec<Column>,
    /// Plot px per catalog price unit this frame (offsets from the VWAP to px).
    px_per_unit: f32,
}

/// Columns kept for the VWAP that VWAP-relative levels follow.
const VWAP_COLUMNS: usize = 4096;

impl LevelsLayer {
    pub fn new(config: LevelsConfig) -> LevelsLayer {
        LevelsLayer {
            config,
            slots: Vec::new(),
            shaped: HashMap::new(),
            feed: None,
            vwap: None,
            vwap_cols: Vec::with_capacity(VWAP_COLUMNS),
            px_per_unit: 0.0,
        }
    }

    /// Join every registered source that serves the chart's product (on a feed change).
    pub fn attach(&mut self, store: &Arc<ChartStore>) {
        let (product, contract): (Product, _) = {
            let d = store.read();
            self.feed = Some(FeedInfo {
                feed: store.feed_id(),
                product: d.product.id,
                period_ms: d.series.period_ms(),
                max_bars: d.series.max_len(),
            });
            (d.product.clone(), d.contract)
        };
        self.vwap = None; // the new feed's node is acquired when a map needs it
        self.slots = lines_feed::levels::serving(&product)
            .into_iter()
            .map(|source| {
                let ctx = LevelContext {
                    product: product.clone(),
                    contract,
                    settings: runtime::settings_for(source.id()),
                };
                let label = source.label();
                let handle = runtime::acquire(source, ctx, Arc::new(StorePrices(store.clone())));
                Slot {
                    handle,
                    label,
                    map: None,
                    status: LevelStatus::Missing,
                    error: None,
                    kinds: u64::MAX,
                    placed: Vec::with_capacity(MAX_LEVELS),
                }
            })
            .collect();
    }

    /// Sources on this chart: `(id, label, kinds)` for menus (render time).
    pub fn sources(&self) -> Vec<(String, &'static str, Vec<String>)> {
        self.slots
            .iter()
            .map(|s| {
                (
                    s.handle.source().to_owned(),
                    s.label,
                    s.map.as_ref().map_or_else(Vec::new, |m| m.kinds.clone()),
                )
            })
            .collect()
    }

    /// Sum of the sources' revisions (repaint when it changes).
    pub fn revision(&self) -> u64 {
        self.slots
            .iter()
            .fold(0u64, |a, s| a.wrapping_add(s.handle.cell().revision()))
    }

    /// Under the store lock, after the viewport update. `last_y`: the last price's row.
    pub fn prepare(&mut self, data: &ChartData, vp: &Viewport, last_y: Option<f32>) {
        let plot_h = vp.height as f32;
        // 1. Each source's current map and freshness.
        for s in &mut self.slots {
            if let Some(st) = s.handle.cell().try_read() {
                // Freshness on the source's clock: the replay clock in a replay.
                let now = if st.replay {
                    data.market_time.unwrap_or(UtcMillis(i64::MIN))
                } else {
                    lines_feed::now_utc()
                };
                s.status = st.status(now);
                if !same(&s.map, &st.map) {
                    s.map = st.map.clone();
                }
                if s.error.as_deref() != st.error.as_deref() {
                    s.error = st.error.clone(); // only when it changes
                }
            }
        }
        // 2. VWAP-relative levels (decision 0022, e.g. VWAP ± k × a statistic) follow the
        // chart's own ETH VWAP: the shared engine node, acquired the first time a map needs it.
        let needs_vwap = self.slots.iter().any(|s| {
            s.map
                .as_ref()
                .is_some_and(|m| m.levels.iter().any(|l| l.is_vwap_relative()))
        });
        if needs_vwap
            && self.vwap.is_none()
            && let Some(info) = self.feed
        {
            self.vwap = Some(engine().acquire(
                info,
                Spec::Vwap {
                    period_ms: info.period_ms,
                    vwap: VwapSpec {
                        reset: Reset::Eth,
                        bands: Bands(0),
                    },
                },
            ));
        }
        let range = (vp.price_hi - vp.price_lo).abs().max(1e-9);
        self.px_per_unit = (vp.height / range) as f32;
        let mut vwap_now = None;
        if let Some(h) = &self.vwap
            && let Some(out) = h.cell().try_read()
        {
            vwap_now = out.points.last().map(|p| p.vwap);
            build_columns(
                &out.points,
                data.series.bars(),
                vp,
                Bands(0),
                &mut self.vwap_cols,
            );
        } else if self.vwap.is_none() {
            self.vwap_cols.clear();
        }
        // 3. Placement.
        for s in &mut self.slots {
            s.placed.clear();
            let source = s.handle.source();
            let Some(map) = s.map.as_ref() else { continue };
            if !self.config.source_shown(source) || s.status == LevelStatus::Missing {
                continue;
            }
            let mut mask = 0u64;
            for (i, k) in map.kinds.iter().enumerate().take(64) {
                if self.config.kind_shown(source, k) {
                    mask |= 1 << i;
                }
            }
            s.kinds = mask;
            place(map, vp, vwap_now, |k| mask & (1 << k) != 0, &mut s.placed);
            place_labels(&mut s.placed, map, LABEL_PX * 1.6, plot_h, last_y);
        }
    }

    fn each(&self, mut f: impl FnMut(&LevelMap, &Placed, f32)) {
        for s in &self.slots {
            let Some(map) = &s.map else { continue };
            // Stale maps are dimmed (shown as stale, never as current).
            let dim = if matches!(s.status, LevelStatus::Stale { .. }) {
                0.4
            } else {
                1.0
            };
            for p in &s.placed {
                f(map, p, dim);
            }
        }
    }

    /// Zone fills (behind the candles).
    pub fn paint_back(&self, plot: Bounds<Pixels>, window: &mut Window) {
        let scale = window.scale_factor();
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        let pw = f32::from(plot.size.width);
        let ppu = self.px_per_unit;
        let cols = &self.vwap_cols;
        self.each(|map, p, dim| {
            let l = &map.levels[p.index as usize];
            if let (true, Some((lo, hi))) = (p.vwap_relative, l.vwap_offsets()) {
                // Along the VWAP, bar by bar.
                if let LevelShape::Zone { .. } = l.shape {
                    for c in cols {
                        let (top, bottom) = (c.y - hi as f32 * ppu, c.y - lo as f32 * ppu);
                        window.paint_quad(fill(
                            Bounds::new(
                                point(px(ox + c.left), px(oy + top)),
                                size(px(c.right - c.left), px((bottom - top).max(1.0 / scale))),
                            ),
                            colour(l.style.rgba, 0.10 * dim),
                        ));
                    }
                }
                return;
            }
            if let LevelShape::Zone { .. } = l.shape {
                let (top, bottom) = (snap(p.top as f64, scale), snap(p.bottom as f64, scale));
                let h = (bottom - top).max(1.0 / scale);
                window.paint_quad(fill(
                    Bounds::new(point(px(ox), px(oy + top)), size(px(pw), px(h))),
                    colour(l.style.rgba, 0.10 * dim),
                ));
            }
        });
    }

    /// Lines and zone edges (above the candles).
    pub fn paint_front(&self, plot: Bounds<Pixels>, window: &mut Window) {
        let scale = window.scale_factor();
        let hair = 1.0 / scale;
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        let pw = f32::from(plot.size.width);
        let line = |window: &mut Window, y: f32, weight: u8, dashed: bool, c: Hsla| {
            let w = f32::from(weight.clamp(1, 4)) * hair;
            let y = snap(f64::from(y - w * 0.5), scale);
            if dashed {
                let (on, off): (f32, f32) = (6.0, 4.0);
                let mut x = 0.0;
                while x < pw {
                    let seg = on.min(pw - x);
                    window.paint_quad(fill(
                        Bounds::new(point(px(ox + x), px(oy + y)), size(px(seg), px(w))),
                        c,
                    ));
                    x += on + off;
                }
            } else {
                window.paint_quad(fill(
                    Bounds::new(point(px(ox), px(oy + y)), size(px(pw), px(w))),
                    c,
                ));
            }
        };
        let ppu = self.px_per_unit;
        let cols = &self.vwap_cols;
        // A VWAP-relative edge: a segment across each bar at VWAP + offset, joined to the
        // previous bar of the same session; dashed draws every other bar.
        let along = |window: &mut Window, offset: f64, weight: u8, dashed: bool, c: Hsla| {
            let w = f32::from(weight.clamp(1, 4)) * hair;
            let dy = offset as f32 * ppu;
            let mut prev: Option<f32> = None;
            for (i, col) in cols.iter().enumerate() {
                let y = col.y - dy;
                if !(dashed && i % 2 == 1) {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(px(ox + col.left), px(oy + y - w * 0.5)),
                            size(px(col.right - col.left), px(w)),
                        ),
                        c,
                    ));
                }
                if let (true, Some(py)) = (col.joins && !dashed, prev) {
                    let (a, b) = (py.min(y), py.max(y));
                    window.paint_quad(fill(
                        Bounds::new(
                            point(px(ox + col.left - w * 0.5), px(oy + a - w * 0.5)),
                            size(px(w), px(b - a + w)),
                        ),
                        c,
                    ));
                }
                prev = Some(y);
            }
        };
        self.each(|map, p, dim| {
            let l = &map.levels[p.index as usize];
            let c = colour(l.style.rgba, dim);
            if let (true, Some((lo, hi))) = (p.vwap_relative, l.vwap_offsets()) {
                match l.shape {
                    LevelShape::Line(_) => along(window, lo, l.style.weight, l.style.dashed, c),
                    LevelShape::Zone { .. } => {
                        let c = colour(l.style.rgba, 0.6 * dim);
                        along(window, hi, 1, l.style.dashed, c);
                        along(window, lo, 1, l.style.dashed, c);
                    }
                }
                return;
            }
            match l.shape {
                LevelShape::Line(_) => line(window, p.top, l.style.weight, l.style.dashed, c),
                LevelShape::Zone { .. } => {
                    let c = colour(l.style.rgba, 0.6 * dim);
                    line(window, p.top, 1, l.style.dashed, c);
                    line(window, p.bottom, 1, l.style.dashed, c);
                }
            }
        });
    }

    /// Labels: theme text on the legend box at the plot's right edge, with a swatch of the
    /// level's colour.
    pub fn paint_labels(
        &mut self,
        plot: Bounds<Pixels>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.config.hide_labels {
            return;
        }
        let scale = window.scale_factor();
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        let pw = f32::from(plot.size.width);
        let box_h = LABEL_PX * 1.6;
        for si in 0..self.slots.len() {
            let Some(map) = self.slots[si].map.clone() else {
                continue;
            };
            let dim = matches!(self.slots[si].status, LevelStatus::Stale { .. });
            for pi in 0..self.slots[si].placed.len() {
                let p = self.slots[si].placed[pi];
                let (Some(y), Some(text)) =
                    (p.label_y, map.levels[p.index as usize].label.as_deref())
                else {
                    continue;
                };
                let rgba = map.levels[p.index as usize].style.rgba;
                if !self.shaped.contains_key(text) {
                    if self.shaped.len() >= MAX_SHAPED {
                        self.shaped.clear();
                    }
                    let run = TextRun {
                        len: text.len(),
                        font: window.text_style().font(),
                        color: theme.text_main,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let line = window.text_system().shape_line(
                        SharedString::from(text.to_owned()),
                        px(LABEL_PX),
                        &[run],
                        None,
                    );
                    self.shaped.insert(text.to_owned(), line);
                }
                let line = &self.shaped[text];
                let w = f32::from(line.width) + 22.0;
                let x = snap(f64::from((pw - w - 8.0).max(0.0)), scale);
                let top = snap(f64::from(y - box_h * 0.5), scale);
                window.paint_quad(fill(
                    Bounds::new(point(px(ox + x), px(oy + top)), size(px(w), px(box_h))),
                    theme.label_bg,
                ));
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(ox + x + 6.0), px(oy + top + box_h * 0.5 - 1.5)),
                        size(px(8.0), px(3.0)),
                    ),
                    colour(rgba, if dim { 0.4 } else { 1.0 }),
                ));
                let _ = line.paint(
                    point(
                        px(ox + x + 18.0),
                        px(oy + top + box_h * 0.5 - LABEL_PX * 0.65),
                    ),
                    px(LABEL_PX * 1.3),
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
        }
    }

    /// Levels placed per source this frame (tests: placement and visibility).
    #[cfg(test)]
    pub fn placed_per_source(&self) -> Vec<usize> {
        self.slots.iter().map(|s| s.placed.len()).collect()
    }

    /// Placed levels with label positions, for source `i` (tests).
    #[cfg(test)]
    pub fn placed(&self, i: usize) -> &[Placed] {
        &self.slots[i].placed
    }

    /// Override a source's freshness (tests: stale and missing rendering).
    #[cfg(test)]
    pub fn force_status(&mut self, i: usize, status: LevelStatus) {
        self.slots[i].status = status;
    }

    /// Legend rows (render time): one per source on this chart, with its freshness.
    pub fn legend(&self) -> Vec<LegendRow> {
        self.slots
            .iter()
            .filter(|s| self.config.source_shown(s.handle.source()))
            .map(|s| {
                let (text, warn) = match (s.status, &s.map) {
                    (LevelStatus::Missing, _) => (
                        match &s.error {
                            Some(e) => format!("missing · {e}"),
                            None => "missing".into(),
                        },
                        true,
                    ),
                    (LevelStatus::Stale { age_ms }, _) => {
                        (format!("stale · {}", age(age_ms)), true)
                    }
                    (LevelStatus::Current, Some(m)) => {
                        let (_, _, _, h, mi, _) = m.as_of.to_civil();
                        (format!("as of {h:02}:{mi:02}Z"), false)
                    }
                    (LevelStatus::Current, None) => (String::new(), false),
                };
                LegendRow {
                    label: s.label,
                    text,
                    warn,
                }
            })
            .collect()
    }
}

fn age(ms: i64) -> String {
    match ms / 1000 {
        s if s < 120 => format!("{s} s"),
        s if s < 7200 => format!("{} min", s / 60),
        s => format!("{} h", s / 3600),
    }
}

use crate::theme::source_colour as colour;

/// The same map (by pointer), or both absent.
fn same(a: &Option<Arc<LevelMap>>, b: &Option<Arc<LevelMap>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
