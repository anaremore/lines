//! External price levels (decision 0022): the vendor-neutral **level map** a level source
//! publishes, its validation, allocation-free placement for the chart layer, and the
//! **price-at-time** helper adapters use for anchor translation.
//!
//! A map holds horizontal **lines** and **zones** (lower–upper bands), each with a *kind* (a
//! short slug the source defines, e.g. `support`; panes can hide kinds), an optional label, a
//! source-defined style, and metadata. Values are either
//! - [`LevelValue::Tick`]: an exact price on the instrument's tick grid (decision 0002), when
//!   the source says its values are tradable prices (validated: off-grid is an error), or
//! - [`LevelValue::Raw`]: the source's own number in catalog price units, kept as given and
//!   rounded only for drawing (e.g. a translated level that isn't a price).
//!
//! Maps are built and validated off the UI thread ([`LevelMapBuilder::build`]); placement
//! for paint ([`place`], [`place_labels`]) writes into caller-owned buffers and never
//! allocates.

use std::fmt;

use crate::bars::Bar;
use crate::instrument::{ContractKey, ProductId};
use crate::price::{Price, PriceFormat};
use crate::style::{Color, FillStyle, LineStyle, LineType};
use crate::time::UtcMillis;
use crate::viewport::Viewport;

/// Most levels in one map (a source sending more is broken or hostile).
pub const MAX_LEVELS: usize = 1024;
/// Most kinds in one map.
pub const MAX_KINDS: usize = 64;
const MAX_SLUG: usize = 32;
const MAX_LABEL: usize = 64;
const MAX_META: usize = 256;

/// A level's value.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum LevelValue {
    /// An exact price on the tick grid.
    Tick(Price),
    /// The source's value in catalog price units, not necessarily on the grid.
    Raw(f64),
    /// The chart's own **ETH-session VWAP** plus this offset (catalog price units; negative
    /// below). Moves with the VWAP: drawn bar by bar along it, like σ bands (e.g. a band of
    /// VWAP ± k × a source statistic). Withheld while the chart has no VWAP value.
    VwapOffset(f64),
}

impl LevelValue {
    /// The value in catalog price units given the chart's current ETH VWAP (`None` for a
    /// VWAP-relative value when there is no VWAP).
    pub fn resolve(&self, vwap: Option<f64>) -> Option<f64> {
        match *self {
            LevelValue::Tick(p) => Some(p.0 as f64),
            LevelValue::Raw(v) => Some(v),
            LevelValue::VwapOffset(d) => vwap.map(|v| v + d),
        }
    }

    pub fn is_vwap_relative(&self) -> bool {
        matches!(self, LevelValue::VwapOffset(_))
    }

    /// The number the value carries: the price, or the offset for a VWAP-relative value.
    fn raw(&self) -> f64 {
        match *self {
            LevelValue::Tick(p) => p.0 as f64,
            LevelValue::Raw(v) | LevelValue::VwapOffset(v) => v,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum LevelShape {
    Line(LevelValue),
    /// A band from `lower` to `upper` (inclusive), `lower <= upper`.
    Zone {
        lower: LevelValue,
        upper: LevelValue,
    },
}

/// How the source wants a level drawn (decision 0022: source colours are allowed for
/// external levels; labels still use theme text tokens).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct LevelStyle {
    /// `0xRRGGBBAA`. Zones are filled with this colour at a low opacity and edged with it.
    pub rgba: u32,
    /// Line weight in device pixels, 1..=4.
    pub weight: u8,
    pub dashed: bool,
}

impl Default for LevelStyle {
    fn default() -> Self {
        LevelStyle {
            rgba: 0x9a9f_b2ff,
            weight: 1,
            dashed: false,
        }
    }
}

/// Optional facts about a level or a whole map, shown on request, never used for drawing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelMeta {
    /// When the source computed it.
    pub as_of: Option<UtcMillis>,
    /// The source's session name for it (e.g. the trading day it belongs to).
    pub session: Option<String>,
    /// Where it comes from (e.g. the source's method or upstream), free text.
    pub provenance: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Level {
    /// Index into [`LevelMap::kinds`].
    pub kind: u16,
    pub shape: LevelShape,
    pub label: Option<String>,
    pub style: LevelStyle,
    pub meta: LevelMeta,
}

impl Level {
    /// `(low, high)` in catalog price units at the chart's current ETH VWAP (`None`: a
    /// VWAP-relative level while there is no VWAP).
    pub fn span_at(&self, vwap: Option<f64>) -> Option<(f64, f64)> {
        match self.shape {
            LevelShape::Line(v) => v.resolve(vwap).map(|x| (x, x)),
            LevelShape::Zone { lower, upper } => Some((lower.resolve(vwap)?, upper.resolve(vwap)?)),
        }
    }

    /// Moves with the chart's VWAP (drawn along it, bar by bar).
    pub fn is_vwap_relative(&self) -> bool {
        match self.shape {
            LevelShape::Line(v) => v.is_vwap_relative(),
            LevelShape::Zone { lower, .. } => lower.is_vwap_relative(),
        }
    }

    /// `(low, high)` offsets from the VWAP of a VWAP-relative level.
    pub fn vwap_offsets(&self) -> Option<(f64, f64)> {
        match self.shape {
            LevelShape::Line(LevelValue::VwapOffset(d)) => Some((d, d)),
            LevelShape::Zone {
                lower: LevelValue::VwapOffset(a),
                upper: LevelValue::VwapOffset(b),
            } => Some((a, b)),
            _ => None,
        }
    }
}

/// A source's named number that isn't a price (an indicator reading, a statistic), published
/// with its map for strategies (decision 0025). Never drawn, and never turned into a `Price`
/// (order prices come only from levels, explicitly rounded: decision 0002). Unbounded.
#[derive(Clone, Debug, PartialEq)]
pub struct NamedValue {
    /// Stable name: 1-32 lowercase letters, digits, `-`, `_` or `.`, unique in the map.
    pub name: String,
    pub value: f64,
    /// When the source computed it, if it says ([`LevelMap::value_as_of`] falls back to the
    /// map's as-of).
    pub as_of: Option<UtcMillis>,
}

/// Named values a map may carry.
pub const MAX_VALUES: usize = 64;

/// A line series a source publishes for a chart's lower pane (decision 0022: an external
/// source may provide lower-pane line series beside its levels). Values are numbers, never
/// prices. A value holds from its point until the next point or the end of its segment, and no
/// further; outside every segment nothing is known: a gap, never drawn or interpolated.
#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    /// Stable key (a [`NamedValue`] name's rules), unique in the map: the line a study draws.
    pub key: String,
    /// When each value was set, ascending.
    pub points: Vec<(UtcMillis, f64)>,
    /// The spans the value was reported, ascending and disjoint (`from <= to`).
    pub segments: Vec<(UtcMillis, UtcMillis)>,
}

/// Series a map may carry, and points per series.
pub const MAX_SERIES: usize = 16;
pub const MAX_SERIES_POINTS: usize = 100_000;

impl Series {
    /// The value in force at `t`: inside a segment, the last point at or before `t` within
    /// that segment; `None` in a gap or before the segment's first point. Allocation-free.
    pub fn value_at(&self, t: UtcMillis) -> Option<f64> {
        let s = self.segment_at(t)?;
        let i = self.points.partition_point(|p| p.0 <= t).checked_sub(1)?;
        let (pt, v) = self.points[i];
        (pt >= s.0).then_some(v)
    }

