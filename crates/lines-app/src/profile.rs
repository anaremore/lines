//! The chart's volume profile (roadmap 5): a [`ChartLayer`] drawing a horizontal histogram of
//! traded volume per price, anchored to the plot's left edge (over older bars, never the
//! live edge), with the point of control and the value area.
//!
//! Modes ([`ProfileMode`]): the current **session** (ETH by default, or RTH, from the
//! product's session calendar), the **visible range** of bars, or a **composite** of the last
//! N sessions. The data is kept per trade by the feed's store (`lines_core::profile`); this
//! layer only groups it into rows (exactly as the DOM groups prices) and paints them.
//!
//! - `prepare` (under the store lock) skips all work when nothing it depends on changed (the
//!   store's profile version, the mode, the grouping, the visible bars). Otherwise it groups
//!   the levels into a reused buffer (`ProfileRows::build`, allocation-free) and writes the
//!   label into a fixed string.
//! - `paint` emits quads for the rows on screen only; the chart calls it *before* the
//!   candles, so the profile sits behind them. `paint_label` (after the indicator lines)
//!   draws the label, shaped once per distinct text (a cache miss allocates, as for every
//!   chart label).
//!
//! Never obscuring the last price: the profile grows from the left edge, away from the newest
//! bars and the price axis, is translucent, and the chart draws the last-price line after all
//! layers (and its tag on the axis). The POC and value-area edges are hairlines.
//!
//! Missing data shows as missing (docs/design.md §7): a profile whose session (or range)
//! began before the first trade the store counted is labelled `partial from HH:MMZ`.
//!
//! Keys (chart focused): Alt+V cycles off → session → visible range → composite;
//! Alt+R switches ETH/RTH; Alt+B toggles the buy/sell split; Alt+G cycles the grouping
//! (auto, then the DOM presets).

use std::fmt::Write as _;

use gpui::{
    App, Bounds, Hsla, Pixels, ShapedLine, SharedString, TextAlign, TextRun, Window, fill, point,
    px, size,
};
use lines_core::geometry::snap;
use lines_core::ladder::{GROUPING_PRESETS, row_of};
use lines_core::{BarsView, FixedStr, Price, ProfileRows, SessionKind, UtcMillis, Viewport};
use lines_feed::ChartData;
use serde::{Deserialize, Serialize};

use crate::chart_view::ChartLayer;
use crate::theme::Theme;

