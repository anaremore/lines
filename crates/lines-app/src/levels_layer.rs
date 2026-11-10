//! The chart's external levels (decision 0022): every registered level source that serves the
//! chart's product draws its map here. Lines and zones use the **source's colours** (the one
//! exception to "theme tokens only") unless styled (decision 0026): a kind the source declares
//! as a level type takes its type's look (`study_style::resolve_type`: the chart's override for
//! the type, else the chart's override for the source, else the type's recommended look); any
//! other kind the source-wide override (`study_style::LEVELS`: one color and opacity for its
//! lines, one for its zones; zone edges take the Zones color at the Lines opacity). Labels sit
//! on the legend box, with the price when the label mode says so, in the level's own color
//! where that is AA there (`Theme::level_label_text`), else theme text; a type may ask for
//! theme text (`LabelColor::Text`) and put its labels at a zone's edge (`LabelAt`).
//!
//! - Paint order (see `chart_view`): zone fills behind the candles ([`LevelsLayer::paint_back`]),
//!   lines and zone edges above them ([`LevelsLayer::paint_front`]), labels last
//!   ([`LevelsLayer::paint_labels`]).
//! - `prepare` (under the store lock) copies each source's current map handle and places its
//!   visible levels into fixed buffers (`lines_core::levels::place`): no allocation in steady
//!   state. Labels are shaped once per distinct text (a cache miss allocates).
//! - Freshness (docs/design.md §7): a **stale** map is drawn dimmed and says so in the legend;
//!   a **missing** one draws nothing and says "missing". Never shown as current.
//! - Labels stay on their own levels (`place_labels`): the price never moves them, the
//!   last-price line and tag are drawn over them (after every layer), and only labels that
//!   overlap each other give way, with hysteresis so the layout doesn't flicker.
//! - Per pane ([`LevelsConfig`], saved with the workspace): all levels, per source, per kind,
//!   and labels. The chart's study visibility applies on top: "hide studies" hides levels,
//!   "hide study labels" and Clean chart hide their labels and legend rows.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use gpui::{
    App, Bounds, Hsla, Pixels, ShapedLine, SharedString, TextAlign, TextRun, Window, fill, point,
    px, size,
};
use lines_core::geometry::snap;
use lines_core::levels::{
    LabelAt, LabelColor, LabelMode, LabelSeen, LegendStat, Level, LevelMap, LevelShape, LevelType,
    MAX_KINDS, MAX_LEVELS, Placed, label_text, place, place_labels,
};
use lines_core::style::DashPattern;
use lines_core::{BarsView, PriceFormat, Product, UtcMillis, Viewport};
use lines_feed::ChartData;
use lines_feed::indicators::{FeedInfo, IndicatorHandle, engine};
use lines_feed::levels::runtime::{self, LevelHandle, LevelStatus};
use lines_feed::levels::{LevelContext, StorePrices};
use lines_feed::store::ChartStore;
use lines_indicators::render::{Column, build_columns};
use lines_indicators::{Bands, PartStyles, Reset, Spec, VwapSpec};
use serde::{Deserialize, Serialize};

use crate::study_style::{self, Look, TypeLook};
use crate::theme::{Theme, drawing_dash, source_colour};

const LABEL_PX: f32 = 11.0;
/// A label box's width beyond its text: its padding (no swatch: the text is in the level's
/// color, and the label sits on its own level).
pub(crate) const LABEL_CHROME: f32 = 12.0;
/// A label's text inset from its box's left edge.
const LABEL_PAD: f32 = 6.0;
/// The labels' gutter beyond its widest chip: a margin left of the chips and right of them.
pub(crate) const GUTTER_PAD: f32 = 8.0;
/// A chip's margin from the plot's right edge.
const GUTTER_RIGHT: f32 = 6.0;
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
    /// Source ids whose labels are off (their levels still drawn).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels_off: Vec<String>,
    /// `source/kind` pairs shown although their parent kind is hidden
    /// (`LevelType::parent`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shown_kinds: Vec<String>,
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

    /// A kind shown, given the kind it was split from (`LevelType::parent`): hiding the
    /// parent hides it too (so a chart saved before a source split a kind keeps hiding what it
    /// hid), unless the kind was shown on its own since (`shown_kinds`).
    pub fn kind_shown_under(&self, source: &str, kind: &str, parent: Option<&str>) -> bool {
        self.kind_shown(source, kind)
            && parent.is_none_or(|p| {
                self.kind_shown(source, p) || Self::listed(&self.shown_kinds, source, kind)
            })
    }

    fn listed(list: &[String], source: &str, kind: &str) -> bool {
        list.iter().any(|k| {
            k.split_once('/')
                .is_some_and(|(s, kd)| s == source && kd == kind)
        })
    }

    /// Show `kind`, hidden through its parent, on its own: the parent (a kind itself or not)
    /// and its other children stay as they are.
    pub fn show_under(&mut self, source: &str, kind: &str) {
        let e = format!("{source}/{kind}");
        self.hidden_kinds.retain(|k| *k != e);
        if !self.shown_kinds.contains(&e) {
            self.shown_kinds.push(e);
            self.shown_kinds.sort();
        }
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
        let e = format!("{source}/{kind}");
        // Hiding a kind shown on its own under a hidden parent ends that too.
        self.shown_kinds.retain(|k| *k != e);
        Self::toggle(&mut self.hidden_kinds, e);
    }

    pub fn labels_shown(&self, source: &str) -> bool {
        !self.hide_labels && !self.labels_off.iter().any(|s| s == source)
    }

    pub fn toggle_labels(&mut self, source: &str) {
        Self::toggle(&mut self.labels_off, source.to_owned());
    }
}

/// One source on this chart.
struct Slot {
    handle: LevelHandle,
    label: &'static str,
    /// The clock its freshness was judged on at the last snapshot (wall live, market in a
    /// replay): external series end where fresh data stopped by it.
    now: UtcMillis,
    map: Option<Arc<LevelMap>>,
    status: LevelStatus,
    /// Its series' freshness (`LevelState::series_status`: refused levels don't stale them).
    series_status: LevelStatus,
    error: Option<String>,
    /// Kind visibility bit per kind index (maps have at most 64 kinds).
    kinds: u64,
    placed: Vec<Placed>,
    /// The chart's style override for this source (decision 0026; default: the source's).
    looks: [Look; 2],
    /// The level types the source declares, and its own label mode.
    types: &'static [LevelType],
    label_default: LabelMode,
    /// This chart's label mode for the source.
    label_mode: LabelMode,
    /// Each kind's look (by kind index of the current map) when the source declares its type
    /// (`study_style::resolve_type`); `None`: drawn as its levels' own style says. Resolved when
    /// the map or the styles change, never per frame.
    kind_looks: [Option<TypeLook>; MAX_KINDS],
    /// Each level's label text in the label mode (by level index), built with the map.
    texts: Vec<Option<String>>,
    /// Bumped whenever the looks above change (the zone scale's cache key).
    rev: u64,
    /// What each level's label did last frame (by level index; `place_labels`' hysteresis),
    /// sized with the map.
    seen: Vec<LabelSeen>,