    /// The segment containing `t`.
    pub fn segment_at(&self, t: UtcMillis) -> Option<(UtcMillis, UtcMillis)> {
        let i = self.segments.partition_point(|s| s.0 <= t).checked_sub(1)?;
        let s = self.segments[i];
        (t <= s.1).then_some(s)
    }

    fn check(&self) -> Result<(), LevelError> {
        let bad = || LevelError::BadSeries(self.key.clone());
        if !value_name_ok(&self.key) || self.points.len() > MAX_SERIES_POINTS {
            return Err(bad());
        }
        let sorted = self.points.windows(2).all(|w| w[0].0 <= w[1].0)
            && self.points.iter().all(|p| p.1.is_finite())
            && self.segments.iter().all(|s| s.0 <= s.1)
            && self.segments.windows(2).all(|w| w[0].1 < w[1].0);
        if sorted { Ok(()) } else { Err(bad()) }
    }
}

/// One source's levels for one product (and contract, if the source is contract specific).
#[derive(Clone, Debug, PartialEq)]
pub struct LevelMap {
    /// The source's id (stable; persisted per pane and stored in recordings).
    pub source: String,
    pub product: ProductId,
    pub contract: Option<ContractKey>,
    /// When the source computed the map (its own clock).
    pub as_of: UtcMillis,
    /// Kind slugs, in first-use order; levels refer to them by index.
    pub kinds: Vec<String>,
    pub levels: Vec<Level>,
    pub meta: LevelMeta,
    /// Named values (not prices): see [`NamedValue`].
    pub values: Vec<NamedValue>,
    /// Line series for lower panes: see [`Series`]. Shared, so a source rebuilding its map
    /// each refresh keeps unchanged series without copying them.
    pub series: Vec<std::sync::Arc<Series>>,
    /// The source couldn't take this refresh's levels (the reason; e.g. its upstream says they
    /// are stale): the map carries its previous levels unchanged, shown stale with this reason,
    /// while its series are new and judged on their own freshness.
    pub levels_refused: Option<String>,
    /// Only its series are new (a source's live push between refreshes): its levels are the
    /// previous map's, so their freshness and error stay as they were.
    pub series_update: bool,
    /// What the source puts in its legend row beside its name (`None`: nothing): see
    /// [`LevelLegend`].
    pub legend: Option<LevelLegend>,
}

/// A source's legend content (generic: the source decides the words): a short title after
/// its name ("Map A") and up to [`MAX_LEGEND_STATS`] stats on a line under it, each a name
/// and its text, with a tone. The app draws a stat's tone in the theme's up or down color,
/// and the text carries the sign itself (validated: an up stat's text starts with `+`, a down
/// stat's with `-` or `{2212}`), so direction is never shown by color alone. Never the map's
/// time: the legend shows no map time while the map is current.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelLegend {
    /// After the source's name; at most [`MAX_LEGEND_TEXT`] characters.
    pub title: Option<String>,
    pub stats: Vec<LegendStat>,
}

