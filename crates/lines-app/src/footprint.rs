//! Footprint chart mode (roadmap 10e): per bar, volume by price row split by aggressor,
//! drawn in place of the candle wherever the bar has trade-level data. A study in the Studies
//! menu, off by default, saved with the chart (`ChartState::footprint`).
//!
//! The cells are `lines_core::footprint` (pure, exact, the merge of each bar's 1-minute
//! footprints on any timeframe), built in `prepare` under the store lock into fixed buffers.
//! `paint` draws quads and numbers from the cached-glyph painter (`crate::text::Glyphs`) with
//! `FixedStr` formatting: nothing allocates per frame once the glyphs are shaped.
//!
//! - **Modes:** bid × ask volume (sell left, buy right), delta per row, total volume.
//! - **Imbalances** are marked by a bar on the cell's side (buy: right edge, sell: left
//!   edge) as well as the number's colour, so they read without colour.
//! - **POC** per bar: the cell is outlined.
//! - **Rows** use the DOM grid and grouping, coarsened until a row is at least 9 px tall.
//! - **Fallbacks:** bars without trade data (history) keep their candle, and the legend says
//!   the footprint comes from live/replay trades only; when bars are too narrow to read cells,
//!   every bar is a candle and the legend asks to zoom in.

use gpui::{App, Bounds, Pixels, Window, fill, point, px, size};
use lines_core::footprint::{BarCells, FootprintFrame, ImbalanceRule, fit_grouping};
use lines_core::{BarsView, FixedStr, Qty, Viewport};
use lines_feed::ChartData;
use serde::{Deserialize, Serialize};

use crate::chart_view::ChartLayer;
use crate::text::Glyphs;
use crate::theme::Theme;

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
            FootprintMode::BidAsk => "bid × ask",
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

/// Rows shorter than this are coarsened (logical px).
pub const MIN_ROW_PX: f64 = 9.0;
/// Most cells drawn in one frame (bounded work; a bar beyond it shows as a candle).
const MAX_CELLS: usize = 40_000;
/// Most rows in one bar.
const MAX_ROWS: usize = 2_048;
/// The type scale's smallest size (docs/design.md §5).
const TEXT_PX: f32 = 11.0;

pub struct FootprintLayer {
    config: FootprintConfig,
    frame: FootprintFrame,
    /// Cells are drawn this frame (shown, and bars wide enough).
    active: bool,
    tick: i64,
    /// The product's quantity format (cells print through it).
    qty: lines_core::QtyFormat,
    glyphs: Glyphs,
    /// The visible range's first bar (candles are indexed from it).
    first_bar: usize,
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
            first_bar: 0,
        }
    }

    pub fn config(&self) -> FootprintConfig {
        self.config
    }

    pub fn set_config(&mut self, c: FootprintConfig) {
        self.config = c;
    }

    /// The legend line, if the footprint is on: its mode, and why bars are candles. Decided
    /// at render time from the same inputs paint uses (the bar width, and whether the oldest
    /// visible bar is older than the first counted trade), so a single frame never disagrees
    /// with what it draws.
    pub fn legend(
        &self,
        bar_px: f64,
        first_visible: Option<lines_core::UtcMillis>,
        data_start: Option<lines_core::UtcMillis>,
    ) -> Option<(String, Option<&'static str>)> {
        if !self.config.shown() {
            return None;
        }
        let note = if bar_px < self.config.mode.min_bar_px() {
            Some("zoom in to see cells")
        } else if first_visible.is_some_and(|t| data_start.is_none_or(|d| t < d)) {
            Some("footprint from live/replay trades only")
        } else {
            None
        };
        Some((format!("Footprint · {}", self.config.mode.label()), note))
    }

    /// Whether bar `i` (an index into the chart's bars) is drawn as cells this frame (its
    /// candle is not drawn).
    pub fn covers(&self, i: usize) -> bool {
        self.active && self.frame.bars.binary_search_by_key(&i, |b| b.bar).is_ok()
    }

    /// Cells were drawn this frame (tests).
    #[cfg(test)]
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
        }
        let need = (width as f64 / 30.0) as usize + 4;
        if self.frame.bars.capacity() < need {
            self.frame.bars.reserve_exact(need - self.frame.bars.len());
        }
    }
}