    /// The label texts last measured for this source (`LevelsLayer::measure_labels`): an
    /// identical new batch measures nothing again.
    measured_texts: Vec<Option<String>>,
    /// What its zone fills are scaled by under the candles (`zone_scale`) for (theme, map,
    /// looks): computed when one of them changes, not every frame.
    zone_scale: Cell<Option<ZoneScale>>,
    /// Each level's label text color (by level index) for (theme name, map, looks revision):
    /// resolved when one of them changes (`Slot::label_colors`), not every frame.
    label_colors: Vec<Hsla>,
    label_colors_key: Option<(&'static str, usize, u64)>,
}

/// (theme name, map pointer, looks revision) and the zone scale computed for them.
type ZoneScale = (&'static str, usize, u64, f32);

impl Slot {
    /// The look of `l`'s type, if the source declares it.
    fn type_look(&self, l: &Level) -> Option<TypeLook> {
        self.kind_looks.get(usize::from(l.kind)).copied().flatten()
    }

    /// Whether map kind `k`'s type is drawn by its style (the style editor's Show), and its
    /// parent's (`LevelType::parent`) where the map has that kind: a child is hidden with its
    /// parent. `true` for a kind without a declared type.
    fn type_drawn(&self, map: &LevelMap, k: usize) -> bool {
        let shown = |j: usize| {
            self.kind_looks
                .get(j)
                .copied()
                .flatten()
                .is_none_or(|t| t.shown)
        };
        let parent = map
            .kinds
            .get(k)
            .and_then(|id| self.types.iter().find(|t| t.id == id))
            .and_then(|t| t.parent)
            .and_then(|p| map.kinds.iter().position(|q| q == p));
        shown(k) && parent.is_none_or(shown)
    }

    /// A zone's fill at `dim`, before the under-candles scale: its type's fill (per-type
    /// override, else the source's Zones, else the type's own, with its light-theme
    /// counterpart), else the source's Zones look over the level's own color.
    fn zone_fill(&self, theme: &Theme, l: &Level, dim: f32) -> Paint {
        match self.type_look(l) {
            Some(t) => theme.part_color(
                t.fill_color(theme.is_dark()),
                source_colour(l.style.rgba, study_style::LEVEL_ZONE_ALPHA * dim),
                t.fill.opacity,
            ),
            None => theme.level_zone(&self.looks, l.style.rgba, dim),
        }
    }

    /// A line's (or, `edge`, a zone edge's) color, width (logical px) and dash at `dim`.
    fn stroke(&self, theme: &Theme, l: &Level, dim: f32, edge: bool, hair: f32) -> Stroke {
        match self.type_look(l) {
            Some(t) if !t.own_line => {
                let w = f32::from(t.line.width.clamp(1, 4));
                // A zone's edges take the source's Zones color at 60 % (decision 0026) when
                // the chart set one and not the type's own line color.
                let (c, a) = match t.edge {
                    Some(z) if edge => (z, 0.6 * dim),
                    _ => (t.line_color(theme.is_dark()), dim),
                };
                Stroke {
                    color: theme.part_color(c, source_colour(l.style.rgba, a), t.line.opacity),
                    width: w,
                    dash: drawing_dash(t.line.line, w),
                }
            }
            _ => {
                let (color, weight) = if edge {
                    (
                        theme.level_zone_edge(&self.looks, l.style.rgba, 0.6 * dim),
                        1,
                    )
                } else {
                    (
                        theme.level_line(&self.looks, l.style.rgba, dim),
                        l.style.weight,
                    )
                };
                Stroke {
                    color,
                    // The source's weight is in device pixels.
                    width: f32::from(weight.clamp(1, 4)) * hair,
                    dash: if l.style.dashed {
                        OWN_DASH
                    } else {
                        DashPattern::SOLID
                    },
                }
            }
        }
    }

    /// The scale (0..=1) on this source's zone fills so the candles over each keep 3:1
    /// (`Theme::under_candles`), and over each stack where they overlap
    /// (`Theme::stack_under_candles`); one scale for all its zones: the most held back.
    /// Computed when the theme, map or looks change (allocating then), not every frame.
    fn zone_scale(&self, map: &Arc<LevelMap>, theme: &Theme) -> f32 {
        let key = Arc::as_ptr(map) as usize;
        if let Some((name, k, rev, out)) = self.zone_scale.get()
            && name == theme.name
            && k == key
            && rev == self.rev
        {
            return out;
        }
        // Each zone's fill and, at fixed prices, its span (a VWAP-relative one can sit
        // anywhere: over any of them).
        let zones: Vec<(Option<(f64, f64)>, Paint)> = map
            .levels
            .iter()
            .filter(|l| matches!(l.shape, LevelShape::Zone { .. }))
            .map(|l| {
                let span = (!l.is_vwap_relative()).then(|| l.span_at(None)).flatten();
                (span, self.zone_fill(theme, l, 1.0))
            })
            .collect();
        let mut out = held_back(theme, zones.iter().map(|(_, f)| *f));
        // And every stack where zones overlap: those containing each fixed zone's low edge
        // (any overlap contains one), alone and under each VWAP-relative zone.
        let mut stack: Vec<Paint> = Vec::with_capacity(zones.len() + 1);
        for (lo, _) in zones.iter().filter_map(|(s, _)| *s) {
            stack.clear();
            stack.extend(
                zones
                    .iter()
                    .filter(|(s, _)| s.is_some_and(|(a, b)| a <= lo && lo <= b))
                    .map(|(_, f)| *f),
            );
            if stack.len() > 1 {
                out = out.min(theme.stack_under_candles(&stack));
            }
            for (_, v) in zones.iter().filter(|(s, _)| s.is_none()) {
                stack.push(*v);
                out = out.min(theme.stack_under_candles(&stack));
                stack.pop();
            }
        }
        self.zone_scale.set(Some((theme.name, key, self.rev, out)));
        out
    }

    /// Each level's label text color in `theme` (by level index), resolved once per (theme,
    /// map, looks): the level's line color, opaque, where it is AA on the label's box over
    /// what is under that box in the gutter (`Theme::level_label_text`: the chart, its grid,
    /// each of this source's zone fills at fixed prices as drawn, and the stack of those
    /// containing the label's price), else the theme's text; the theme's text for a type that
    /// asks for it. VWAP-relative zones are drawn along the bars only, never in the gutter.
    /// Allocates only when one of them changes.
    fn label_colors(&mut self, map: &Arc<LevelMap>, theme: &Theme, hair: f32) {
        let key = (theme.name, Arc::as_ptr(map) as usize, self.rev);
        if self.label_colors_key == Some(key) {
            return;
        }
        let scale = self.zone_scale(map, theme);
        // Each zone at fixed prices: its span and its fill as drawn.
        let zones: Vec<((f64, f64), Paint)> = map
            .levels
            .iter()
            .filter(|l| matches!(l.shape, LevelShape::Zone { .. }) && !l.is_vwap_relative())
            .filter_map(|l| {
                let mut z = self.zone_fill(theme, l, 1.0);
                z.a *= scale;
                Some((l.span_at(None)?, z))
            })
            .collect();
        let alone: Vec<Paint> = zones
            .iter()
            .map(|(_, f)| theme.zones_over_chart([*f]))
            .collect();
        let mut unders: Vec<Paint> = Vec::with_capacity(alone.len() + 1);
        let mut colors = Vec::with_capacity(map.levels.len());
        for l in &map.levels {
            unders.clear();
            unders.extend_from_slice(&alone);
            if let Some(p) = self.label_price(l) {
                unders.push(
                    theme.zones_over_chart(
                        zones
                            .iter()
                            .filter(|((lo, hi), _)| *lo <= p && p <= *hi)
                            .map(|(_, f)| *f),
                    ),
                );
            }
            colors.push(self.label_color(theme, l, hair, &unders));
        }
        self.label_colors = colors;
        self.label_colors_key = Some(key);
    }

    /// The price level `l`'s label sits at (its line's; a zone's edge its type puts it at),
    /// `None` for one moving with the VWAP.
    fn label_price(&self, l: &Level) -> Option<f64> {
        let (lo, hi) = l.span_at(None)?;
        let at = self.type_look(l).map_or(LabelAt::Own, |t| t.label.at);
        Some(if at == LabelAt::Low { lo } else { hi })
    }

    /// Level `l`'s label text color (see [`Slot::label_colors`]); `zones`: what is under its
    /// label, opaque.
    fn label_color(&self, theme: &Theme, l: &Level, hair: f32, zones: &[Paint]) -> Paint {
        let wants = self
            .type_look(l)
            .map_or(LabelColor::Level, |t| t.label.color);
        match wants {
            LabelColor::Text => theme.text_main,
            LabelColor::Level => {
                let c = self.stroke(theme, l, 1.0, false, hair).color;
                theme.level_label_text(c, zones)
            }
        }
    }

    /// Re-resolve the kinds' looks and the label texts (the map or the styles changed).
    fn refresh(&mut self, styles: &BTreeMap<String, PartStyles>, price: &PriceFormat) {
        let empty = PartStyles::new();
        let saved = styles.get(self.handle.source()).unwrap_or(&empty);
        self.looks = looks_for(styles, self.handle.source());
        self.label_mode = study_style::read_label(saved, self.label_default);
        self.kind_looks = [None; MAX_KINDS];
        self.texts.clear();
        if let Some(map) = &self.map {
            for (i, k) in map.kinds.iter().enumerate().take(MAX_KINDS) {
                if let Some(t) = self.types.iter().find(|t| t.id == k) {
                    let ty = study_style::read_type(saved, t);
                    let mut look = study_style::resolve_type(t, &self.looks, &ty);
                    look.label = study_style::read_type_label(saved, t);
                    self.kind_looks[i] = Some(look);
                }
            }
            let kind_looks = &self.kind_looks;
            self.texts.extend(map.levels.iter().map(|l| {
                let at = kind_looks
                    .get(usize::from(l.kind))
                    .copied()
                    .flatten()
                    .map_or(LabelAt::Own, |t| t.label.at);
                label_text(l, self.label_mode, at, price)
            }));
        }
        self.rev = self.rev.wrapping_add(1);
    }
}

/// A color as painted (a name the token test reads as no literal).
type Paint = Hsla;

/// How a line is stroked this frame.
#[derive(Copy, Clone)]
struct Stroke {
    color: Hsla,
    /// Logical px.
    width: f32,
    dash: DashPattern,
}

/// A dashed level without a declared type: the 6/4 dash levels always had.
const OWN_DASH: DashPattern = DashPattern {
    seg: [6.0, 4.0, 0.0, 0.0],
    n: 2,
};

/// Labels' prices before the chart's product is known (never shown: a map arrives after
/// `attach`, which sets the product's).
const FALLBACK_PRICE: PriceFormat = PriceFormat {
    scale: lines_core::price::PriceScale::ONE,
    style: lines_core::price::DisplayStyle::Decimal { digits: 0 },
    tick: 1,
};

/// The scale (0..=1) on zone fills `fills` so the candles over each keep 3:1 in `theme`
/// (`Theme::under_candles`): the most held back.
fn held_back(theme: &Theme, fills: impl Iterator<Item = Hsla>) -> f32 {
    let mut k = 1.0f32;
    for z in fills {
        if z.a > 0.0 {
            k = k.min(theme.under_candles(z, 1).0.a / z.a);
        }
    }
    k
}

/// The scale (0..=1) on zone fills of colors `rgbas` (a source's, as `looks` style them) so
/// the candles over each keep 3:1 in `theme` (`Theme::under_candles`): the most held back.
pub fn zone_scale(theme: &Theme, looks: &[Look; 2], rgbas: impl Iterator<Item = u32>) -> f32 {
    held_back(theme, rgbas.map(|rgba| theme.level_zone(looks, rgba, 1.0)))
}

/// One legend row: the source's label, its legend title (`LevelMap::legend`), its freshness
/// text (empty while current: a current map's time is never shown), whether that is a warning
/// (missing), whether the row is dimmed (stale), and its stats (a line under it).
pub struct LegendRow {
    pub label: &'static str,
    pub title: Option<String>,
    pub text: String,
    pub warn: bool,
    pub dim: bool,
    pub stats: Vec<LegendStat>,
}

pub struct LevelsLayer {
    pub config: LevelsConfig,
    /// Style overrides by source id: the app-wide ones (settings `level_styles`).
    styles: BTreeMap<String, PartStyles>,
    slots: Vec<Slot>,
    shaped: Shaped,
    /// The labels' gutter this frame (plot px at the right, the chart's: 0 when none).
    gutter: f32,
    /// Where labels go this frame: the plot's edge (no gutter set: tests), a gutter's room, or
    /// nowhere (not even a short cut fits: no labels at all).
    room: LabelRoom,
    /// Label text widths as measured, by text (`measure_labels`, at render): kept across
    /// maps, so an identical batch keeps an identical gutter.
    widths: HashMap<String, f32>,
    /// Each label text's shortest useful cut's width (one letter, "…" and its price, or "X…"
    /// without one), by text, measured with its width.
    shortest: HashMap<String, f32>,
    /// A label cut to fit a room, by its full text: (the room, the cut; empty: not even the
    /// shortest cut fits). Made once per text and room, kept across maps.
    cuts: HashMap<String, (f32, String)>,
    /// The chart's price format (labels with the price); `None` before `attach`.
    price: Option<PriceFormat>,
    /// The chart's feed, for the ETH VWAP node.
    feed: Option<FeedInfo>,
    /// The chart's ETH VWAP from the shared indicator engine, acquired only while some map
    /// has VWAP-relative levels.
    vwap: Option<IndicatorHandle>,
    /// This frame's VWAP, one column per visible bar (fixed capacity).
    vwap_cols: Vec<Column>,
    /// Plot px per catalog price unit this frame (offsets from the VWAP to px).
    px_per_unit: f32,
    /// The chart's market clock as last seen under the store lock (freshness of replayed
    /// sources when a snapshot can't read the store).
    clock: Option<UtcMillis>,
    /// The chart's store, product and contract as of [`Self::attach`], to ask the sources
    /// again when their answers change (render time: never reads the store).
    chart: Option<(Arc<ChartStore>, Product, Option<lines_core::ContractKey>)>,
    /// `levels::serving_generation()` when the sources were last asked.
    serving_gen: u64,
}

/// Columns kept for the VWAP that VWAP-relative levels follow.
const VWAP_COLUMNS: usize = 4096;

/// A new map's label state: started over, unless only its series are new (its levels are the
/// previous map's), when each label keeps its hysteresis.
fn reset_seen(map: Option<&LevelMap>, seen: &mut Vec<LabelSeen>) {
    let n = map.map_or(0, |m| m.levels.len());
    if map.is_some_and(|m| m.series_update) && seen.len() == n {
        return;
    }
    seen.clear();
    seen.resize(n, LabelSeen::New);
}

impl LevelsLayer {
    /// A new theme: labels shaped in the old theme's text color are shaped again (their
    /// widths, and so the gutter and the cuts, don't change).
    pub fn theme_changed(&mut self) {
        self.shaped.clear();
    }

    /// A new UI font (`crate::fonts`): labels are measured, cut and shaped again.
    pub fn text_changed(&mut self) {
        self.shaped.clear();
        self.widths.clear();
        self.shortest.clear();
        self.cuts.clear();
        for s in &mut self.slots {
            s.measured_texts.clear();
        }
    }

    pub fn new(config: LevelsConfig) -> LevelsLayer {
        LevelsLayer {
            config,
            styles: BTreeMap::new(),
            slots: Vec::new(),
            shaped: HashMap::new(),
            price: None,
            gutter: 0.0,
            room: LabelRoom::Plot,
            widths: HashMap::new(),
            shortest: HashMap::new(),
            cuts: HashMap::new(),
            feed: None,
            vwap: None,
            vwap_cols: Vec::with_capacity(VWAP_COLUMNS),
            px_per_unit: 0.0,
            clock: None,
            chart: None,
            serving_gen: 0,
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
        self.price = Some(product.price);
        self.chart = Some((store.clone(), product, contract));
        // The old slots stay held while the new ones are acquired, so re-attaching to the
        // same market joins the running tasks (same map, no "missing", no refetch or
        // replay re-read); slots for another market are dropped after.
        self.join_serving();
    }

    /// Ask the registered sources which serve the chart's product (`serves`), keep the slots
    /// of those still serving, add the new ones and drop the rest: a source that doesn't
    /// serve the product has no slot, so nothing drawn and no legend or menu row. An option
    /// chart has no level sources at all: their levels are the underlying's, in another
    /// market's prices (decision 0028 puts options on the underlying's product). Uses what
    /// [`Self::attach`] saved: never reads the store (it runs at render).
    fn join_serving(&mut self) {
        let Some((store, product, contract)) = self.chart.clone() else {
            return;
        };
        self.serving_gen = lines_feed::levels::serving_generation();
        let mut old = std::mem::take(&mut self.slots);
        let sources = if contract.is_some_and(|c| c.option_key().is_some()) {
            Vec::new()
        } else {
            lines_feed::levels::serving(&product)
        };
        let market = (product.id, contract);
        self.slots = sources
            .into_iter()
            .map(|source| {
                if let Some(i) = old
                    .iter()
                    .position(|s| s.handle.source() == source.id() && s.handle.market() == market)
                {
                    return old.swap_remove(i);
                }
                let ctx = LevelContext {
                    product: product.clone(),
                    contract,
                    settings: runtime::settings_for(source.id()),
                };
                let label = source.label();
                let looks = looks_for(&self.styles, source.id());
                let (types, label_default) = (source.types(), source.label_mode());
                let handle = runtime::acquire(source, ctx, Arc::new(StorePrices(store.clone())));
                Slot {
                    zone_scale: Cell::new(None),
                    label_colors: Vec::new(),
                    label_colors_key: None,
                    looks,
                    types,
                    label_default,
                    label_mode: label_default,
                    kind_looks: [None; MAX_KINDS],
                    texts: Vec::new(),
                    rev: 0,
                    seen: Vec::new(),
                    measured_texts: Vec::new(),
                    handle,
                    label,
                    now: UtcMillis(i64::MIN),
                    map: None,
                    status: LevelStatus::Missing,
                    series_status: LevelStatus::Missing,
                    error: None,
                    kinds: u64::MAX,
                    placed: Vec::with_capacity(MAX_LEVELS),
                }
            })
            .collect();
    }

    /// Source `id`'s map, its series' freshness and its clock as of this frame's
    /// [`Self::snapshot`] (`None`:
    /// the source doesn't serve this chart). Lower-pane series studies read their lines here.
    pub fn source_view(
        &self,
        id: &str,
    ) -> Option<(Option<&Arc<LevelMap>>, LevelStatus, UtcMillis)> {
        let s = self.slots.iter().find(|s| s.handle.source() == id)?;
        Some((s.map.as_ref(), s.series_status, s.now))
    }

    /// The style overrides by source id, as saved.
    pub fn styles(&self) -> &BTreeMap<String, PartStyles> {
        &self.styles
    }

    /// Restyle sources (the chart's `StudyStyles::levels`).
    pub fn set_styles(&mut self, styles: &BTreeMap<String, PartStyles>) {
        if *styles == self.styles {
            return;
        }
        self.styles = styles.clone();
        let price = self.price.unwrap_or(FALLBACK_PRICE);
        for s in &mut self.slots {
            s.refresh(&self.styles, &price);
        }
    }

    /// Whether kind `kind` of source `id` is drawn by its type's style (the style editor's
    /// Show); `true` for a kind without a declared type.
    pub fn kind_styled_shown(&self, id: &str, kind: &str) -> bool {
        let Some(s) = self.slots.iter().find(|s| s.handle.source() == id) else {
            return true;
        };
        let Some(k) = s.map.as_ref().and_then(|m| m.kind_index(kind)) else {
            return true;
        };
        s.map
            .as_ref()
            .is_none_or(|m| s.type_drawn(m, usize::from(k)))
    }

    /// The kind `kind` of source `id` was split from (`LevelType::parent`).
    pub fn kind_parent(&self, id: &str, kind: &str) -> Option<&'static str> {
        let s = self.slots.iter().find(|s| s.handle.source() == id)?;
        s.types.iter().find(|t| t.id == kind)?.parent
    }

    /// The level types source `id` declares and its own label mode (its style editor).
    pub fn source_types(&self, id: &str) -> Option<(&'static [LevelType], LabelMode)> {
        let s = self.slots.iter().find(|s| s.handle.source() == id)?;
        Some((s.types, s.label_default))
    }

    /// The first color source `id`'s current map gives its levels of `kind` (a type's
    /// "Source colors" swatch).
    pub fn kind_color(&self, id: &str, kind: &str) -> Option<lines_core::style::Rgb> {
        let s = self.slots.iter().find(|s| s.handle.source() == id)?;
        let map = s.map.as_ref()?;
        let k = map.kind_index(kind)?;
        let rgba = map.levels.iter().find(|l| l.kind == k)?.style.rgba;
        Some(lines_core::style::Rgb::hex(rgba >> 8))
    }

    /// The first color source `id`'s current map gives (its "Source colors" swatch).
    pub fn source_color(&self, id: &str) -> Option<lines_core::style::Rgb> {
        let s = self.slots.iter().find(|s| s.handle.source() == id)?;
        let rgba = s.map.as_ref()?.levels.first()?.style.rgba;
        Some(lines_core::style::Rgb::hex(rgba >> 8))
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

    /// Sum of the sources' revisions (repaint when it changes). A change in which sources
    /// serve the chart counts too (the next snapshot joins or drops them).
    pub fn revision(&self) -> u64 {
        // The generation and the slot count weigh in apart from the sum, so a slot leaving
        // (its revision gone from the sum) can't make the total repeat.
        let start = lines_feed::levels::serving_generation()
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(
                study_style::level_styles_generation().wrapping_mul(0xA24B_AED4_963E_E407),
            )
            .wrapping_add((self.slots.len() as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
        self.slots
            .iter()
            .fold(start, |a, s| a.wrapping_add(s.handle.cell().revision()))
    }

    /// This frame's view of every source (map, freshness, error), taken once per frame
    /// *before* the legend is built (render time). [`Self::legend`] and the draw path
    /// ([`Self::prepare`], paint) both read this snapshot, so the legend never describes a
    /// different state than the levels drawn (a source can update between render and paint).
    /// `market_time`: the chart's market clock, if readable now (else the last one seen).
    pub fn snapshot(&mut self, market_time: Option<UtcMillis>) {
        if market_time.is_some() {
            self.clock = market_time;
        }
        if lines_feed::levels::serving_generation() != self.serving_gen {
            // A source's answer to `serves` changed: join or leave it.
            self.join_serving();
        }
        let clock = self.clock;
        let (styles, price) = (&self.styles, self.price.unwrap_or(FALLBACK_PRICE));
        for s in &mut self.slots {
            let mut fresh = false;
            if let Some(st) = s.handle.cell().try_read() {
                // Freshness on the source's clock: the replay clock in a replay.
                let now = if st.replay {
                    clock.unwrap_or(UtcMillis(i64::MIN))
                } else {
                    lines_feed::now_utc()
                };
                s.status = st.status(now);
                s.series_status = st.series_status(now);
                // External series judge freshness on the market's clock whenever this run
                // isn't live (a replay or a screenshot: a synthetic source runs "live" over
                // it, but its series follow the replay's day), else the wall clock.
                s.now = if lines_feed::levels::runtime::is_live() {
                    lines_feed::now_utc()
                } else {
                    clock.unwrap_or(now)
                };
                if !same(&s.map, &st.map) {
                    s.map = st.map.clone();
                    fresh = true;
                }
                if s.error.as_deref() != st.error.as_deref() {
                    s.error = st.error.clone(); // only when it changes
                }
            }
            if fresh {
                s.refresh(styles, &price);
                // A new map: its levels' labels start from scratch, unless only its series are
                // new (a source's live push carries the previous map's levels), so a push
                // never undoes the labels' hysteresis.
                reset_seen(s.map.as_deref(), &mut s.seen);
            }
        }
    }

    /// Under the store lock, after the viewport update. Draws from this frame's
    /// [`Self::snapshot`] (never re-reads the sources).
    #[cfg(test)]
    pub fn prepare(&mut self, data: &ChartData, bars: BarsView<'_>, vp: &Viewport) {
        self.place(data, vp);
        self.columns(bars, vp);
    }

    /// The VWAP-relative levels' columns for this frame's bars (after the time axis is final:
    /// the labels' gutter moves it).
    pub fn columns(&mut self, bars: BarsView<'_>, vp: &Viewport) {
        if let Some(h) = &self.vwap
            && let Some(out) = h.cell().try_read()
        {
            build_columns(&out.points, bars, vp, Bands(0), &mut self.vwap_cols);
        } else if self.vwap.is_none() {
            self.vwap_cols.clear();
        }
    }

    /// Under the store lock, after the price scale's update: each source's levels and labels
    /// placed by price (the time axis isn't used: it may still move for the labels' gutter).
    pub fn place(&mut self, data: &ChartData, vp: &Viewport) {
        let (plot_w, plot_h) = (vp.width as f32, vp.height as f32);
        if data.market_time.is_some() {
            self.clock = data.market_time;
        }
        // 1. Each source's map and freshness: this frame's snapshot (render time).
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
        let vwap_now = self.vwap.as_ref().and_then(|h| {
            h.cell()
                .try_read()
                .and_then(|o| o.points.last().map(|p| p.vwap))
        });
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
                // Shown on this pane (its parent kind too), and its type (if declared) drawn.
                // A child kind is also hidden while its parent's type is (`Slot::type_drawn`).
                let parent = s.types.iter().find(|t| t.id == k).and_then(|t| t.parent);
                if self.config.kind_shown_under(source, k, parent) && s.type_drawn(map, i) {
                    mask |= 1 << i;
                }
            }
            s.kinds = mask;
            place(map, vp, vwap_now, |k| mask & (1 << k) != 0, &mut s.placed);
            // Each label at its type's edge (kept through collisions: one that clashes is
            // dropped, never moved).
            for p in &mut s.placed {
                p.anchor = s
                    .kind_looks
                    .get(usize::from(map.levels[p.index as usize].kind))
                    .copied()
                    .flatten()
                    .map_or(LabelAt::Own, |t| t.label.at);
            }
            // A label that can't be drawn in its room (its full text measured, else its cut for
            // this room: an empty cut means none fits) claims no room. No room at all (labels
            // can't show): none placed.
            let room = self.room.at(plot_w);
            let (texts, widths, cuts) = (&s.texts, &self.widths, &self.cuts);
            place_labels(&mut s.placed, LABEL_PX * 1.6, plot_h, &mut s.seen, |i| {
                let i = i as usize;
                let Some(name) = map.levels[i].label.as_deref() else {
                    return false;
                };
                let Some(room) = room else {
                    return false;
                };
                let full = texts.get(i).and_then(|t| t.as_deref()).unwrap_or(name);
                let w = widths.get(full).copied().unwrap_or_else(|| estimate(full));
                label_can_show(w, cuts.get(full), room)
            });
        }
    }

    fn each(&self, mut f: impl FnMut(&Arc<LevelMap>, &Placed, f32, &Slot)) {
        for s in &self.slots {
            let Some(map) = &s.map else { continue };
            // Stale maps are dimmed (shown as stale, never as current).
            let dim = if matches!(s.status, LevelStatus::Stale { .. }) {
                0.4
            } else {
                1.0
            };
            for p in &s.placed {
                f(map, p, dim, s);
            }
        }
    }

    /// What the labels' gutter needs, from the whole level set (every labeled level of every
    /// source shown with its labels on, kinds and types shown; never only those in view, so it
    /// changes only with the levels, the label settings or the zoom): (the widest label with
    /// its chip and margins, the widest shortest cut likewise). (0, 0) when no label would
    /// show. Measured at render (`measure_labels`), else estimated. No allocation.
    pub fn gutter_need(&self) -> (f32, f32) {
        if self.config.hide_labels {
            return (0.0, 0.0);
        }
        let (mut widest, mut cut) = (0.0f32, 0.0f32);
        for s in &self.slots {
            let Some(map) = &s.map else { continue };
            let source = s.handle.source();
            if !self.config.source_shown(source)
                || !self.config.labels_shown(source)
                || s.status == LevelStatus::Missing
            {
                continue;
            }
            for (i, l) in map.levels.iter().enumerate() {
                let Some(name) = l.label.as_deref() else {
                    continue;
                };
                let kind = map.kinds.get(usize::from(l.kind)).map(String::as_str);
                let parent = kind
                    .and_then(|k| s.types.iter().find(|t| t.id == k))
                    .and_then(|t| t.parent);
                let shown = kind.is_some_and(|k| self.config.kind_shown_under(source, k, parent))
                    && s.type_drawn(map, usize::from(l.kind));
                if !shown {
                    continue;
                }
                let text = s.texts.get(i).and_then(|t| t.as_deref()).unwrap_or(name);
                let w = self
                    .widths
                    .get(text)
                    .copied()
                    .unwrap_or_else(|| estimate(text));
                widest = widest.max(w);
                // Its shortest cut, under the same filter (a hidden type's long price never
                // decides whether the shown labels fit).
                let c = self.shortest.get(text).copied().unwrap_or_else(|| {
                    match text.rsplit_once(" : ") {
                        Some((_, p)) => (p.chars().count() + 5) as f32 * LABEL_PX * 0.62,
                        None => 2.0 * LABEL_PX * 0.62,
                    }
                });
                cut = cut.max(c);
            }
        }
        if widest <= 0.0 {
            return (0.0, 0.0);
        }
        let chip = LABEL_CHROME + GUTTER_PAD;
        (widest + chip, cut + chip)
    }

    /// The chart's labels' gutter for this frame (0: none; labels then right-aligned at the
    /// plot's edge, as wide as a plot `plot_w` px wide allows), and whether labels fit at all
    /// (`fits`: false when not even a short cut fits the cap: no labels).
    pub fn set_gutter(&mut self, w: f32, plot_w: f32, fits: bool) {
        self.gutter = w.max(0.0);
        self.room = if fits {
            LabelRoom::Room(gutter_room(self.gutter, plot_w))
        } else {
            LabelRoom::None
        };
    }

    /// Measure the label set of every source shown with its labels on (in render, before
    /// [`Self::gutter_need`]): each label text's width and its shortest cut's width once, kept
    /// by text. An identical batch measures nothing.
    pub fn measure_labels(&mut self, mut measure: impl FnMut(&str) -> f32) {
        if self.config.hide_labels {
            return;
        }
        // Bounded: past what the current maps (and one more) can hold, texts no current map
        // uses are dropped (the current ones stay, so nothing is measured again).
        let limit = (self.slots.len() + 1) * MAX_LEVELS;
        if self.widths.len() > limit || self.cuts.len() > limit {
            let slots = &self.slots;
            let used = |k: &String| {
                slots
                    .iter()
                    .any(|s| s.texts.iter().flatten().any(|t| t == k))
            };
            self.widths.retain(|k, _| used(k));
            self.shortest.retain(|k, _| used(k));
            self.cuts.retain(|k, _| used(k));
        }
        for s in &mut self.slots {
            let Some(map) = &s.map else { continue };
            let source = s.handle.source();
            // As placing and the gutter do: a slot with no levels yet (its first map's levels
            // refused) shows no labels.
            if s.status == LevelStatus::Missing
                || !self.config.source_shown(source)
                || !self.config.labels_shown(source)
            {
                continue;
            }
            if s.texts == s.measured_texts {
                continue;
            }
            for (i, l) in map.levels.iter().enumerate() {
                let Some(name) = l.label.as_deref() else {
                    continue;
                };
                let text = s.texts.get(i).and_then(|t| t.as_deref()).unwrap_or(name);
                if self.widths.contains_key(text) {
                    continue;
                }
                self.widths.insert(text.to_owned(), measure(text));
                let w = match shortest_cut(name, text) {
                    Some(cut) => measure(&cut),
                    None => self.widths[text],
                };
                self.shortest.insert(text.to_owned(), w);
            }
            s.measured_texts = s.texts.clone();
        }
    }

    /// Zone fills (behind the candles).
    pub fn paint_back(&self, plot: Bounds<Pixels>, theme: &Theme, window: &mut Window) {
        let scale = window.scale_factor();
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        let pw = f32::from(plot.size.width);
        let ppu = self.px_per_unit;
        let cols = &self.vwap_cols;
        self.each(|map, p, dim, s| {
            let l = &map.levels[p.index as usize];
            // Held back so the candles over it keep 3:1.
            let mut zone = s.zone_fill(theme, l, dim);
            zone.a *= s.zone_scale(map, theme);
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
                            zone,
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
                    zone,
                ));
            }
        });
    }