/// One legend stat: `name text` ("Res -7.9").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegendStat {
    /// 1-12 characters.
    pub name: String,
    /// 1-12 characters, signed when the tone is up or down.
    pub text: String,
    pub tone: StatTone,
}

/// How a legend stat is colored.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum StatTone {
    /// The theme's text.
    #[default]
    Plain,
    /// The theme's up color; the text starts with `+`.
    Up,
    /// The theme's down color; the text starts with `-` or the minus sign.
    Down,
}

/// Most stats in a legend.
pub const MAX_LEGEND_STATS: usize = 8;
/// Longest legend title (characters).
pub const MAX_LEGEND_TEXT: usize = 40;
const MAX_STAT_TEXT: usize = 12;

impl LegendStat {
    fn ok(&self) -> bool {
        let len = |s: &str| (1..=MAX_STAT_TEXT).contains(&s.chars().count());
        let signed = match self.tone {
            StatTone::Plain => true,
            StatTone::Up => self.text.starts_with('+'),
            StatTone::Down => self.text.starts_with(['-', '\u{2212}']),
        };
        len(&self.name) && len(&self.text) && signed
    }
}

impl LevelLegend {
    fn ok(&self) -> bool {
        self.title
            .as_ref()
            .is_none_or(|t| t.chars().count() <= MAX_LEGEND_TEXT)
            && self.stats.len() <= MAX_LEGEND_STATS
            && self.stats.iter().all(LegendStat::ok)
    }
}

impl LevelMap {
    pub fn builder(source: &str, product: ProductId, as_of: UtcMillis) -> LevelMapBuilder {
        LevelMapBuilder {
            map: LevelMap {
                source: source.to_owned(),
                product,
                contract: None,
                as_of,
                kinds: Vec::new(),
                levels: Vec::new(),
                meta: LevelMeta::default(),
                values: Vec::new(),
                series: Vec::new(),
                levels_refused: None,
                series_update: false,
                legend: None,
            },
            error: None,
        }
    }

    /// A named value by name.
    pub fn value(&self, name: &str) -> Option<&NamedValue> {
        self.values.iter().find(|v| v.name == name)
    }

    /// When a named value was computed: its own as-of, else the map's.
    pub fn value_as_of(&self, name: &str) -> Option<UtcMillis> {
        self.value(name).map(|v| v.as_of.unwrap_or(self.as_of))
    }

    /// Index of a kind slug.
    pub fn kind_index(&self, kind: &str) -> Option<u16> {
        self.kinds.iter().position(|k| k == kind).map(|i| i as u16)
    }
}

/// Why a map was refused. The whole map is refused, never partly shown.
#[derive(Clone, Debug, PartialEq)]
pub enum LevelError {
    BadSlug(String),
    TooLong(&'static str),
    TooMany(&'static str),
    NotFinite,
    /// A `Tick` value not on the instrument's grid.
    OffTick(Price),
    /// A zone with `lower > upper`.
    Inverted,
    /// A zone with one bound relative to the VWAP and the other absolute.
    MixedAnchors,
    BadWeight(u8),
    /// A named value's name isn't 1-32 of `a-z`, `0-9`, `-`, `_` and `.`.
    BadValueName(String),
    /// Two named values share a name.
    DuplicateValue(String),
    /// A named value isn't a finite number.
    ValueNotFinite(String),
    /// A series has a bad or repeated key, too many points, a value that isn't finite, or
    /// points or segments out of order.
    BadSeries(String),
    /// The legend's title is too long, it has too many stats, or a stat is empty, too long, or
    /// up or down without its sign.
    BadLegend,
}

impl fmt::Display for LevelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LevelError::BadSlug(s) => {
                write!(f, "bad id {s:?} (use 1-32 of a-z, 0-9, '-' and '_')")
            }
            LevelError::TooLong(what) => write!(f, "{what} too long"),
            LevelError::TooMany(what) => write!(f, "too many {what}"),
            LevelError::BadSeries(k) => write!(f, "series {k:?} is malformed"),
            LevelError::BadLegend => write!(f, "the legend is malformed"),
            LevelError::NotFinite => write!(f, "a level value is not a finite number"),
            LevelError::OffTick(p) => write!(f, "level {} is not on the tick grid", p.0),
            LevelError::Inverted => write!(f, "a zone's lower bound is above its upper bound"),
            LevelError::MixedAnchors => {
                write!(f, "a zone mixes a VWAP-relative bound with an absolute one")
            }
            LevelError::BadWeight(w) => write!(f, "line weight {w} (use 1-4)"),
            LevelError::BadValueName(s) => {
                write!(
                    f,
                    "bad value name {s:?} (use 1-32 of a-z, 0-9, '-', '_' and '.')"
                )
            }
            LevelError::DuplicateValue(s) => write!(f, "two values are named {s:?}"),
            LevelError::ValueNotFinite(s) => write!(f, "value {s:?} is not a finite number"),
        }
    }
}