/// Rows the grouping buffer holds; wider profiles get a coarser grouping
/// (`ProfileRows::fit_grouping`).
const MAX_ROWS: usize = 4096;
/// Auto grouping: the smallest DOM preset whose rows are at least this tall (logical px).
const AUTO_MIN_ROW_PX: f64 = 3.0;
const LABEL_PX: f32 = 11.0;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileMode {
    #[default]
    Off,
    /// The current session (or, between sessions, the last one).
    Session,
    /// The bars on screen.
    VisibleRange,
    /// The last `composite_sessions` sessions, current included.
    Composite,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileSession {
    /// Full electronic session (the default, as for VWAP: decision 0017).
    #[default]
    Eth,
    /// Regular trading hours.
    Rth,
}

impl ProfileSession {
    pub fn kind(self) -> SessionKind {
        match self {
            ProfileSession::Eth => SessionKind::Eth,
            ProfileSession::Rth => SessionKind::Rth,
        }
    }
}

/// A chart's volume profile settings: plain data, serialisable for workspace persistence.
/// Unknown or missing fields take their defaults; out-of-range values are clamped on use.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ProfileConfig {
    pub mode: ProfileMode,
    /// Session type for the session and composite modes.
    pub session: ProfileSession,
    /// Sessions in a composite (2..=30).
    pub composite_sessions: u8,
    /// Value area share of the volume, percent (1..=100).
    pub value_area_percent: u8,
    /// Split each row by aggressor side.
    pub split: bool,
    /// Ticks per row; `None` = auto (the smallest DOM preset with readable rows).
    pub grouping: Option<u32>,
    /// Profile width, percent of the plot width (5..=60).
    pub width_percent: u8,
    /// Hidden on this chart (the studies menu's eye): kept, not computed or drawn.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

impl Default for ProfileConfig {
    fn default() -> Self {
        ProfileConfig {
            mode: ProfileMode::Off,
            session: ProfileSession::Eth,
            composite_sessions: 5,
            value_area_percent: 70,
            split: false,
            grouping: None,
            width_percent: 25,
            hidden: false,
        }
    }
}

impl ProfileConfig {
    pub fn is_default(&self) -> bool {
        *self == ProfileConfig::default()
    }

    fn composite(&self) -> usize {
        self.composite_sessions
            .clamp(2, lines_core::SessionProfiles::MAX_SESSIONS as u8) as usize
    }

    fn value_area(&self) -> u8 {
        self.value_area_percent.clamp(1, 100)
    }

    fn width(&self) -> f32 {
        f32::from(self.width_percent.clamp(5, 60)) / 100.0
    }

    /// Handle an Alt+key on the chart. `true` if it changed the config.
    pub fn on_alt_key(&mut self, key: &str) -> bool {
        match key {
            "v" => {
                self.mode = match self.mode {
                    ProfileMode::Off => ProfileMode::Session,
                    ProfileMode::Session => ProfileMode::VisibleRange,
                    ProfileMode::VisibleRange => ProfileMode::Composite,
                    ProfileMode::Composite => ProfileMode::Off,
                }
            }
            "r" => {
                self.session = match self.session {
                    ProfileSession::Eth => ProfileSession::Rth,
                    ProfileSession::Rth => ProfileSession::Eth,
                }
            }
            "b" => self.split = !self.split,
            "g" => {
                self.grouping = match self.grouping {
                    None => Some(GROUPING_PRESETS[0]),
                    Some(g) => GROUPING_PRESETS.iter().copied().find(|&p| p > g),
                }
            }
            _ => return false,
        }
        true
    }
}

/// What the last build was computed from; equal keys mean the rows are still valid.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct BuildKey {
    version: u64,
    config: ProfileConfig,
    grouping: u32,
    /// Session open (session/composite) or first visible bar time (visible range).
    from: i64,
    to: i64,
}

pub struct VolumeProfileLayer {
    config: ProfileConfig,
    rows: ProfileRows,
    key: Option<BuildKey>,
    /// Rows are valid and should be drawn.
    active: bool,
    label: FixedStr<96>,
    shaped: Option<(FixedStr<96>, ShapedLine)>,
}

impl VolumeProfileLayer {
    pub fn new(config: ProfileConfig) -> Self {
        VolumeProfileLayer {
            config,
            rows: ProfileRows::with_capacity(MAX_ROWS),
            key: None,
            active: false,
            label: FixedStr::new(),
            shaped: None,
        }
    }

    pub fn config(&self) -> ProfileConfig {
        self.config
    }

    /// Rows were built at least once (tests: hidden studies do no work).
    #[cfg(test)]
    pub fn has_built(&self) -> bool {
        self.key.is_some()
    }

    pub fn set_config(&mut self, config: ProfileConfig) {
        self.config = config;
        self.key = None;
    }

