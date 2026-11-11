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
//!   state. Labels are shaped once per distinct text and color (a cache miss allocates).
//! - Freshness (docs/design.md §7): a **stale** map is drawn dimmed and says so in the legend;
//!   a **missing** one draws nothing and says "missing". Never shown as current.
//! - Labels stay on their own levels (`place_labels`): the price never moves them, and only
//!   labels that overlap each other give way (stacked a row away, else dropped), with
//!   hysteresis so the layout doesn't flicker.
//! - Where labels go is the chart's choice ([`LabelPlace`], `LevelsConfig::label_place`; the
//!   profile's tag follows it too): **tags in the price axis** (the default: each a tag at its
//!   price, outlined in its color; a zone's at its labelled edge; the axis grows to its widest
//!   tag; one that clashes stacks a row away with a short leader to its price, and none covers
//!   the last-price tag), **a column** of their own between the plot and the axis (a fixed
//!   width the trader drags; long names end in "…"), or **on the chart** (just right of the
//!   newest bar and moving with it; pinned to the plot's right edge when it is off screen;
//!   one under the pointer fades). No room is ever reserved inside the plot: the bars run to
//!   its edge.
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
/// A label's room on the chart beyond its widest chip: a margin left of the chips and right
/// of them.
pub(crate) const GUTTER_PAD: f32 = 8.0;
/// A chip's margin from the plot's right edge (labels on the chart, pinned there).
const GUTTER_RIGHT: f32 = 6.0;
/// A label on the chart sits this far right of the newest bar.
const NEWEST_GAP: f32 = 6.0;
/// A tag in the price axis: the room left of it for its leader, and its margin from the
/// axis's right edge.
pub(crate) const AXIS_LEADER: f32 = 6.0;
pub(crate) const AXIS_EDGE: f32 = 2.0;
/// The widest the price axis grows for level tags (logical px at 100 % zoom).
pub(crate) const AXIS_TAG_CAP: f32 = 168.0;
/// The label column's margin each side of its labels.
pub(crate) const COLUMN_PAD: f32 = 4.0;
/// The label column's width (logical px at 100 % zoom): new, and the most and least a drag
/// makes it.
pub const COLUMN_W_DEFAULT: u16 = 120;
pub const COLUMN_W_MIN: u16 = 48;
pub const COLUMN_W_MAX: u16 = 320;
/// A label on the chart under the pointer fades to this opacity, so the bars under it show.
const HOVER_FADE: f32 = 0.25;

/// The study labels' metrics at a UI zoom factor (`crate::zoom`, 1.0 at 100 %): every size,
/// padding and step of a level label, chip and tag (the consts above are their 100 % values).
/// Only the UI zoom changes them: the chart's time and price zoom never do.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct LabelMetrics {
    /// Text size.
    pub text: f32,
    /// A chip's height (the stacking step between labels).
    pub box_h: f32,
    pub chrome: f32,
    pub pad: f32,
    pub gutter_pad: f32,
    pub gutter_right: f32,
    pub newest_gap: f32,
    pub axis_leader: f32,
    pub axis_edge: f32,
    pub column_pad: f32,
}

impl LabelMetrics {
    pub(crate) fn at(z: f32) -> LabelMetrics {
        LabelMetrics {
            text: LABEL_PX * z,
            box_h: LABEL_PX * 1.6 * z,
            chrome: LABEL_CHROME * z,
            pad: LABEL_PAD * z,
            gutter_pad: GUTTER_PAD * z,
            gutter_right: GUTTER_RIGHT * z,
            newest_gap: NEWEST_GAP * z,
            axis_leader: AXIS_LEADER * z,
            axis_edge: AXIS_EDGE * z,
            column_pad: COLUMN_PAD * z,
        }
    }

    /// At the UI zoom now.
    fn now() -> LabelMetrics {
        LabelMetrics::at(crate::zoom::get())
    }
}

pub(crate) fn label_px() -> f32 {
    LabelMetrics::now().text
}
pub(crate) fn label_chrome() -> f32 {
    LabelMetrics::now().chrome
}
fn label_pad() -> f32 {
    LabelMetrics::now().pad
}
pub(crate) fn gutter_pad() -> f32 {
    LabelMetrics::now().gutter_pad
}
fn gutter_right() -> f32 {
    LabelMetrics::now().gutter_right
}
fn newest_gap() -> f32 {
    LabelMetrics::now().newest_gap
}
pub(crate) fn axis_leader() -> f32 {
    LabelMetrics::now().axis_leader
}
pub(crate) fn axis_edge() -> f32 {
    LabelMetrics::now().axis_edge
}
pub(crate) fn column_pad() -> f32 {
    LabelMetrics::now().column_pad
}