impl std::error::Error for LevelError {}

fn slug_ok(s: &str) -> bool {
    name_ok(s, b"-_")
}

/// Value names also allow `.` (e.g. `rsi.14`).
fn value_name_ok(s: &str) -> bool {
    name_ok(s, b"-_.")
}

fn name_ok(s: &str, extra: &[u8]) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SLUG
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || extra.contains(&b))
}

/// Builds and validates a [`LevelMap`]. The first error is kept and returned by `build`.
pub struct LevelMapBuilder {
    map: LevelMap,
    error: Option<LevelError>,
}

impl LevelMapBuilder {
    pub fn contract(mut self, c: ContractKey) -> Self {
        self.map.contract = Some(c);
        self
    }

    pub fn meta(mut self, meta: LevelMeta) -> Self {
        self.map.meta = meta;
        self
    }

    fn fail(&mut self, e: LevelError) {
        self.error.get_or_insert(e);
    }

    fn kind(&mut self, kind: &str) -> u16 {
        if !slug_ok(kind) {
            self.fail(LevelError::BadSlug(kind.to_owned()));
            return 0;
        }
        if let Some(i) = self.map.kind_index(kind) {
            return i;
        }
        if self.map.kinds.len() >= MAX_KINDS {
            self.fail(LevelError::TooMany("kinds"));
            return 0;
        }
        self.map.kinds.push(kind.to_owned());
        (self.map.kinds.len() - 1) as u16
    }

    /// Add a level. `label` is shown beside it (theme text, legend box).
    pub fn level(
        mut self,
        kind: &str,
        shape: LevelShape,
        label: Option<&str>,
        style: LevelStyle,
        meta: LevelMeta,
    ) -> Self {
        let kind = self.kind(kind);
        self.map.levels.push(Level {
            kind,
            shape,
            label: label.map(str::to_owned),
            style,
            meta,
        });
        self
    }

    pub fn line(self, kind: &str, v: LevelValue, label: Option<&str>, style: LevelStyle) -> Self {
        self.level(
            kind,
            LevelShape::Line(v),
            label,
            style,
            LevelMeta::default(),
        )
    }

    pub fn zone(
        self,
        kind: &str,
        lower: LevelValue,
        upper: LevelValue,
        label: Option<&str>,
        style: LevelStyle,
    ) -> Self {
        self.level(
            kind,
            LevelShape::Zone { lower, upper },
            label,
            style,
            LevelMeta::default(),
        )
    }

    /// Add a named value (not a price): see [`NamedValue`].
    pub fn value(mut self, name: &str, value: f64, as_of: Option<UtcMillis>) -> Self {
        self.map.values.push(NamedValue {
            name: name.to_owned(),
            value,
            as_of,
        });
        self
    }

    /// Add a line series for lower panes: see [`Series`].
    pub fn series(mut self, s: std::sync::Arc<Series>) -> Self {
        self.map.series.push(s);
        self
    }

    /// See [`LevelMap::series_update`].
    pub fn series_update(mut self) -> Self {
        self.map.series_update = true;
        self
    }

    /// The source's legend content: see [`LevelLegend`].
    pub fn legend(mut self, legend: LevelLegend) -> Self {
        self.map.legend = Some(legend);
        self
    }

    /// See [`LevelMap::levels_refused`].
    pub fn levels_refused(mut self, reason: impl Into<String>) -> Self {
        self.map.levels_refused = Some(reason.into());
        self
    }