impl ChartLayer for FootprintLayer {
    fn prepare(&mut self, data: &ChartData, bars: BarsView<'_>, vp: &Viewport) {
        self.active = false;
        let range = vp.visible_range(bars.len());
        self.first_bar = range.start;
        if !self.config.shown() {
            return;
        }
        if vp.bar_px < self.config.mode.min_bar_px() {
            return; // too narrow to read: candles (the legend asks to zoom in)
        }
        self.tick = data.product.price.tick.max(1);
        self.qty = data.product.qty;
        let px_per_unit = vp.height / (vp.price_hi - vp.price_lo).abs().max(1e-9);
        let g = fit_grouping(
            self.config.grouping.unwrap_or(1),
            self.tick,
            px_per_unit,
            MIN_ROW_PX,
        );
        let units = (g as i64).saturating_mul(self.tick);
        self.frame.build(
            data.profiles.footprint(),
            bars,
            range,
            units,
            self.config.rule(self.qty.decimals),
        );
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
        if !self.active {
            return;
        }
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        let scale = window.scale_factor();
        let hair = 1.0 / scale;
        // Device-pixel snapping as the candles: cell edges on device-pixel boundaries (the
        // POC outline is one device pixel), cells centred on the bar's wick position.
        let snap = |v: f64| lines_core::geometry::snap(v, scale);
        let rect = |x: f32, y: f32, w: f32, h: f32| {
            Bounds::new(point(px(ox + x), px(oy + y)), size(px(w), px(h)))
        };
        let u = self.frame.row_units;
        // A whole number of device pixels per row, so every cell edge lands on one.
        let row_h = {
            let r = u as f64 * vp.height / (vp.price_hi - vp.price_lo).abs().max(1e-9);
            ((r * f64::from(scale)).round().max(1.0) / f64::from(scale)) as f32
        };
        let max = self.frame.max_cell.0.max(1) as f32;
        let mode = self.config.mode;
        let text = row_h >= TEXT_PX + 1.0;
        let mut s = FixedStr::<32>::new();
        for bi in 0..self.frame.bars.len() {
            let b: BarCells = self.frame.bars[bi];
            let cx_bar = lines_core::geometry::slot_x(vp, b.bar as f64, scale);
            let half = snap((vp.bar_px * 0.46).max(2.0));
            let (left, w) = (cx_bar - half, half * 2.0);
            for k in 0..b.len {
                let c = self.frame.cells[b.first + k];
                // Row `r` holds prices [r·u, (r+1)·u); centred on its lowest price's tick.
                let mid = (c.row * u) as f64 + (u - self.tick) as f64 * 0.5;
                let top = snap(vp.price_to_y(mid) - f64::from(row_h) * 0.5);
                if top + row_h < 0.0 || f64::from(top) > vp.height {
                    continue; // off the plot: nothing to draw
                }
                let shade = (c.total.0 as f32 / max).clamp(0.08, 1.0);
                let bg = match mode {
                    FootprintMode::Delta if c.delta() > 0 => t.up.opacity(0.12 + 0.4 * shade),
                    FootprintMode::Delta if c.delta() < 0 => t.down.opacity(0.12 + 0.4 * shade),
                    _ => t.text_dim.opacity(0.06 + 0.22 * shade),
                };
                window.paint_quad(fill(rect(left, top, w, (row_h - hair).max(hair)), bg));
                // Imbalance markers: a bar on the imbalanced side (shape, not only colour).
                let mark = (w * 0.08).clamp(2.0, 4.0);
                if c.imbalance.sell {
                    window.paint_quad(fill(rect(left, top, mark, row_h - hair), t.down));
                }
                if c.imbalance.buy {
                    window.paint_quad(fill(rect(left + w - mark, top, mark, row_h - hair), t.up));
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
                        window.paint_quad(fill(q, t.text_main));
                    }
                }
                if !text {
                    continue;
                }
                let y = oy + top;
                let qty = self.qty;
                let num = |s: &mut FixedStr<32>, v: i64| {
                    s.clear();
                    let _ = qty.write(Qty(v), s);
                };
                match mode {
                    FootprintMode::BidAsk => {
                        let gap = 3.0;
                        let mid_x = ox + left + w * 0.5;
                        let sell = if c.imbalance.sell {
                            t.down
                        } else {
                            t.text_main
                        };
                        let buy = if c.imbalance.buy { t.up } else { t.text_main };
                        num(&mut s, c.sell.0);
                        self.glyphs.paint_right(
                            s.as_str(),
                            mid_x - gap,
                            y,
                            row_h,
                            sell,
                            c.imbalance.sell,
                            window,
                            cx,
                        );
                        num(&mut s, c.buy.0);
                        self.glyphs.paint(
                            s.as_str(),
                            mid_x + gap,
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
        assert!(!fp.covers(0) && fp.covers(1) && fp.covers(3));
        let f = fp.frame();
        assert_eq!(f.bars.len(), 3);
        let b = f.bars[0];
        assert_eq!(
            f.cells_of(&b)
                .iter()
                .map(|c| (c.buy.0, c.sell.0))
                .collect::<Vec<_>>(),
            [(2, 0), (0, 3), (4, 0)]
        );
        let first = Some(bars.bars[vp.visible_range(4).start].time);
        let (label, note) = fp
            .legend(vp.bar_px, first, d.profiles.data_start())
            .unwrap();
        assert_eq!(label, "Footprint · bid × ask");
        assert_eq!(note, Some("footprint from live/replay trades only"));
        // Narrow bars: no cells at all; the legend asks to zoom in.
        let narrow = view(20.0);
        fp.prepare(&d, bars, &narrow);
        assert!(!fp.is_active() && !fp.covers(1));
        assert_eq!(
            fp.legend(narrow.bar_px, first, d.profiles.data_start())
                .unwrap()
                .1,
            Some("zoom in to see cells")
        );
        // Off: nothing, and no legend line.
        fp.set_config(FootprintConfig::default());
        fp.prepare(&d, bars, &vp);
        assert!(!fp.is_active());
        assert!(fp.legend(80.0, first, None).is_none());
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
}