/// Where a chart's study labels go (owner request; the level labels and the volume
/// profile's tag). Saved per chart (`LevelsConfig::label_place`), leniently: a value this
/// build doesn't know reads as the default, never a corrupt workspace.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "serde_json::Value", into = "String")]
pub enum LabelPlace {
    /// A tag per label in the price axis, at its price (no room taken from the bars).
    #[default]
    Axis,
    /// A column of their own between the plot and the price axis.
    Column,
    /// On the chart, just right of the newest bar (over bars when it is off screen).
    Chart,
}

impl LabelPlace {
    pub const ALL: [LabelPlace; 3] = [LabelPlace::Axis, LabelPlace::Column, LabelPlace::Chart];

    /// Its menu and editor name.
    pub fn label(self) -> &'static str {
        match self {
            LabelPlace::Axis => "Axis",
            LabelPlace::Column => "Column",
            LabelPlace::Chart => "Chart",
        }
    }

    /// Where it puts them, in words (the palette).
    pub fn words(self) -> &'static str {
        match self {
            LabelPlace::Axis => "on the price axis",
            LabelPlace::Column => "in a column",
            LabelPlace::Chart => "on the chart",
        }
    }

    fn is_axis(&self) -> bool {
        *self == LabelPlace::Axis
    }
}

impl From<serde_json::Value> for LabelPlace {
    fn from(v: serde_json::Value) -> LabelPlace {
        match v.as_str() {
            Some("column") => LabelPlace::Column,
            Some("chart") => LabelPlace::Chart,
            _ => LabelPlace::Axis,
        }
    }
}

impl From<LabelPlace> for String {
    fn from(p: LabelPlace) -> String {
        match p {
            LabelPlace::Axis => "axis",
            LabelPlace::Column => "column",
            LabelPlace::Chart => "chart",
        }
        .to_owned()
    }
}

/// Where this frame's labels are drawn ([`LevelsLayer::paint_labels`]), in plot px.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct LabelStrip {
    /// The price axis's or the label column's left edge (unused on the chart).
    pub left: f32,
    /// The newest bar's right edge, when it is on the plot: labels on the chart go just
    /// right of it.
    pub newest_right: Option<f32>,
    /// The pointer, for the fade of a label on the chart under it.
    pub hover: Option<(f32, f32)>,
}
/// Most labels shaped and kept, by (text, color) (cleared when exceeded).
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
    /// Where this chart's study labels go (default: tags in the price axis).
    #[serde(skip_serializing_if = "LabelPlace::is_axis")]
    pub label_place: LabelPlace,
    /// The label column's width (logical px at 100 % zoom) as last dragged or fitted;
    /// `None`: [`COLUMN_W_DEFAULT`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_column: Option<u16>,
}

impl LevelsConfig {
    pub fn is_default(&self) -> bool {
        *self == LevelsConfig::default()
    }