    /// The profile's label (mode, session, coverage), drawn after the indicator lines so it
    /// stays readable: the rows themselves are painted behind the candles.
    pub fn paint_label(
        &mut self,
        plot: Bounds<Pixels>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !self.active {
            return;
        }
        let t = theme;
        let scale = window.scale_factor();
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        let ph = f32::from(plot.size.height);
        let paint = |window: &mut Window, x: f32, y: f32, w: f32, h: f32, c: Hsla| {
            window.paint_quad(fill(
                Bounds::new(point(px(ox + x), px(oy + y)), size(px(w), px(h))),
                c,
            ));
        };
        // Label, at the bottom of the profile (the hover readout owns the top-left corner).
        if self
            .shaped
            .as_ref()
            .is_none_or(|(s, _)| s.as_str() != self.label.as_str())
        {
            let text = self.label.as_str();
            let run = TextRun {
                len: text.len(),
                font: window.text_style().font(),
                color: t.text_dim,
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
            self.shaped = Some((self.label, line));
        }
        if let Some((_, line)) = &self.shaped {
            let w = f32::from(line.width) + 12.0;
            let box_h = LABEL_PX * 1.6;
            let x = snap(6.0, scale);
            let y = snap(f64::from((ph - box_h - 6.0).max(0.0)), scale);
            paint(window, x, y, w, box_h, t.label_bg);
            let _ = line.paint(
                point(px(ox + x + 6.0), px(oy + y + box_h * 0.5 - LABEL_PX * 0.65)),
                px(LABEL_PX * 1.3),
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }

    /// Group `levels` (called up to three times) into rows; grouping `wanted` ticks, coarser
    /// if the profile wouldn't fit the buffer.
    fn build<I>(&mut self, tick: i64, wanted: u32, levels: impl Fn() -> I)
    where
        I: Iterator<Item = (Price, lines_core::SideVolume)>,
    {
        let mut extent: Option<(Price, Price)> = None;
        for (p, _) in levels() {
            extent = Some(extent.map_or((p, p), |(a, b)| (a.min(p), b.max(p))));
        }
        let Some((lo, hi)) = extent else {
            self.rows.build(tick, 70, std::iter::empty);
            return;
        };
        let g = ProfileRows::fit_grouping(wanted, tick, lo, hi, MAX_ROWS);
        self.rows
            .build(i64::from(g) * tick, self.config.value_area(), levels);
    }
}

/// The smallest DOM preset whose rows are at least [`AUTO_MIN_ROW_PX`] tall.
fn auto_grouping(vp: &Viewport, tick: i64) -> u32 {
    let px_per_unit = vp.height / (vp.price_hi - vp.price_lo).abs().max(1e-9);
    GROUPING_PRESETS
        .iter()
        .copied()
        .find(|&g| g as f64 * tick as f64 * px_per_unit >= AUTO_MIN_ROW_PX)
        .unwrap_or(GROUPING_PRESETS[GROUPING_PRESETS.len() - 1])
}

fn write_partial(s: &mut FixedStr<96>, from: UtcMillis) {
    let (_, _, _, h, m, _) = from.to_civil();
    let _ = write!(s, " · partial from {h:02}:{m:02}Z");
}

/// ` · gap 15:02–15:40Z` (and ` +N` more): counted trades are missing inside the range.
fn write_gap(s: &mut FixedStr<96>, (a, b): (i64, i64), more: u32) {
    let (_, _, _, h1, m1, _) = UtcMillis(a).to_civil();
    let (_, _, _, h2, m2, _) = UtcMillis(b).to_civil();
    let _ = write!(s, " · gap {h1:02}:{m1:02}–{h2:02}:{m2:02}Z");
    if more > 0 {
        let _ = write!(s, " +{more}");
    }
}

impl ChartLayer for VolumeProfileLayer {
    fn prepare(&mut self, data: &ChartData, bars: BarsView<'_>, vp: &Viewport) {
        let cfg = self.config;
        self.active = false;
        if cfg.mode == ProfileMode::Off || cfg.hidden {
            return;
        }
        let Some(now) = data.market_time else {
            return;
        };
        let tick = data.product.price.tick.max(1);
        let grouping = cfg
            .grouping
            .unwrap_or_else(|| auto_grouping(vp, tick))
            .max(1);
        let profiles = &data.profiles;
        let kind = cfg.session.kind();
        let name = match cfg.session {
            ProfileSession::Eth => "ETH",
            ProfileSession::Rth => "RTH",
        };
        let mut label = FixedStr::<96>::new();
        // The range the profile covers, for the key and the partial check.
        let (from, to) = match cfg.mode {
            ProfileMode::Off => return,
            ProfileMode::Session => {
                let Some(s) = profiles.current(kind, now) else {
                    return;
                };
                let _ = write!(label, "{name} {}", s.session.trade_date);
                (s.session.open, s.session.close)
            }
            ProfileMode::Composite => {
                let list = profiles.sessions(kind);
                let Some(newest) = list.back() else {
                    return;
                };
                let first = &list[list.len().saturating_sub(cfg.composite())];
                let n = list.len().min(cfg.composite());
                let plural = if n == 1 { "" } else { "s" };
                let _ = write!(label, "{name} {n} session{plural}");
                if n < cfg.composite() {
                    let _ = write!(label, " (of {})", cfg.composite());
                }
                (first.session.open, newest.session.close)
            }
            ProfileMode::VisibleRange => {
                // The footprint is per 1-minute bar: a higher timeframe's bars merge theirs.
                let r = vp.visible_range(bars.len());
                if r.is_empty() {
                    return;
                }
                let _ = label.write_str("visible range");
                (bars.bars[r.start].time, bars.end(r.end - 1))
            }
        };
        // Where the counted trades (stored and live) don't cover the range: it starts later
        // (`partial from`), or has holes (`gap`).
        let gaps = data.counted_gaps(from.0, to.0 - 1);
        if gaps.any {
            if let Some(s) = gaps.data_from {
                write_partial(&mut label, UtcMillis(s));
            }
            if let Some(g) = gaps.first_gap {
                write_gap(&mut label, g, gaps.more_gaps);
            }
        }
        self.label = label;
        let key = BuildKey {
            version: profiles.version(),
            config: cfg,
            grouping,
            from: from.0,
            to: to.0,
        };
        if self.key != Some(key) {
            match cfg.mode {
                ProfileMode::Session => {
                    let s = profiles.current(kind, now).map(|s| &s.volume);
                    self.build(tick, grouping, || s.into_iter().flat_map(|v| v.iter()));
                }
                ProfileMode::Composite => {
                    let list = profiles.sessions(kind);
                    let skip = list.len().saturating_sub(cfg.composite());
                    self.build(tick, grouping, || {
                        list.iter().skip(skip).flat_map(|s| s.volume.iter())
                    });
                }
                ProfileMode::VisibleRange => {
                    let fp = profiles.footprint();
                    self.build(tick, grouping, || fp.range(from, to));
                }
                ProfileMode::Off => {}
            }
            self.key = Some(key);
        }
        self.active = !self.rows.rows.is_empty() && self.rows.max_row.0 > 0;
    }

    fn paint(
        &mut self,
        plot: Bounds<Pixels>,
        vp: &Viewport,
        theme: &Theme,
        window: &mut Window,
        _cx: &mut App,
    ) {
        if !self.active {
            return;
        }
        let t = theme;
        let r = &self.rows;
        let scale = window.scale_factor();
        let hair = 1.0 / scale;
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        let (pw, ph) = (f32::from(plot.size.width), f32::from(plot.size.height));
        let width = snap(f64::from(pw * self.config.width()), scale);
        let paint = |window: &mut Window, x: f32, y: f32, w: f32, h: f32, c: Hsla| {
            if w > 0.0 && h > 0.0 {
                window.paint_quad(fill(
                    Bounds::new(point(px(ox + x), px(oy + y)), size(px(w), px(h))),
                    c,
                ));
            }
        };
        // Rows on screen only.
        let (plo, phi) = (vp.price_lo.min(vp.price_hi), vp.price_lo.max(vp.price_hi));
        let u = r.row_units;
        let last = r.rows.len() as i64 - 1;
        let i_lo = (row_of(Price(plo.floor() as i64), u) - r.base_row).clamp(0, last.max(0));
        let i_hi = (row_of(Price(phi.ceil() as i64), u) - r.base_row).clamp(0, last.max(0));
        let max = r.max_row.0.max(1) as f64;
        let (va_lo, va_hi) = r.value_area.unwrap_or((usize::MAX, 0));
        let row_y = |i: usize| {
            let lo = r.row_price(i).0 as f64;
            (
                snap(vp.price_to_y(lo + u as f64), scale),
                snap(vp.price_to_y(lo), scale),
            )
        };
        for i in i_lo as usize..=i_hi as usize {
            let row = &r.rows[i];
            if row.total.0 <= 0 {
                continue;
            }
            let (top, bottom) = row_y(i);
            let mut h = bottom - top;
            if h >= 3.0 * hair {
                h -= hair; // a device-pixel gap between rows
            }
            let h = h.max(hair);
            let len = snap(row.total.0 as f64 / max * f64::from(width), scale).max(hair);
            let in_va = i >= va_lo && i <= va_hi;
            let dim = if in_va { 1.0 } else { 0.55 };
            let base = if in_va { t.profile_va } else { t.volume_bar };
            if self.config.split {
                // Sells from the base edge, then buys, then unknown: position tells them
                // apart, not only colour.
                let v = row.volume;
                let seg = |q: i64| snap(q as f64 / max * f64::from(width), scale);
                let (s, b) = (seg(v.sell.0), seg(v.buy.0));
                let k = (len - s - b).max(0.0);
                paint(window, 0.0, top, s, h, t.profile_sell.opacity(dim));
                paint(window, s, top, b, h, t.profile_buy.opacity(dim));
                paint(window, s + b, top, k, h, base);
            } else {
                paint(window, 0.0, top, len, h, base);
            }
            if Some(i) == r.poc {
                // Point of control: a hairline through the row, the bar's length.
                let y = snap(f64::from(top + h * 0.5), scale);
                paint(window, 0.0, y, len, hair, t.profile_poc);
            }
        }
        // Value-area edges: hairlines across the profile's width.
        if let Some((a, b)) = r.value_area {
            let (top, _) = row_y(b);
            let (_, bottom) = row_y(a);
            for y in [top, bottom - hair] {
                if (0.0..ph).contains(&y) {
                    paint(window, 0.0, y, width, hair, t.profile_edge);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_tolerates_missing_fields() {
        let c = ProfileConfig {
            mode: ProfileMode::Composite,
            session: ProfileSession::Rth,
            composite_sessions: 10,
            value_area_percent: 68,
            split: true,
            grouping: Some(5),
            width_percent: 30,
            hidden: false,
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(
            json,
            r#"{"mode":"composite","session":"rth","composite-sessions":10,"value-area-percent":68,"split":true,"grouping":5,"width-percent":30}"#
        );
        assert_eq!(serde_json::from_str::<ProfileConfig>(&json).unwrap(), c);
        let partial: ProfileConfig = serde_json::from_str(r#"{"mode":"session"}"#).unwrap();
        assert_eq!(
            partial,
            ProfileConfig {
                mode: ProfileMode::Session,
                ..ProfileConfig::default()
            }
        );
        // Out-of-range values are clamped on use, never trusted.
        let wild = ProfileConfig {
            composite_sessions: 200,
            value_area_percent: 0,
            width_percent: 99,
            ..c
        };
        assert_eq!((wild.composite(), wild.value_area()), (30, 1));
        assert_eq!(wild.width(), 0.6);
    }

    #[test]
    fn alt_keys_cycle_the_config() {
        let mut c = ProfileConfig::default();
        let modes: Vec<_> = (0..4)
            .map(|_| {
                assert!(c.on_alt_key("v"));
                c.mode
            })
            .collect();
        assert_eq!(
            modes,
            [
                ProfileMode::Session,
                ProfileMode::VisibleRange,
                ProfileMode::Composite,
                ProfileMode::Off
            ]
        );
        c.on_alt_key("r");
        assert_eq!(c.session, ProfileSession::Rth);
        c.on_alt_key("b");
        assert!(c.split);
        let groupings: Vec<_> = (0..6)
            .map(|_| {
                c.on_alt_key("g");
                c.grouping
            })
            .collect();
        assert_eq!(
            groupings,
            [Some(1), Some(2), Some(5), Some(10), Some(25), None]
        );
        assert!(!c.on_alt_key("x"));
    }

    /// `prepare` on a real store: session selection, ETH vs RTH, partial coverage, composite
    /// and visible-range sources, and no rebuild when nothing changed.
    #[test]
    fn prepare_picks_the_source_and_labels_coverage() {
        use lines_core::{Aggressor, Capabilities, Month, Qty};
        use lines_feed::catalog;
        use lines_feed::provider::MarketEvent;
        use lines_feed::store::ChartStore;

        let es = catalog::get(catalog::ES).unwrap();
        let z26 = lines_core::ContractKey::monthly(es.id, 2026, Month::new(12).unwrap());
        let store = ChartStore::new(es, Some(z26), Capabilities::default(), "test", 60_000, 1000);
        let mut d = store.read();
        let trade = |at: &str, p: i64, q: i64| MarketEvent::Trade {
            time: UtcMillis::parse_iso(at).unwrap(),
            price: Price(p),
            qty: Qty(q),
            aggressor: Some(Aggressor::Buy),
        };
        // Trade date 11-05 (ETH opened 11-04 23:00 UTC): data starts at 13:00 UTC, so the ETH
        // profile is partial; RTH (14:30-21:00 UTC) is complete.
        d.apply(&[
            trade("2026-11-05T13:00:00", 600_000, 5), // 07:00 CT, ETH only
            trade("2026-11-05T15:00:00", 600_025, 2),
            trade("2026-11-05T15:01:00", 600_050, 3),
        ]);
        let mut vp = Viewport::default();
        vp.set_size(800.0, 600.0);
        (vp.price_lo, vp.price_hi) = (599_000.0, 601_000.0);
        vp.right_index = 61.0;
        let mut layer = VolumeProfileLayer::new(ProfileConfig {
            mode: ProfileMode::Session,
            ..ProfileConfig::default()
        });
        layer.prepare(
            &d,
            BarsView::fixed(d.series.bars(), d.series.period_ms()),
            &vp,
        );
        assert!(layer.active);
        assert_eq!(layer.label.as_str(), "ETH 2026-11-05 · partial from 13:00Z");
        assert_eq!(layer.rows.total, Qty(10));
        assert_eq!(
            layer.rows.row_price(layer.rows.poc.unwrap()),
            Price(600_000)
        );
        // Nothing changed: the rows are not rebuilt (the key is the same).
        let key = layer.key;
        layer.prepare(
            &d,
            BarsView::fixed(d.series.bars(), d.series.period_ms()),
            &vp,
        );
        assert_eq!(layer.key, key);

        layer.set_config(ProfileConfig {
            session: ProfileSession::Rth,
            ..layer.config()
        });
        layer.prepare(
            &d,
            BarsView::fixed(d.series.bars(), d.series.period_ms()),
            &vp,
        );
        assert_eq!(layer.label.as_str(), "RTH 2026-11-05");
        assert_eq!(layer.rows.total, Qty(5));
        assert_eq!(
            layer.rows.row_price(layer.rows.poc.unwrap()),
            Price(600_050)
        );

        // A new trade changes the version: rebuilt, incrementally fed by the store.
        d.apply(&[trade("2026-11-05T15:02:00", 600_025, 4)]);
        layer.prepare(
            &d,
            BarsView::fixed(d.series.bars(), d.series.period_ms()),
            &vp,
        );
        assert_eq!(layer.rows.total, Qty(9));
        assert_eq!(
            layer.rows.row_price(layer.rows.poc.unwrap()),
            Price(600_025)
        );

        layer.set_config(ProfileConfig {
            mode: ProfileMode::Composite,
            session: ProfileSession::Eth,
            ..layer.config()
        });
        layer.prepare(
            &d,
            BarsView::fixed(d.series.bars(), d.series.period_ms()),
            &vp,
        );
        assert_eq!(
            layer.label.as_str(),
            "ETH 1 session (of 5) · partial from 13:00Z"
        );

        layer.set_config(ProfileConfig {
            mode: ProfileMode::VisibleRange,
            ..layer.config()
        });
        layer.prepare(
            &d,
            BarsView::fixed(d.series.bars(), d.series.period_ms()),
            &vp,
        );
        assert_eq!(layer.label.as_str(), "visible range");
        assert_eq!(layer.rows.total, Qty(14));

        layer.set_config(ProfileConfig::default());
        layer.prepare(
            &d,
            BarsView::fixed(d.series.bars(), d.series.period_ms()),
            &vp,
        );
        assert!(!layer.active);
    }

    #[test]
    fn auto_grouping_keeps_rows_readable() {
        let mut vp = Viewport::default();
        vp.set_size(800.0, 600.0);
        (vp.price_lo, vp.price_hi) = (600_000.0, 602_500.0); // 100 ticks of 25 over 600 px
        assert_eq!(auto_grouping(&vp, 25), 1); // 6 px per tick
        (vp.price_lo, vp.price_hi) = (600_000.0, 625_000.0); // 1000 ticks: 0.6 px
        assert_eq!(auto_grouping(&vp, 25), 5); // 3 px
        (vp.price_lo, vp.price_hi) = (0.0, 1e9);
        assert_eq!(auto_grouping(&vp, 25), 25);
    }
}
