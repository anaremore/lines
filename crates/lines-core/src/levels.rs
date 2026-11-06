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
use crate::price::Price;
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
}

impl LevelValue {
    /// The value in catalog price units (for drawing).
    pub fn units(&self) -> f64 {
        match *self {
            LevelValue::Tick(p) => p.0 as f64,
            LevelValue::Raw(v) => v,
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
    /// `(low, high)` in catalog price units.
    pub fn span(&self) -> (f64, f64) {
        match self.shape {
            LevelShape::Line(v) => (v.units(), v.units()),
            LevelShape::Zone { lower, upper } => (lower.units(), upper.units()),
        }
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
            },
            error: None,
        }
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
    BadWeight(u8),
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
            LevelError::BadWeight(w) => write!(f, "line weight {w} (use 1-4)"),
        }
    }
}

impl std::error::Error for LevelError {}

fn slug_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SLUG
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
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
            LevelValue::Raw(x) if !x.is_finite() || x.abs() > 9.0e15 => Err(LevelError::NotFinite),
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
        for i in 0..self.map.levels.len() {
            let l = &self.map.levels[i];
            let r = (|| {
                match l.shape {
                    LevelShape::Line(v) => check(v)?,
                    LevelShape::Zone { lower, upper } => {
                        check(lower)?;
                        check(upper)?;
                        if lower.units() > upper.units() {
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
}

/// Place the visible levels of `map` that intersect the plot into `out` (cleared first; at
/// most `out.capacity()`, so this never allocates). `visible(kind)` filters by kind.
pub fn place(map: &LevelMap, vp: &Viewport, visible: impl Fn(u16) -> bool, out: &mut Vec<Placed>) {
    out.clear();
    let (lo, hi) = (vp.price_lo.min(vp.price_hi), vp.price_lo.max(vp.price_hi));
    for (i, l) in map.levels.iter().enumerate() {
        if out.len() == out.capacity() {
            break;
        }
        if !visible(l.kind) {
            continue;
        }
        let (a, b) = l.span();
        if b < lo || a > hi {
            continue; // entirely off screen
        }
        out.push(Placed {
            index: i as u32,
            top: vp.price_to_y(b) as f32,
            bottom: vp.price_to_y(a) as f32,
            label_y: None,
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