    /// Validate against the instrument's `tick` (catalog units) and return the map.
    pub fn build(mut self, tick: i64) -> Result<LevelMap, LevelError> {
        if self.map.series.len() > MAX_SERIES {
            self.fail(LevelError::TooMany("series"));
        }
        let bad_series = self.map.series.iter().enumerate().find_map(|(i, s)| {
            s.check().err().or_else(|| {
                self.map.series[..i]
                    .iter()
                    .any(|o| o.key == s.key)
                    .then(|| LevelError::BadSeries(s.key.clone()))
            })
        });
        if let Some(e) = bad_series {
            self.fail(e);
        }
        if !slug_ok(&self.map.source) {
            self.fail(LevelError::BadSlug(self.map.source.clone()));
        }
        if self.map.legend.as_ref().is_some_and(|l| !l.ok()) {
            self.fail(LevelError::BadLegend);
        }
        if self.map.levels.len() > MAX_LEVELS {
            self.fail(LevelError::TooMany("levels"));
        }
        let tick = tick.max(1);
        let check = |v: LevelValue| match v {
            LevelValue::Tick(p) if p.0 % tick != 0 => Err(LevelError::OffTick(p)),
            LevelValue::Raw(x) | LevelValue::VwapOffset(x)
                if !x.is_finite() || x.abs() > 9.0e15 =>
            {
                Err(LevelError::NotFinite)
            }
            _ => Ok(()),
        };
        let meta_ok = |m: &LevelMeta| {
            [&m.session, &m.provenance]
                .iter()
                .all(|s| s.as_ref().is_none_or(|s| s.len() <= MAX_META))
        };
        if !meta_ok(&self.map.meta) {
            self.fail(LevelError::TooLong("metadata"));
        }
        if self.map.values.len() > MAX_VALUES {
            self.fail(LevelError::TooMany("values"));
        }
        // Too many is already the error: don't spend time (quadratic name checks) on a map
        // that is refused anyway.
        let bad = (self.map.values.len() <= MAX_VALUES)
            .then(|| {
                self.map.values.iter().enumerate().find_map(|(i, v)| {
                    if !value_name_ok(&v.name) {
                        Some(LevelError::BadValueName(v.name.clone()))
                    } else if self.map.values[..i].iter().any(|o| o.name == v.name) {
                        Some(LevelError::DuplicateValue(v.name.clone()))
                    } else if !v.value.is_finite() {
                        Some(LevelError::ValueNotFinite(v.name.clone()))
                    } else {
                        None
                    }
                })
            })
            .flatten();
        if let Some(e) = bad {
            self.fail(e);
        }
        for i in 0..self.map.levels.len() {
            let l = &self.map.levels[i];
            let r = (|| {
                match l.shape {
                    LevelShape::Line(v) => check(v)?,
                    LevelShape::Zone { lower, upper } => {
                        check(lower)?;
                        check(upper)?;
                        if lower.is_vwap_relative() != upper.is_vwap_relative() {
                            return Err(LevelError::MixedAnchors);
                        }
                        if lower.raw() > upper.raw() {
                            return Err(LevelError::Inverted);
                        }
                    }
                }
                if !(1..=4).contains(&l.style.weight) {
                    return Err(LevelError::BadWeight(l.style.weight));
                }
                if l.label
                    .as_ref()
                    .is_some_and(|s| s.chars().count() > MAX_LABEL)
                {
                    return Err(LevelError::TooLong("label"));
                }
                if !meta_ok(&l.meta) {
                    return Err(LevelError::TooLong("metadata"));
                }
                Ok(())
            })();
            if let Err(e) = r {
                self.fail(e);
            }
        }
        match self.error {
            Some(e) => Err(e),
            None => Ok(self.map),
        }
    }
}

// ---- level types (declared by a source; styled per chart) --------------------------------

/// How a type's levels look unless the chart restyles them (decision 0026). A color is a
/// [`Color`]: [`Color::Own`] is each level's own color (its [`LevelStyle`], decision 0022), a
/// theme token ([`Color::Palette`]: the theme's n-th study color; [`Color::Text`]: its main text
/// color) follows dark/light and the palette, [`Color::Custom`] is the same in every theme.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeStyle {
    /// Drawn at all.
    pub shown: bool,
    /// Lines of the type, and a zone's edges. Width in logical px; opacity scales the level's
    /// own alpha.
    pub line: LineStyle,
    /// A zone's fill: opacity scales a zone's own translucency (40 %: the default 25 % is the
    /// 10 % zones always had).
    pub fill: FillStyle,
    /// The line's recommended color on the light themes, where it differs (a fixed color that
    /// reads only on dark, given a light-theme counterpart; `None`: `line.color` everywhere).
    /// A label in the level's color takes it too.
    pub light: Option<Color>,
    /// The fill's recommended color on the light themes, where it differs (`None`:
    /// `fill.color` everywhere).
    pub fill_light: Option<Color>,
    /// Its labels' text: the level's color (the default; the theme's text where that color
    /// isn't AA on the label) or the theme's text.
    pub label: LabelColor,
    /// Where its labels sit: on its own price (a zone's high edge, as always), or just under
    /// its low edge or just above its high edge.
    pub label_at: LabelAt,
}

/// A level label's text color (a level type's option, decision 0026).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum LabelColor {
    /// The level's own line color, where it is AA (4.5:1) on the label over everything under
    /// it; else the theme's text for that label.
    #[default]
    Level,
    /// The theme's text.
    Text,
}

impl LabelColor {
    pub const ALL: [LabelColor; 2] = [LabelColor::Level, LabelColor::Text];

    /// The saved name.
    pub fn key(self) -> &'static str {
        match self {
            LabelColor::Level => "level",
            LabelColor::Text => "text",
        }
    }

    pub fn from_key(s: &str) -> Option<LabelColor> {
        LabelColor::ALL.into_iter().find(|c| c.key() == s)
    }

    /// The style editor's words.
    pub fn label(self) -> &'static str {
        match self {
            LabelColor::Level => "Level color",
            LabelColor::Text => "Text color",
        }
    }
}

/// Where a level's label sits (a level type's option, decision 0026). Labels that clash give
/// way by being dropped, never by moving to another edge ([`place_labels`]).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum LabelAt {
    /// Centered on its own price: a line's, a zone's high edge.
    #[default]
    Own,
    /// Just under its low edge (with that edge's price).
    Low,
    /// Just above its high edge.
    High,
}

impl LabelAt {
    pub const ALL: [LabelAt; 3] = [LabelAt::Low, LabelAt::High, LabelAt::Own];