    /// Lines and zone edges (above the candles).
    pub fn paint_front(&self, plot: Bounds<Pixels>, theme: &Theme, window: &mut Window) {
        let scale = window.scale_factor();
        let hair = 1.0 / scale;
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        let pw = f32::from(plot.size.width);
        let line = |window: &mut Window, y: f32, st: Stroke| {
            let w = st.width.max(hair);
            let y = snap(f64::from(y - w * 0.5), scale);
            crate::eyedropper::rect(
                Bounds::new(point(px(ox), px(oy + y)), size(px(pw), px(w))),
                st.color,
                "Level",
            );
            // Across the bars to the plot's edge; in the labels' gutter the chip covers it.
            st.dash.segments(pw, |a, b| {
                window.paint_quad(fill(
                    Bounds::new(point(px(ox + a), px(oy + y)), size(px(b - a), px(w))),
                    st.color,
                ));
            });
        };
        let ppu = self.px_per_unit;
        let cols = &self.vwap_cols;
        // A VWAP-relative edge: a segment across each bar at VWAP + offset, joined to the
        // previous bar of the same session; dashed draws every other bar.
        let along = |window: &mut Window, offset: f64, st: Stroke| {
            let (w, dashed, c) = (st.width.max(hair), !st.dash.is_solid(), st.color);
            let dy = offset as f32 * ppu;
            let mut prev: Option<f32> = None;
            for (i, col) in cols.iter().enumerate() {
                let y = col.y - dy;
                let seg = Bounds::new(
                    point(px(ox + col.left), px(oy + y - w * 0.5)),
                    size(px(col.right - col.left), px(w)),
                );
                crate::eyedropper::rect(seg, c, "Level");
                if !(dashed && i % 2 == 1) {
                    window.paint_quad(fill(seg, c));
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
        self.each(|map, p, dim, s| {
            let l = &map.levels[p.index as usize];
            let zone = matches!(l.shape, LevelShape::Zone { .. });
            let st = s.stroke(theme, l, dim, zone, hair);
            if let (true, Some((lo, hi))) = (p.vwap_relative, l.vwap_offsets()) {
                along(window, hi, st);
                if zone {
                    along(window, lo, st);
                }
                return;
            }
            line(window, p.top, st);
            if zone {
                line(window, p.bottom, st);
            }
        });
    }

    /// Labels: on the legend box at the plot's right edge, in the level's color where that is
    /// AA there, else theme text (`Slot::label_colors`); a stale map's in dim text.
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
        let Some(room) = self.room.at(pw) else {
            return;
        };
        let hair = 1.0 / scale;
        for si in 0..self.slots.len() {
            let Some(map) = self.slots[si].map.clone() else {
                continue;
            };
            if !self.config.labels_shown(self.slots[si].handle.source()) {
                continue;
            }
            let dim = matches!(self.slots[si].status, LevelStatus::Stale { .. });
            // Resolved once per theme, map and looks (allocates only then).
            self.slots[si].label_colors(&map, theme, hair);
            for pi in 0..self.slots[si].placed.len() {
                let p = self.slots[si].placed[pi];
                let level = &map.levels[p.index as usize];
                let (Some(y), Some(name)) = (p.label_y, level.label.as_deref()) else {
                    continue;
                };
                // The label mode's text (`name : price`), built with the map, in its room: the
                // room for its text: the gutter's (the chip right-aligned in it), else the
                // plot's. Too wide: the name is cut with "…" before the price (never the
                // price); still too wide, none (never a label running past its room). A cut is
                // made once per text and room (`cuts`, kept across maps), not per frame.
                let i = p.index as usize;
                let full = self.slots[si]
                    .texts
                    .get(i)
                    .and_then(|t| t.as_deref())
                    .unwrap_or(name);
                // Measured in render (`measure_labels`); shaped only to be drawn.
                let full_w = match self.widths.get(full) {
                    Some(w) => *w,
                    None => shaped_width(&mut self.shaped, full, theme, window),
                };
                let full_fits = full_w <= room;
                if !full_fits {
                    // Candidates measured without keeping them (only the cut is shaped and
                    // cached): a narrow pane doesn't flood the shaped labels.
                    ensure_cut(&mut self.cuts, full, name, room, |t| {
                        measure(t, theme, window)
                    });
                }
                let text: &str = if full_fits {
                    full
                } else {
                    match self.cuts.get(full) {
                        Some((_, t)) if !t.is_empty() => t.as_str(),
                        _ => continue,
                    }
                };
                // Its color (stale: dim text, as the whole map is dimmed).
                let color = if dim {
                    theme.text_dim
                } else {
                    self.slots[si]
                        .label_colors
                        .get(i)
                        .copied()
                        .unwrap_or(theme.text_main)
                };
                let line = shaped_in(&mut self.shaped, text, color, window);
                let w = f32::from(line.width) + LABEL_CHROME;
                let x = snap(f64::from((pw - w - GUTTER_RIGHT).max(0.0)), scale);
                let top = snap(f64::from(y - box_h * 0.5), scale);
                window.paint_quad(fill(
                    Bounds::new(point(px(ox + x), px(oy + top)), size(px(w), px(box_h))),
                    theme.label_bg,
                ));
                let _ = line.paint(
                    point(
                        px(ox + x + LABEL_PAD),
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

    /// Source `i`'s map again as a new batch of the same levels (a new `Arc`), as the runtime
    /// publishes each refresh (tests).
    #[cfg(test)]
    pub fn renew_map(&mut self, i: usize) {
        let price = self.price.unwrap_or(FALLBACK_PRICE);
        let s = &mut self.slots[i];
        if let Some(m) = &s.map {
            s.map = Some(Arc::new((**m).clone()));
            s.refresh(&self.styles, &price);
        }
    }

    /// Level `l` of source `i` cut for `room` as paint cuts it (tests): the cut, or `None`
    /// when not even its shortest cut fits.
    #[cfg(test)]
    pub fn cut_for(
        &mut self,
        i: usize,
        l: usize,
        room: f32,
        measure: impl FnMut(&str) -> f32,
    ) -> Option<String> {
        let s = &self.slots[i];
        let name = s.map.as_ref()?.levels[l].label.clone()?;
        let full = s.texts.get(l).cloned().flatten().unwrap_or(name.clone());
        ensure_cut(&mut self.cuts, &full, &name, room, measure);
        self.cuts
            .get(&full)
            .map(|(_, t)| t.clone())
            .filter(|t| !t.is_empty())
    }

    /// Fill the text caches with `n` texts no map uses (tests: eviction).
    #[cfg(test)]
    pub fn stuff_caches(&mut self, n: usize) {
        for k in 0..n {
            self.widths.insert(format!("unused {k}"), 1.0);
        }
    }

    /// Texts in the width cache (tests).
    #[cfg(test)]
    pub fn cached_widths(&self) -> usize {
        self.widths.len()
    }

    /// Each level's label text color of source `i` in `theme`, as paint resolves it (tests).
    #[cfg(test)]
    pub fn label_colors_for(&mut self, i: usize, theme: &Theme) -> Vec<Hsla> {
        let s = &mut self.slots[i];
        let Some(map) = s.map.clone() else {
            return Vec::new();
        };
        s.label_colors(&map, theme, 1.0);
        s.label_colors.clone()
    }

    /// Label texts per level of source `i` in its label mode (tests).
    #[cfg(test)]
    pub fn label_texts(&self, i: usize) -> &[Option<String>] {
        &self.slots[i].texts
    }

    /// Kind looks of source `i` (tests).
    #[cfg(test)]
    pub fn kind_look(&self, i: usize, kind: &str) -> Option<TypeLook> {
        let s = &self.slots[i];
        let k = s.map.as_ref()?.kind_index(kind)?;
        s.kind_looks[usize::from(k)]
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

    /// Source `i`'s map as of the last snapshot (tests).
    #[cfg(test)]
    pub fn map(&self, i: usize) -> Option<&Arc<LevelMap>> {
        self.slots[i].map.as_ref()
    }

    /// Override a source's freshness (tests: stale and missing rendering).
    #[cfg(test)]
    pub fn force_status(&mut self, i: usize, status: LevelStatus) {
        self.slots[i].status = status;
    }

    /// Legend rows (render time, after [`Self::snapshot`]): one per source on this chart,
    /// with its freshness, from the same snapshot the levels are drawn from.
    pub fn legend(&self) -> Vec<LegendRow> {
        self.slots
            .iter()
            .filter(|s| self.config.source_shown(s.handle.source()))
            .map(|s| {
                // A map's time is never shown in the legend (it says nothing a trader acts on
                // while current); a stale map says the word "stale", dimmed with its levels.
                let (text, warn, dim) = match s.status {
                    LevelStatus::Missing => (
                        match &s.error {
                            Some(e) => format!("missing · {}", without_name(s.label, e)),
                            None => "missing".into(),
                        },
                        true,
                        false,
                    ),
                    LevelStatus::Stale { .. } => ("stale".to_owned(), false, true),
                    LevelStatus::Current => (String::new(), false, false),
                };
                // The source's own legend content, from a map it has drawn (not while missing).
                let legend = s
                    .map
                    .as_ref()
                    .filter(|_| s.status != LevelStatus::Missing)
                    .and_then(|m| m.legend.as_ref());
                LegendRow {
                    label: s.label,
                    title: legend.and_then(|l| l.title.clone()),
                    text,
                    warn,
                    dim,
                    stats: legend.map_or_else(Vec::new, |l| l.stats.clone()),
                }
            })
            .collect()
    }
}

/// The room for a label's text in a gutter `gutter` px wide (already in whole steps): the
/// gutter less its chip and margins, exactly, so the label that sized the gutter fits it
/// whole. No gutter (0): the plot's width less the same, in the gutter's 8 px steps (a pane
/// resized by a pixel doesn't cut its labels again).
pub(crate) fn gutter_room(gutter: f32, plot_w: f32) -> f32 {
    if gutter > 0.0 {
        return gutter - LABEL_CHROME - GUTTER_PAD;
    }
    let r = plot_w - LABEL_CHROME - GUTTER_PAD;
    (r / lines_core::levels::GUTTER_STEP).floor() * lines_core::levels::GUTTER_STEP
}

/// The shortest text a label can show as (what `lines_core::levels::cut_label` can cut it to
/// at most): its name's first letter, "…" and its price (or "X…" without one); `None` when the
/// name is one letter (it can't be cut: the full text is the shortest).
pub(crate) fn shortest_cut(name: &str, text: &str) -> Option<String> {
    if name.chars().count() < 2 {
        return None;
    }
    let first: String = name.chars().take(1).collect();
    Some(match text.rsplit_once(" : ") {
        Some((_, p)) => format!("{first}… : {p}"),
        None => format!("{first}…"),
    })
}

/// Cut label `full` (named `name`) to fit `room` once (kept by text and room in `cuts`; an
/// empty cut: not even the shortest fits). Measures only on a miss.
fn ensure_cut(
    cuts: &mut HashMap<String, (f32, String)>,
    full: &str,
    name: &str,
    room: f32,
    measure: impl FnMut(&str) -> f32,
) {
    if cuts.get(full).is_some_and(|(r, _)| *r == room) {
        return;
    }
    let price = full.rsplit_once(" : ").map(|(_, p)| p);
    let cut = lines_core::levels::cut_label(name, price, room, measure);
    cuts.insert(full.to_owned(), (room, cut.unwrap_or_default()));
}

/// Where labels go this frame.
#[derive(Copy, Clone, Debug, PartialEq)]
enum LabelRoom {
    /// No gutter set (tests): the plot's edge, as wide as the plot allows.
    Plot,
    /// The gutter's room for a label's text.
    Room(f32),
    /// Not even a short cut fits: no labels at all.
    None,
}

impl LabelRoom {
    fn at(self, plot_w: f32) -> Option<f32> {
        match self {
            LabelRoom::Plot => Some(gutter_room(0.0, plot_w)),
            LabelRoom::Room(r) => Some(r),
            LabelRoom::None => None,
        }
    }
}

/// Whether a label `w` px wide (its full text) can show in `room`: whole, or cut (a cut made
/// for this room says; empty: not even one letter and its price fit). Not cut yet for this
/// room: yes (the first paint cuts it, and an empty cut frees the room the next frame).
pub(crate) fn label_can_show(w: f32, cut: Option<&(f32, String)>, room: f32) -> bool {
    if w <= room {
        return true;
    }
    match cut {
        Some((r, t)) if *r == room => !t.is_empty(),
        _ => true,
    }
}

/// A label's width from its length, on the wide side (before it was shaped).
fn estimate(t: &str) -> f32 {
    t.chars().count() as f32 * LABEL_PX * 0.62
}

/// `text`'s width as a label, shaped and not kept (a cut's candidates).
pub(crate) fn measure(text: &str, theme: &Theme, window: &mut Window) -> f32 {
    measure_at(text, LABEL_PX, theme, window)
}

/// `text`'s width at `size` px in the window's font, shaped and not kept.
pub(crate) fn measure_at(text: &str, size: f32, theme: &Theme, window: &mut Window) -> f32 {
    let run = TextRun {
        len: text.len(),
        font: crate::fonts::ui(),
        color: theme.text_main,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    f32::from(
        window
            .text_system()
            .shape_line(SharedString::from(text.to_owned()), px(size), &[run], None)
            .width,
    )
}

/// What of a source's legend row fits a legend `room` px wide (inside its padding), measured
/// with `measure`: its title after the source's name (`head`: the name's width and the gap
/// and "· " before the title) cut with "…" at a whole character, and how many of its stats
/// (`stat_w`: each stat's width, name and value with their gap; `gap` between stats) fit
/// whole, in order, with room for a closing "…" when some don't. A number is never cut.
pub struct LegendFit {
    /// The title as shown (`None`: none, or not even "…" fits).
    pub title: Option<String>,
    /// How many stats show, from the first.
    pub stats: usize,
    /// Some stats don't fit: the line ends with "…".
    pub more: bool,
}

pub fn fit_legend(
    room: f32,
    head: f32,
    title: Option<&str>,
    stat_w: &[f32],
    gap: f32,
    mut measure: impl FnMut(&str) -> f32,
) -> LegendFit {
    let title = title.and_then(|t| {
        let left = room - head;
        if measure(t) <= left {
            return Some(t.to_owned());
        }
        // The longest prefix (whole characters, trailing spaces dropped) with "…" that fits.
        let ends: Vec<usize> = t.char_indices().map(|(i, _)| i).skip(1).collect();
        ends.iter().rev().find_map(|&e| {
            let cut = format!("{}…", t[..e].trim_end());
            (measure(&cut) <= left).then_some(cut)
        })
    });
    // The first `k` stats' width.
    let width = |k: usize| stat_w[..k].iter().sum::<f32>() + gap * k.saturating_sub(1) as f32;
    let n = if width(stat_w.len()) <= room {
        stat_w.len()
    } else {
        let more_w = measure("…");
        (0..stat_w.len())
            .rev()
            .find(|&k| width(k) + if k > 0 { gap } else { 0.0 } + more_w <= room)
            .unwrap_or(0)
    };
    LegendFit {
        title,
        stats: n,
        more: n < stat_w.len(),
    }
}

/// Labels shaped and kept, by text: one per color the text is drawn in.
type Shaped = HashMap<String, Vec<(Hsla, ShapedLine)>>;

/// Most colors a label text is kept shaped in (see [`shaped_in`]).
const SHAPED_COLORS: usize = 3;

/// `text` shaped as a label in `color` (shaped once per distinct text and color and kept, at
/// most [`SHAPED_COLORS`] colors per text; a cache miss allocates, as the map's labels are
/// new). A stale/current flip shapes each label once in its other color and then keeps both,
/// so flips back and forth shape nothing (GPUI's line layout cache serves the glyphs of a
/// text shaped the frame before, whatever its color).
fn shaped_in<'a>(
    shaped: &'a mut Shaped,
    text: &str,
    color: Hsla,
    window: &mut Window,
) -> &'a ShapedLine {
    let have = shaped
        .get(text)
        .and_then(|v| v.iter().position(|(c, _)| *c == color));
    let at = match have {
        Some(i) => i,
        None => {
            if shaped.len() >= MAX_SHAPED && !shaped.contains_key(text) {
                shaped.clear();
            }
            let run = TextRun {
                len: text.len(),
                font: crate::fonts::ui(),
                color,
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
            let v = shaped.entry(text.to_owned()).or_default();
            // Bounded per text: its level color, the stale dim and one more (a restyle); the
            // oldest goes first.
            if v.len() >= SHAPED_COLORS {
                v.remove(0);
            }
            v.push((color, line));
            v.len() - 1
        }
    };
    &shaped[text][at].1
}

/// `text`'s width as a label (shaped once per distinct text and kept; a cache miss
/// allocates, as the map's labels are new).
fn shaped_width(shaped: &mut Shaped, text: &str, theme: &Theme, window: &mut Window) -> f32 {
    if let Some((_, l)) = shaped.get(text).and_then(|v| v.first()) {
        return f32::from(l.width);
    }
    f32::from(shaped_in(shaped, text, theme.text_main, window).width)
}

/// A source's error without its own name in front (`"Acme: not translated: …"` on the
/// `Acme` row reads `not translated: …`): the legend row already names the source.
pub fn without_name<'a>(label: &str, error: &'a str) -> &'a str {
    let e = error.trim_start();
    match e.get(..label.len()) {
        Some(head) if head.eq_ignore_ascii_case(label) => {
            let rest = &e[label.len()..];
            match rest.trim_start().strip_prefix(':') {
                Some(r) => r.trim_start(),
                None => error,
            }
        }
        _ => error,
    }
}

/// A source's override from the saved styles (none: the source's own style).
fn looks_for(styles: &BTreeMap<String, PartStyles>, id: &str) -> [Look; 2] {
    styles.get(id).map_or_else(
        || Look::defaults(&study_style::LEVELS),
        |m| study_style::read(m, &study_style::LEVELS),
    )
}

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