    /// The label column's width at 100 % zoom, within its limits.
    pub fn column_w(&self) -> f32 {
        f32::from(
            self.label_column
                .unwrap_or(COLUMN_W_DEFAULT)
                .clamp(COLUMN_W_MIN, COLUMN_W_MAX),
        )
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
    label_colors_key: Option<(&'static str, usize, u64, i32, u32)>,
    /// `label_colors`' buffers: the fixed zones (span, fill as drawn), the fills moving with
    /// the VWAP, and what is under one label.
    label_zones: Vec<((f64, f64), Paint)>,
    label_moving: Vec<Paint>,
    label_unders: Vec<Paint>,
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
        let mut c = match self.type_look(l) {
            Some(t) => theme.part_color(
                t.fill_color(theme.is_dark()),
                source_colour(l.style.rgba, study_style::LEVEL_ZONE_ALPHA),
                t.fill.opacity,
            ),
            None => theme.level_zone(&self.looks, l.style.rgba, 1.0),
        };
        // Dimmed after the color resolves: a type's or a user's color dims with its map too.
        c.a *= dim;
        c
    }

    /// A line's (or, `edge`, a zone edge's) color, width (logical px) and dash at `dim`.
    fn stroke(&self, theme: &Theme, l: &Level, dim: f32, edge: bool, hair: f32) -> Stroke {
        match self.type_look(l) {
            Some(t) if !t.own_line => {
                let w = f32::from(t.line.width.clamp(1, 4));
                // A zone's edges take the source's Zones color at 60 % (decision 0026) when
                // the chart set one and not the type's own line color.
                let (c, a) = match t.edge {
                    Some(z) if edge => (z, 0.6),
                    _ => (t.line_color(theme.is_dark()), 1.0),
                };
                // Dimmed after the color resolves: a type's or a user's color dims too.
                let mut color = theme.part_color(c, source_colour(l.style.rgba, a), t.line.opacity);
                color.a *= dim;
                Stroke {
                    color,
                    width: w,
                    dash: drawing_dash(t.line.line, w),
                }
            }
            _ => {
                let (mut color, weight) = if edge {
                    (theme.level_zone_edge(&self.looks, l.style.rgba, 0.6), 1)
                } else {
                    (
                        theme.level_line(&self.looks, l.style.rgba, 1.0),
                        l.style.weight,
                    )
                };
                color.a *= dim;
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
    /// map, looks, zoom step): the level's line color, opaque, where it is AA on the label's
    /// box over everything of this source's that can be under that box
    /// (`Theme::level_label_text`: the chart, its grid, each of its zone and band fills as
    /// drawn, and each stack of its fixed zones across the box's whole height, a row either
    /// way for a label stacked off its anchor) and clear of the last price's color, else the
    /// theme's text; the theme's text for a type that asks for it. A band moving with the VWAP
    /// can sit anywhere, so its fill is checked alone and under each stack. Another source's
    /// zones aren't checked. Reuses its buffers: no allocation once they have grown.
    fn label_colors(&mut self, map: &Arc<LevelMap>, theme: &Theme, hair: f32, ppu: f32) {
        // The zoom in quarter steps of a doubling, rounded down (fewer px per unit: a box
        // spanning more price, so the check only gets stricter within a step).
        let step = if ppu > 0.0 {
            (ppu.log2() * 4.0).floor() as i32
        } else {
            i32::MIN
        };
        // The UI zoom too: a label's box (its reach over zones) is its size.
        let key = (
            theme.name,
            Arc::as_ptr(map) as usize,
            self.rev,
            step,
            crate::zoom::generation(),
        );
        if self.label_colors_key == Some(key) {
            return;
        }
        let ppu_low = if step == i32::MIN {
            0.0
        } else {
            2f64.powf(f64::from(step) / 4.0)
        };
        let scale = self.zone_scale(map, theme);
        let mut zones = std::mem::take(&mut self.label_zones);
        let mut moving = std::mem::take(&mut self.label_moving);
        let mut unders = std::mem::take(&mut self.label_unders);
        zones.clear();
        moving.clear();
        for l in &map.levels {
            if !matches!(l.shape, LevelShape::Zone { .. }) {
                continue;
            }
            let mut z = self.zone_fill(theme, l, 1.0);
            z.a *= scale;
            match (l.is_vwap_relative(), l.span_at(None)) {
                (false, Some(span)) => zones.push((span, z)),
                (true, _) => moving.push(z),
                _ => {}
            }
        }
        // A label box and a row either way (a stacked label), in price.
        let h = f64::from(label_px() * 1.6);
        let reach = if ppu_low > 0.0 {
            (h * 1.5) / ppu_low
        } else {
            f64::INFINITY
        };
        self.label_colors.clear();
        for l in &map.levels {
            unders.clear();
            for (_, f) in &zones {
                unders.push(theme.zones_over_chart([*f]));
            }
            for f in &moving {
                unders.push(theme.zones_over_chart([*f]));
            }
            if let Some((a, b)) = self.label_span(l, reach, h / ppu_low.max(1e-12)) {
                // Every stack across [a, b]: at a, and just past each zone edge inside it.
                let at = |p: f64, unders: &mut Vec<Paint>| {
                    let stack = || {
                        zones
                            .iter()
                            .filter(move |((lo, hi), _)| *lo <= p && p <= *hi)
                            .map(|(_, f)| *f)
                    };
                    unders.push(theme.zones_over_chart(stack()));
                    for m in &moving {
                        unders.push(theme.zones_over_chart(stack().chain([*m])));
                    }
                };
                at(a, &mut unders);
                at(b, &mut unders);
                for ((lo, hi), _) in &zones {
                    for e in [*lo, *hi] {
                        if a <= e && e <= b {
                            at(e, &mut unders);
                        }
                    }
                }
            }
            let c = self.label_color(theme, l, hair, &unders);
            self.label_colors.push(c);
        }
        self.label_zones = zones;
        self.label_moving = moving;
        self.label_unders = unders;
        self.label_colors_key = Some(key);
    }

    /// The prices level `l`'s label box can cover: its anchor's box (`box_units`: a label's
    /// height in price) widened by `reach` toward where a stacked label can go (a line's
    /// either way, a low edge's further down, a high edge's further up); everything when the
    /// zoom isn't known. `None`: it moves with the VWAP (its fill is checked alone).
    fn label_span(&self, l: &Level, reach: f64, box_units: f64) -> Option<(f64, f64)> {
        let (lo, hi) = l.span_at(None)?;
        if !reach.is_finite() {
            return Some((f64::NEG_INFINITY, f64::INFINITY));
        }
        let at = self.type_look(l).map_or(LabelAt::Own, |t| t.label.at);
        Some(match at {
            LabelAt::Own => (hi - reach, hi + reach),
            LabelAt::Low => (lo - box_units - reach, lo),
            LabelAt::High => (hi, hi + box_units + reach),
        })
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
    /// The source's id (the row's study: `StudyRef::Levels`).
    pub id: String,
    /// Hidden on this chart: listed, dimmed, nothing else said.
    pub hidden: bool,
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
    /// Each label's room this frame: the plot's (on the chart), a tag's or the column's, or
    /// none (not even a short cut fits: no labels at all).
    room: LabelRoom,
    /// A span no label may cover this frame (centre y, height): the last-price tag, while
    /// labels are tags in the price axis ([`Self::set_blocked`]).
    blocked: Option<(f32, f32)>,
    /// Label text widths as measured, by text (`measure_labels`, at render): kept across
    /// maps, so an identical batch keeps an identical axis and column fit.
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
    /// The last-price line's y this frame (plot px; `None`: no last price), kept clear of
    /// level lines and zone edges ([`Self::set_last_y`]).
    last_y: Option<f32>,
    /// The label chips painted this frame (plot px: left, right, top, bottom), so the
    /// last-price line breaks around them ([`Self::label_chips`]); capacity kept, so steady
    /// frames don't allocate.
    chips: Vec<[f32; 4]>,
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
            shaped: ColorCache::new(),
            price: None,
            room: LabelRoom::Plot,
            blocked: None,
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
            last_y: None,
            chips: Vec::new(),
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
                    label_zones: Vec::new(),
                    label_moving: Vec::new(),
                    label_unders: Vec::new(),
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
            let on_edge = self.config.label_place == LabelPlace::Axis;
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
            // Each label at its type's edge (kept through collisions: one that clashes stacks
            // a row further from its zone, or is dropped; never moved to the other edge).
            // A tag in the price axis marks its edge's price itself.
            for p in &mut s.placed {
                p.anchor = s
                    .kind_looks
                    .get(usize::from(map.levels[p.index as usize].kind))
                    .copied()
                    .flatten()
                    .map_or(LabelAt::Own, |t| t.label.at);
                p.on_edge = on_edge;
            }
            // A label that can't be drawn in its room (its full text measured, else its cut for
            // this room: an empty cut means none fits) claims no room. No room at all (labels
            // can't show): none placed.
            let room = self.room.at(plot_w);
            let (texts, widths, cuts) = (&s.texts, &self.widths, &self.cuts);
            let blocked = self.blocked.filter(|_| on_edge);
            place_labels(
                &mut s.placed,
                label_px() * 1.6,
                plot_h,
                blocked,
                &mut s.seen,
                |i| {
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
                },
            );
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

    /// What the labels need, from the whole level set (every labeled level of every source
    /// shown with its labels on, kinds and types shown; never only those in view, so the
    /// price axis and a fitted column change only with the levels, the label settings or the
    /// zoom, never on a pan): (the widest label's text, the widest shortest cut's text), with
    /// no chip. (0, 0) when no label would show. Measured at render (`measure_labels`), else
    /// estimated. No allocation.
    pub fn label_need(&self) -> (f32, f32) {
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
                        Some((_, p)) => (p.chars().count() + 5) as f32 * label_px() * 0.62,
                        None => 2.0 * label_px() * 0.62,
                    }
                });
                cut = cut.max(c);
            }
        }
        if widest <= 0.0 {
            return (0.0, 0.0);
        }
        (widest, cut)
    }