    /// The saved name.
    pub fn key(self) -> &'static str {
        match self {
            LabelAt::Own => "own",
            LabelAt::Low => "low",
            LabelAt::High => "high",
        }
    }

    pub fn from_key(s: &str) -> Option<LabelAt> {
        LabelAt::ALL.into_iter().find(|c| c.key() == s)
    }

    /// The style editor's words.
    pub fn label(self) -> &'static str {
        match self {
            LabelAt::Own => "Own price",
            LabelAt::Low => "Low edge",
            LabelAt::High => "High edge",
        }
    }
}

impl TypeStyle {
    /// Each level as its source drew it before types: its own color, solid, 1 px, a zone's fill
    /// at 10 %.
    pub const OWN: TypeStyle = TypeStyle {
        shown: true,
        line: LineStyle {
            color: Color::Own,
            line: LineType::Solid,
            width: 1,
            opacity: 100,
        },
        fill: FillStyle {
            color: Color::Own,
            opacity: 25,
        },
        light: None,
        fill_light: None,
        label: LabelColor::Level,
        label_at: LabelAt::Own,
    };

    /// Whether this is [`TypeStyle::OWN`]'s line: drawn as each level's own style says (its
    /// weight in device pixels, its dashes, a zone's edges at 60 %).
    pub fn own_line(&self) -> bool {
        self.line.color == Color::Own
            && self.line.line == LineType::Solid
            && self.line.width == 1
            && self.line.opacity == 100
    }
}

/// A kind of level a source declares (`LevelSource::types`): the kind slug its levels carry
/// ([`LevelMap::kinds`]), how the style editor lists it, and its recommended look.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct LevelType {
    /// The kind slug (`hp`, `demand`).
    pub id: &'static str,
    /// The editor's name for it ("Upper band").
    pub label: &'static str,
    /// The heading it's listed under ("Zones", "Lines").
    pub group: &'static str,
    /// Its levels include zones: the editor offers a fill.
    pub zone: bool,
    pub style: TypeStyle,
    /// A kind it was split from (`None`: none): a chart that hid the parent kind hides this one
    /// too, so splitting a kind never shows what a chart had hidden.
    pub parent: Option<&'static str>,
}

impl LevelType {
    pub const fn line(
        id: &'static str,
        label: &'static str,
        group: &'static str,
        style: TypeStyle,
    ) -> LevelType {
        LevelType {
            id,
            label,
            group,
            zone: false,
            style,
            parent: None,
        }
    }

    pub const fn zone(
        id: &'static str,
        label: &'static str,
        group: &'static str,
        style: TypeStyle,
    ) -> LevelType {
        LevelType {
            id,
            label,
            group,
            zone: true,
            style,
            parent: None,
        }
    }

    /// This type, split from kind `parent` (see [`LevelType::parent`]).
    pub const fn child_of(self, parent: &'static str) -> LevelType {
        LevelType {
            parent: Some(parent),
            ..self
        }
    }
}

/// What a level's label says.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum LabelMode {
    /// Its name ("Demand zone").
    #[default]
    Name,
    /// Its name and price, `Demand zone : 7556.25`, in the product's price format. A level that
    /// moves with the VWAP keeps its name only (its price changes every bar).
    NamePrice,
}

impl LabelMode {
    /// The saved name.
    pub fn key(self) -> &'static str {
        match self {
            LabelMode::Name => "name",
            LabelMode::NamePrice => "name-price",
        }
    }

    pub fn from_key(s: &str) -> Option<LabelMode> {
        [LabelMode::Name, LabelMode::NamePrice]
            .into_iter()
            .find(|m| m.key() == s)
    }
}

/// The label drawn for level `l` in `mode`, sitting `at` (`None`: it has none). Built when the
/// map or the mode changes, never per frame: the price is the line's, or the zone edge's its
/// label sits at (the low edge's under it, else the high edge's), a raw value at the nearest
/// price unit, printed with `fmt`.
pub fn label_text(l: &Level, mode: LabelMode, at: LabelAt, fmt: &PriceFormat) -> Option<String> {
    let name = l.label.as_deref()?;
    let value = match (l.shape, at) {
        (LevelShape::Line(v), _) => v,
        (LevelShape::Zone { lower, .. }, LabelAt::Low) => lower,
        (LevelShape::Zone { upper, .. }, _) => upper,
    };
    let price = match (mode, value) {
        (LabelMode::Name, _) | (_, LevelValue::VwapOffset(_)) => None,
        (LabelMode::NamePrice, LevelValue::Tick(p)) => Some(p),
        (LabelMode::NamePrice, LevelValue::Raw(v)) => Some(Price(v.round() as i64)),
    };
    Some(match price {
        Some(p) => format!("{name} : {}", fmt.format::<32>(p).as_str()),
        None => name.to_owned(),
    })
}

// ---- placement (paint path: no allocation) -----------------------------------------------

/// A level placed in plot coordinates (logical px from the plot's top).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Placed {
    /// Index into the map's `levels`.
    pub index: u32,
    /// Top and bottom y; equal for a line.
    pub top: f32,
    pub bottom: f32,
    /// Where its label goes (centre y), after [`place_labels`]; `None`: no label shown.
    pub label_y: Option<f32>,
    /// Moves with the VWAP: `top`/`bottom` are at the current VWAP (for the label and
    /// culling); the layer draws it along the VWAP bar by bar.
    pub vwap_relative: bool,
    /// Where its label sits ([`LabelAt::Own`] from [`place`]; the layer sets its type's).
    pub anchor: LabelAt,
}

