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
}

impl fmt::Display for LevelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LevelError::BadSlug(s) => {
                write!(f, "bad id {s:?} (use 1-32 of a-z, 0-9, '-' and '_')")
            }
            LevelError::TooLong(what) => write!(f, "{what} too long"),
            LevelError::TooMany(what) => write!(f, "too many {what}"),
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

    /// Validate against the instrument's `tick` (catalog units) and return the map.
    pub fn build(mut self, tick: i64) -> Result<LevelMap, LevelError> {
        if !slug_ok(&self.map.source) {
            self.fail(LevelError::BadSlug(self.map.source.clone()));
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
    };
}

/// A kind of level a source declares (`LevelSource::types`): the kind slug its levels carry
/// ([`LevelMap::kinds`]), how the style editor lists it, and its recommended look.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct LevelType {
    /// The kind slug (`hp`, `zone-bull`).
    pub id: &'static str,
    /// The editor's name for it ("Dyn HP").
    pub label: &'static str,
    /// The heading it's listed under ("Zones", "Lines").
    pub group: &'static str,
    /// Its levels include zones: the editor offers a fill.
    pub zone: bool,
    pub style: TypeStyle,
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
        }
    }
}

/// What a level's label says.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum LabelMode {
    /// Its name ("Bull Zone").
    #[default]
    Name,
    /// Its name and price, `Bull Zone : 7556.25`, in the product's price format. A level that
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

/// The label drawn for level `l` in `mode` (`None`: it has none). Built when the map or the mode
/// changes, never per frame: the price is the line's (a zone's upper edge, where its label
/// sits), a raw value at the nearest price unit, printed with `fmt`.
pub fn label_text(l: &Level, mode: LabelMode, fmt: &PriceFormat) -> Option<String> {
    let name = l.label.as_deref()?;
    let value = match l.shape {
        LevelShape::Line(v) => v,
        LevelShape::Zone { upper, .. } => upper,
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
        });
    }
}

/// Choose label positions: a line's label sits on its line, a zone's on its top edge,
/// clamped into the plot. A label that would cover the last-price row (`avoid`, ± one label
/// height) is moved just above or below it; a label overlapping one already placed is
/// dropped (the level is still drawn). In place, no allocation.
pub fn place_labels(
    placed: &mut [Placed],
    map: &LevelMap,
    label_h: f32,
    plot_h: f32,
    avoid: Option<f32>,
) {
    let half = label_h * 0.5;
    for i in 0..placed.len() {
        let p = placed[i];
        if map.levels[p.index as usize].label.is_none() {
            placed[i].label_y = None;
            continue;
        }
        let mut y = p.top.clamp(half, (plot_h - half).max(half));
        if let Some(a) = avoid
            && (y - a).abs() < label_h
        {
            y = if y <= a { a - label_h } else { a + label_h };
            if y < half || y > plot_h - half {
                y = if y < half { a + label_h } else { a - label_h };
            }
        }
        let clash = placed[..i]
            .iter()
            .filter_map(|q| q.label_y)
            .any(|q| (q - y).abs() < label_h);
        placed[i].label_y = (!clash && y >= half - 0.5 && y <= plot_h - half + 0.5).then_some(y);
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