    /// Each label's text room this frame (`Some`: a tag's in the axis or the column's; `None`:
    /// not even a short cut fits, so no labels at all).
    pub fn set_room(&mut self, room: Option<f32>) {
        self.room = room.map_or(LabelRoom::None, LabelRoom::Room);
    }

    /// Labels on the chart: as wide as the plot allows (its own edge's room).
    pub fn set_room_plot(&mut self) {
        self.room = LabelRoom::Plot;
    }

    /// The span no tag in the price axis may cover this frame: the last-price tag (centre y
    /// and height, plot px), before [`Self::place`].
    pub fn set_blocked(&mut self, span: Option<(f32, f32)>) {
        self.blocked = span;
    }

    /// Each shown label's centre y this frame (after [`Self::place`]): the price axis keeps
    /// its own prices clear of the tags. No allocation.
    pub fn tag_rows(&self, mut f: impl FnMut(f32)) {
        if self.config.hide_labels {
            return;
        }
        for s in &self.slots {
            if s.map.is_none() || !self.config.labels_shown(s.handle.source()) {
                continue;
            }
            for p in &s.placed {
                if let Some(y) = p.label_y {
                    f(y);
                }
            }
        }
    }

    /// Whether any level source serves this chart (a label column only for a chart that has
    /// labels to put in it).
    pub fn has_sources(&self) -> bool {
        !self.slots.is_empty()
    }