impl Placed {
    /// Its label's centre y before clamping into the plot: on its own price, just under its
    /// low edge or just above its high edge (`half`: half a label's height).
    pub fn label_anchor(&self, half: f32) -> f32 {
        match self.anchor {
            LabelAt::Own => self.top,
            LabelAt::Low => self.bottom + half,
            LabelAt::High => self.top - half,
        }
    }
}

/// Place the visible levels of `map` that intersect the plot into `out` (cleared first; at
/// most `out.capacity()`, so this never allocates). `visible(kind)` filters by kind; `vwap` is
/// the chart's current ETH VWAP (VWAP-relative levels are placed at it, or skipped without).
pub fn place(
    map: &LevelMap,
    vp: &Viewport,
    vwap: Option<f64>,
    visible: impl Fn(u16) -> bool,
    out: &mut Vec<Placed>,
) {
    out.clear();
    let (lo, hi) = (vp.price_lo.min(vp.price_hi), vp.price_lo.max(vp.price_hi));
    for (i, l) in map.levels.iter().enumerate() {
        if out.len() == out.capacity() {
            break;
        }
        if !visible(l.kind) {
            continue;
        }
        let Some((a, b)) = l.span_at(vwap) else {
            continue; // VWAP-relative, no VWAP yet: withheld
        };
        if b < lo || a > hi {
            continue; // entirely off screen
        }
        out.push(Placed {
            index: i as u32,
            top: vp.price_to_y(b) as f32,
            bottom: vp.price_to_y(a) as f32,
            label_y: None,
            vwap_relative: l.is_vwap_relative(),
            anchor: LabelAt::Own,
        });
    }
}

/// The extra separation (a fraction of a label's height) a label hidden last frame needs
/// before it shows again: labels near the clash distance keep one layout instead of
/// flickering.
pub const LABEL_HYSTERESIS: f32 = 0.25;

/// What a level's label did last frame ([`place_labels`]' hysteresis).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum LabelSeen {
    /// Not placed yet (a new map, a level that just came on screen).
    #[default]
    New,
    Shown,
    Hidden,
}

/// Choose label positions: a label is anchored to its own level where its type puts it
/// ([`Placed::label_anchor`]: on its line or a zone's high edge, just under a low edge or just
/// above a high edge), clamped into the plot, and never moved for anything else: the last
/// price and the tags on the axis paint over it. Labels only give way to each other: a label
/// overlapping one already placed is dropped (the level is still drawn), never moved to
/// another edge. Levels at
/// fixed prices are placed first, in map order, then those moving with the VWAP, so a moving
/// band never reshuffles the others. The layout depends only on the levels and the viewport,
/// never the price, and keeps last frame's decision near the threshold: `seen` (by level
/// index, updated in place) says what each label did last frame; one hidden then shows again
/// only once it is clear of every shown label by [`LABEL_HYSTERESIS`] more (a new one needs
/// only to be clear). `has_label(index)`: whether the level's label can be drawn at all (it
/// has one, and it fits the plot's width): one that can't claims no room. A plot shorter
/// than a label shows none. In place, no allocation.
pub fn place_labels(
    placed: &mut [Placed],
    label_h: f32,
    plot_h: f32,
    seen: &mut [LabelSeen],
    has_label: impl Fn(u32) -> bool,
) {
    let half = label_h * 0.5;
    for p in placed.iter_mut() {
        p.label_y = None;
    }
    if plot_h < label_h {
        return;
    }
    for moving in [false, true] {
        for i in 0..placed.len() {
            let p = placed[i];
            if p.vwap_relative != moving || !has_label(p.index) {
                continue;
            }
            let was = seen.get(p.index as usize).copied().unwrap_or_default();
            let y = p.label_anchor(half).clamp(half, plot_h - half);
            let need = if was == LabelSeen::Hidden {
                label_h * (1.0 + LABEL_HYSTERESIS)
            } else {
                label_h
            };
            let clash = placed
                .iter()
                .filter_map(|q| q.label_y)
                .any(|q| (q - y).abs() < need);
            placed[i].label_y = (!clash).then_some(y);
        }
    }
    for p in placed.iter() {
        if let Some(s) = seen.get_mut(p.index as usize)
            && has_label(p.index)
        {
            *s = if p.label_y.is_some() {
                LabelSeen::Shown
            } else {
                LabelSeen::Hidden
            };
        }
    }
}

// ---- the labels' gutter (paint path: no allocation) ------------------------------------