    /// A label's height (a tag's, a row of the column's), plot px.
    pub fn label_h() -> f32 {
        label_px() * 1.6
    }

    /// Measure the label set of every source shown with its labels on (in render, before
    /// [`Self::label_need`]): each label text's width and its shortest cut's width once, kept
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
            // As placing and the need do: a slot with no levels yet (its first map's levels
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

    /// The label chips painted this frame ([`Self::paint_labels`]), for the last-price line
    /// to break around ([`around_chips`]).
    pub fn label_chips(&self) -> &[[f32; 4]] {
        &self.chips
    }

    /// No label chips this frame (labels not painted).
    pub fn clear_chips(&mut self) {
        self.chips.clear();
    }

    /// The last-price line's y this frame (plot px), before [`Self::paint_front`]: level
    /// lines and zone edges leave its row clear, so the last price always draws above them.
    pub fn set_last_y(&mut self, y: Option<f32>) {
        self.last_y = y;
    }

    /// Lines and zone edges (above the candles), leaving the last-price line's row clear.
    pub fn paint_front(
        &self,
        plot: Bounds<Pixels>,
        theme: &Theme,
        fp: &crate::footprint::FootprintLayer,
        window: &mut Window,
    ) {
        let scale = window.scale_factor();
        let hair = 1.0 / scale;
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        let pw = f32::from(plot.size.width);
        // The last-price line's row (drawn later, translucent): left clear of level lines
        // and zone edges where that line is drawn (it breaks at footprint cells, as
        // `hline_segments` says), so the last price always reads above them.
        let last = self.last_y;
        let row = |window: &mut Window, a: f32, b: f32, y: f32, w: f32, c: Hsla| {
            let mut quad = |x0: f32, x1: f32, top: f32, h: f32| {
                window.paint_quad(fill(
                    Bounds::new(point(px(ox + x0), px(oy + top)), size(px(x1 - x0), px(h))),
                    c,
                ));
            };
            stroke_around_last(a, b, y, w, last, hair, fp, &mut quad);
        };
        // A stroke at 0 % (a fill-only zone's edges) draws nothing at all.
        let line = |window: &mut Window, y: f32, st: Stroke| {
            if st.color.a <= 0.0 {
                return;
            }
            let w = st.width.max(hair);
            let y = snap(f64::from(y - w * 0.5), scale);
            crate::eyedropper::rect(
                Bounds::new(point(px(ox), px(oy + y)), size(px(pw), px(w))),
                st.color,
                "Level",
            );
            // Across the bars to the plot's edge; in the labels' gutter the chip covers it.
            st.dash
                .segments(pw, |a, b| row(window, a, b, y, w, st.color));
        };
        let ppu = self.px_per_unit;
        let cols = &self.vwap_cols;
        // A VWAP-relative edge: a segment across each bar at VWAP + offset, joined to the
        // previous bar of the same session; dashed draws every other bar.
        let along = |window: &mut Window, offset: f64, st: Stroke| {
            if st.color.a <= 0.0 {
                return;
            }
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
                    row(window, col.left, col.right, y - w * 0.5, w, c);
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

    /// Labels, where the chart puts them ([`LabelPlace`]; `strip` says where that is this
    /// frame): on the legend box, in the level's color where that is AA there, else theme
    /// text (`Slot::label_colors`); a stale map's in dim text. Tags in the price axis are
    /// outlined in that color and lead back to their price when stacked away from it.
    pub fn paint_labels(
        &mut self,
        plot: Bounds<Pixels>,
        strip: LabelStrip,
        theme: &Theme,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.chips.clear();
        if self.config.hide_labels {
            return;
        }
        let place = self.config.label_place;
        // On the chart, the set decides together: beside the newest bar only if its widest
        // label fits there, else all pinned to the plot's edge (never split in two columns).
        let beside = strip.newest_right.and_then(|r| {
            chart_label_x(
                r,
                self.label_need().0 + label_chrome(),
                f32::from(plot.size.width),
            )
        });
        let scale = window.scale_factor();
        let (ox, oy) = (f32::from(plot.origin.x), f32::from(plot.origin.y));
        let (pw, ph) = (f32::from(plot.size.width), f32::from(plot.size.height));
        let box_h = label_px() * 1.6;
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
            self.slots[si].label_colors(&map, theme, hair, self.px_per_unit);
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
                    theme.stale_label_text()
                } else {
                    self.slots[si]
                        .label_colors
                        .get(i)
                        .copied()
                        .unwrap_or(theme.text_main)
                };
                let w = f32::from(shaped_in(&mut self.shaped, text, color, window).width)
                    + label_chrome();
                let top = snap(f64::from(y - box_h * 0.5), scale);
                let (x, mut bg, mut ink) = match place {
                    // Just right of the newest bar, moving with it; pinned to the plot's edge
                    // when that doesn't fit (the newest bar off screen, or too near it).
                    LabelPlace::Chart => {
                        let x = beside.unwrap_or((pw - w - gutter_right()).max(0.0));
                        (snap(f64::from(x), scale), theme.label_bg, color)
                    }
                    LabelPlace::Column => (
                        snap(f64::from(strip.left + column_pad()), scale),
                        theme.label_bg,
                        color,
                    ),
                    LabelPlace::Axis => (
                        snap(f64::from(strip.left + axis_leader()), scale),
                        theme.label_bg,
                        color,
                    ),
                };
                match place {
                    LabelPlace::Chart => {
                        // Under the pointer: faded, so the bars under it show (and the last
                        // price runs through it again).
                        let hovered = strip.hover.is_some_and(|(hx, hy)| {
                            hx >= x && hx <= x + w && hy >= top && hy <= top + box_h
                        });
                        if hovered {
                            bg = bg.opacity(bg.a * HOVER_FADE);
                            ink = ink.opacity(ink.a * HOVER_FADE);
                        } else {
                            self.chips.push([x, x + w, top, top + box_h]);
                        }
                        window.paint_quad(fill(
                            Bounds::new(point(px(ox + x), px(oy + top)), size(px(w), px(box_h))),
                            bg,
                        ));
                    }
                    LabelPlace::Column => {
                        window.paint_quad(fill(
                            Bounds::new(point(px(ox + x), px(oy + top)), size(px(w), px(box_h))),
                            bg,
                        ));
                    }
                    LabelPlace::Axis => {
                        // Stacked off its price: a short leader from the price to the tag (only
                        // for a price on the plot: a tag held at an edge for a level off it has
                        // nothing to lead to).
                        let at = p.label_anchor(box_h * 0.5);
                        if (0.0..=ph).contains(&at) && (at - y).abs() > 0.5 {
                            let lead = |window: &mut Window, x0: f32, y0: f32, w: f32, h: f32| {
                                window.paint_quad(fill(
                                    Bounds::new(
                                        point(px(ox + x0), px(oy + y0)),
                                        size(px(w), px(h)),
                                    ),
                                    ink,
                                ));
                            };
                            let stub = crate::zoom::f(2.0);
                            let (lx, a, b) = (strip.left + stub, at.min(y), at.max(y));
                            lead(window, strip.left, at - hair * 0.5, stub + hair, hair);
                            lead(window, lx, a, hair, b - a);
                            lead(window, lx, y - hair * 0.5, x - lx, hair);
                        }
                        window.paint_quad(gpui::quad(
                            Bounds::new(point(px(ox + x), px(oy + top)), size(px(w), px(box_h))),
                            px(crate::zoom::f(2.0)),
                            bg,
                            px(1.0),
                            ink,
                            gpui::BorderStyle::Solid,
                        ));
                    }
                }
                let line = shaped_in(&mut self.shaped, text, ink, window);
                let _ = line.paint(
                    point(
                        px(ox + x + label_pad()),
                        px(oy + top + box_h * 0.5 - label_px() * 0.65),
                    ),
                    px(label_px() * 1.3),
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
        let ppu = self.px_per_unit;
        let s = &mut self.slots[i];
        let Some(map) = s.map.clone() else {
            return Vec::new();
        };
        s.label_colors(&map, theme, 1.0, ppu);
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
        // Levels turned off as a whole: none listed (the switch is the chart's, not a row's).
        if self.config.hidden {
            return Vec::new();
        }
        self.slots
            .iter()
            .map(|s| {
                let id = s.handle.source().to_owned();
                // Hidden here: listed, dimmed, so it is one click from coming back.
                if !self.config.source_shown(&id) {
                    return LegendRow {
                        id,
                        hidden: true,
                        label: s.label,
                        title: None,
                        text: "· hidden".into(),
                        warn: false,
                        dim: true,
                        stats: Vec::new(),
                    };
                }
                // A map's time is never shown in the legend (it says nothing a trader acts on
                // while current); a stale map says "· stale" (after its title), dimmed with its levels.
                let (text, warn, dim) = match s.status {
                    LevelStatus::Missing => (
                        match &s.error {
                            Some(e) => format!("missing · {}", without_name(s.label, e)),
                            None => "missing".into(),
                        },
                        true,
                        false,
                    ),
                    LevelStatus::Stale { .. } => ("· stale".to_owned(), false, true),
                    LevelStatus::Current => (String::new(), false, false),
                };
                // The source's own legend content, from a map it has drawn (not while missing).
                let legend = s
                    .map
                    .as_ref()
                    .filter(|_| s.status != LevelStatus::Missing)
                    .and_then(|m| m.legend.as_ref());
                LegendRow {
                    id,
                    hidden: false,
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

/// A stroke across `[a, b)` from `y`, `w` px tall, less the last-price line's row (`last`,
/// `hair` tall) only where that line is drawn: `fp.hline_segments` (it breaks at footprint
/// cells, and a level stroke there stays whole). `quad(x0, x1, top, h)` paints. No allocation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stroke_around_last(
    a: f32,
    b: f32,
    y: f32,
    w: f32,
    last: Option<f32>,
    hair: f32,
    fp: &crate::footprint::FootprintLayer,
    quad: &mut impl FnMut(f32, f32, f32, f32),
) {
    let Some(ly) = last.filter(|&ly| ly < y + w && y < ly + hair) else {
        quad(a, b, y, w);
        return;
    };
    let mut cur = a;
    fp.hline_segments(ly, a, b, |s, e| {
        if s > cur {
            quad(cur, s, y, w);
        }
        for (top, h) in around_last(y, w, Some(ly), hair) {
            quad(s, e, top, h);
        }
        cur = e;
    });
    if cur < b {
        quad(cur, b, y, w);
    }
}

/// The label chips (`[left, right, top, bottom]`, plot px) the last-price line at `y`
/// (`hair` tall) crosses, as gaps cut from `[x0, x1)`: `f(a, b)` for each part left drawn.
/// Chips are right-aligned in the gutter, so the parts are found in one pass from the right
/// edge's chips leftwards without sorting (no allocation).
pub(crate) fn around_chips(
    chips: &[[f32; 4]],
    y: f32,
    hair: f32,
    x0: f32,
    x1: f32,
    mut f: impl FnMut(f32, f32),
) {
    let mut cur = x0;
    loop {
        // The leftmost chip on this row whose right edge is past `cur`.
        let next = chips
            .iter()
            .filter(|c| c[2] < y + hair && y < c[3] && c[1] > cur && c[0] < x1)
            .min_by(|p, q| p[0].total_cmp(&q[0]));
        let Some(c) = next else {
            break;
        };
        if c[0] > cur {
            f(cur, c[0]);
        }
        cur = cur.max(c[1]);
        if cur >= x1 {
            return;
        }
    }
    if cur < x1 {
        f(cur, x1);
    }
}

/// A horizontal stroke from `y` `w` px tall, less the last-price line's row (`last`, `hair`
/// tall: it is drawn later, translucent, and reads above every level line and zone edge):
/// the parts above and below that row. No allocation.
pub(crate) fn around_last(
    y: f32,
    w: f32,
    last: Option<f32>,
    hair: f32,
) -> impl Iterator<Item = (f32, f32)> {
    let parts = match last {
        Some(ly) if ly < y + w && y < ly + hair => [
            (ly > y).then_some((y, ly - y)),
            (ly + hair < y + w).then_some((ly + hair, y + w - ly - hair)),
        ],
        _ => [Some((y, w)), None],
    };
    parts.into_iter().flatten()
}

/// Where labels on the chart go when the newest bar's right edge is at `newest_right`: just
/// right of it, if the widest of them (`widest`, with its chip) fits there in a plot `plot_w`
/// wide; `None`: pinned to the plot's right edge instead.
pub fn chart_label_x(newest_right: f32, widest: f32, plot_w: f32) -> Option<f32> {
    let x = newest_right + newest_gap();
    (x + widest <= plot_w - gutter_right()).then_some(x)
}

/// The label column's width this frame, px: the saved width at the UI zoom (`saved`), never
/// more than 40 % of the chart beside the price axis (`beside`), in whole px (rounded up, so a
/// column fitted to a label holds it whole at any zoom).
pub fn column_px(saved: f32, beside: f32) -> f32 {
    saved.min(column_cap(beside)).ceil()
}

/// The widest the label column is drawn beside the price axis (`beside` px of chart): 40 %.
pub fn column_cap(beside: f32) -> f32 {
    (beside * 0.4).max(0.0)
}

/// The label column's width (px at 100 % zoom) after its edge was dragged from window x
/// `from` to `x`, starting `w0` wide at a UI zoom of `zoom`: the edge is the column's left
/// side, so dragged left it widens; the edge follows the pointer.
pub fn dragged_column(w0: f32, from: f32, x: f32, zoom: f32) -> f32 {
    w0 + (from - x) / zoom.max(0.01)
}

/// The price axis's width for level tags: the widest label's text `need` (0: none) with its
/// tag's frame, no narrower than the axis's own `base` nor wider than `cap`, and each tag's
/// text room in it (`None`: not even the widest shortest cut `cut` fits, so no tags).
pub fn axis_fit(need: f32, cut: f32, base: f32, cap: f32) -> (f32, Option<f32>) {
    if need <= 0.0 {
        return (base, None);
    }
    let frame = axis_leader() + label_chrome() + axis_edge();
    let axis = (need + frame).clamp(base, cap.max(base)).ceil();
    let room = axis - frame;
    // Not even the shortest cut fits: no tags, and the axis keeps its own width.
    if cut > room {
        return (base, None);
    }
    (axis, Some(room))
}

/// The room for a label's text on the chart (`plot_w` px wide): the plot's width less its
/// chip and margins, in 8 px steps (a pane resized by a pixel doesn't cut its labels again).
pub(crate) fn plot_room(plot_w: f32) -> f32 {
    let step = crate::zoom::f(8.0);
    let r = plot_w - label_chrome() - gutter_pad();
    (r / step).floor() * step
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
    /// On the chart: as wide as the plot allows.
    Plot,
    /// A tag's room in the price axis, or the label column's, for a label's text.
    Room(f32),
    /// Not even a short cut fits: no labels at all.
    None,
}

impl LabelRoom {
    fn at(self, plot_w: f32) -> Option<f32> {
        match self {
            LabelRoom::Plot => Some(plot_room(plot_w)),
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
    t.chars().count() as f32 * label_px() * 0.62
}

/// `text`'s width as a label, shaped and not kept (a cut's candidates).
pub(crate) fn measure(text: &str, theme: &Theme, window: &mut Window) -> f32 {
    measure_at(text, label_px(), theme, window)
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
/// and "· " before the title) and before its state (`status`: "stale" or "missing · …" and
/// the gap before it, 0 while current; kept whole, so a long title never hides "stale") cut
/// with "…" at a whole character, and how many of its stats
/// (`stat_w`: each stat's width, name and value with their gap; `gap` between stats) fit
/// whole, in order, with room for a closing "…" when some don't. A number is never cut.
#[derive(Clone, Debug, PartialEq)]
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
    status: f32,
    title: Option<&str>,
    stat_w: &[f32],
    gap: f32,
    mut measure: impl FnMut(&str) -> f32,
) -> LegendFit {
    let title = title.and_then(|t| {
        let left = room - head - status;
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

/// Labels shaped and kept, by (text, color): every color a text is drawn in (its level's,
/// the stale dim, another level's of the same name) is kept, so nothing is shaped again frame
/// after frame however many colors share a text. At most [`MAX_SHAPED`] entries in all; past
/// that the cache starts over (a new map's labels).
pub(crate) struct ColorCache<V> {
    by_text: HashMap<String, Vec<(Hsla, V)>>,
    len: usize,
}

impl<V> ColorCache<V> {
    pub(crate) fn new() -> Self {
        ColorCache {
            by_text: HashMap::new(),
            len: 0,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.by_text.clear();
        self.len = 0;
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Any entry for `text` (its width is the same in every color).
    pub(crate) fn any(&self, text: &str) -> Option<&V> {
        self.by_text
            .get(text)
            .and_then(|v| v.first())
            .map(|(_, x)| x)
    }

    /// `text` in `color`, made by `make` on a miss (allocates then), kept.
    pub(crate) fn get_or_make(&mut self, text: &str, color: Hsla, make: impl FnOnce() -> V) -> &V {
        let have = self
            .by_text
            .get(text)
            .and_then(|v| v.iter().position(|(c, _)| *c == color));
        let at = match have {
            Some(i) => i,
            None => {
                if self.len >= MAX_SHAPED {
                    self.clear();
                }
                let v = self.by_text.entry(text.to_owned()).or_default();
                v.push((color, make()));
                self.len += 1;
                v.len() - 1
            }
        };
        &self.by_text[text][at].1
    }
}

/// Labels shaped and kept (see [`ColorCache`]).
type Shaped = ColorCache<ShapedLine>;

/// `text` shaped as a label in `color`, once per distinct text and color and kept (a cache
/// miss allocates, as the map's labels are new).
fn shaped_in<'a>(
    shaped: &'a mut Shaped,
    text: &str,
    color: Hsla,
    window: &mut Window,
) -> &'a ShapedLine {
    shaped.get_or_make(text, color, || {
        let run = TextRun {
            len: text.len(),
            font: crate::fonts::ui(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window.text_system().shape_line(
            SharedString::from(text.to_owned()),
            px(label_px()),
            &[run],
            None,
        )
    })
}

/// `text`'s width as a label (shaped once per distinct text and kept; a cache miss
/// allocates, as the map's labels are new).
fn shaped_width(shaped: &mut Shaped, text: &str, theme: &Theme, window: &mut Window) -> f32 {
    if let Some(l) = shaped.any(text) {
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