/// A label cut to fit `room` px (as `width` measures text): the name shortened with "…"
/// before ` : price` (the price is never cut), or the name alone cut with "…" when there is
/// no price. `None`: not even one letter of the name with its price fits. Allocates: call it
/// when the text or the room changes, not per frame.
pub fn cut_label(
    name: &str,
    price: Option<&str>,
    room: f32,
    mut width: impl FnMut(&str) -> f32,
) -> Option<String> {
    let n = name.chars().count();
    for k in (1..n).rev() {
        let head: String = name.chars().take(k).collect();
        let head = head.trim_end();
        let t = match price {
            Some(p) => format!("{head}… : {p}"),
            None => format!("{head}…"),
        };
        if width(&t) <= room {
            return Some(t);
        }
    }
    None
}

/// Plot px the level labels' gutter (`gutter` px wide, at the plot's right) takes from the
/// bars: only what the empty space already right of the newest bar (the view's right margin,
/// or a scroll past the end) doesn't cover. `vp` without any inset; `len` bars. While the
/// newest bar is scrolled off to the right, the whole gutter.
pub fn gutter_inset(gutter: f64, vp: &Viewport, len: usize) -> f64 {
    if gutter <= 0.0 {
        return 0.0;
    }
    let free = if len == 0 {
        vp.width
    } else {
        ((vp.right_index - (len - 1) as f64 - 0.5) * vp.bar_px).max(0.0)
    };
    (gutter - free).max(0.0)
}

/// One frame's gutter: the stepped width from `prev` for what the labels need (`need`, from
/// the whole level set, never the view), no wider than `cap`, and the inset it sets on `vp`
/// for `len` bars (the bars end exactly at the gutter). Returns the width.
pub fn settle_gutter(vp: &mut Viewport, len: usize, prev: f32, need: f32, cap: f32) -> f32 {
    let g = step_gutter(prev, need.min(cap)).min(cap.max(0.0));
    vp.right_inset = 0.0;
    vp.right_inset = gutter_inset(f64::from(g), vp, len);
    g
}

/// What a frame's gutter may be: its cap (in whole steps) and whether labels show at all.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GutterPlan {
    /// The widest the gutter may be, in whole [`GUTTER_STEP`]s.
    pub cap: f32,
    /// Labels show: some label would (`need` > 0) and the shortest useful cut fits the cap.
    /// Otherwise no labels and no gutter.
    pub fits: bool,
}

/// The gutter's plan for a plot `plot_w` px wide: the cap is `wide_cap` (130 px at 100 %
/// zoom); on a narrow plot a quarter of it, or what the shortest useful cut needs
/// (`min_cut`, with its chip, stepped up), but never more than 40 % of it; in whole steps.
/// Labels show only when something would (`need` > 0) and the shortest cut fits the cap.
pub fn gutter_plan(need: f32, min_cut: f32, plot_w: f32, wide_cap: f32) -> GutterPlan {
    let down = |v: f32| (v.max(0.0) / GUTTER_STEP).floor() * GUTTER_STEP;
    // The shortest cut rounded up (a cap stepped down below it would never fit it).
    let cut = (min_cut.max(0.0) / GUTTER_STEP).ceil() * GUTTER_STEP;
    let cap = down(wide_cap)
        .min(down(plot_w * 0.25).max(cut))
        .min(down(plot_w * 0.4));
    GutterPlan {
        cap,
        fits: need > 0.0 && cut <= cap,
    }
}

/// The gutter's width step (px): widths are whole steps, so a label a few px wider or
/// narrower doesn't move the bars.
pub const GUTTER_STEP: f32 = 8.0;

/// The gutter's width this frame from last frame's (`current`) and what the visible labels
/// need now (`target`, 0: no label shows): it grows at once to the next whole step, shrinks
/// only once the labels need two steps less (labels scrolling in and out at a step's edge
/// don't make it flicker), and goes as soon as no label shows.
pub fn step_gutter(current: f32, target: f32) -> f32 {
    if target <= 0.0 {
        return 0.0;
    }
    let want = (target / GUTTER_STEP).ceil() * GUTTER_STEP;
    if want > current || want <= current - 2.0 * GUTTER_STEP {
        want
    } else {
        current
    }
}

// ---- price at time --------------------------------------------------------------------

/// The chart's price at instant `t`: the **open** of the bar that starts nearest `t` within
/// `tolerance_ms` (bars are stamped with their start). `None` when no bar starts that close
/// to `t`: no history there, a gap in the data, or `t` outside the series. Adapters must
/// then withhold any translation that needs the price (decision 0022), never guess.
///
/// Ties (two bar starts equally near `t`) take the earlier bar. `bars` must be in time order
/// (as `BarSeries::bars`).
pub fn price_at(bars: &[Bar], t: UtcMillis, tolerance_ms: i64) -> Option<Price> {
    let tol = tolerance_ms.max(0);
    // First bar starting after t.
    let i = bars.partition_point(|b| b.time.0 <= t.0);
    let before = i.checked_sub(1).map(|j| &bars[j]);
    let after = bars.get(i);
    let dist = |b: &Bar| (b.time.0 - t.0).abs();
    match (before, after) {
        (Some(a), Some(b)) if dist(b) < dist(a) => Some(b),
        (Some(a), _) => Some(a),
        (None, b) => b,
    }
    .filter(|b| dist(b) <= tol)
    .map(|b| b.open)
}

#[cfg(test)]
mod tests;
