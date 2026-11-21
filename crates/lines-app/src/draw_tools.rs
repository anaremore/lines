//! Plugin drawing tools on a chart (decision 0020): each registered
//! [`lines_feed::levels::DrawTool`] that serves the chart's product is a button in the drawing
//! toolbar and a two-anchor tool (`Tool::Plugin`) for `crate::chart_draw`, which owns the input
//! (placing both points, handles, drag, select, Delete, undo). The tool turns each drawing's two
//! points into a level map, purely; [`ToolMaps`] asks it only when a drawing's points change
//! (placed, its handle moved: live while dragging, the placement's preview), caches each
//! drawing's map, and merges one tool's drawings into one map, which the chart's levels layer
//! draws as a level source keyed by the tool's id (`LevelsLayer::set_tool_maps`): kinds, styles,
//! label layout and priority, hover-only kinds, the Studies menu's Kinds.
//!
//! Plugin drawings are never saved: the drawings file skips them
//! (`drawings_store::DrawingsFile::from_drawings`) and the workspace file holds no drawings.
//! They live in the product's in-memory drawing set for the run.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lines_core::ContractKey;
use lines_core::drawings::TimeAxis;
use lines_core::drawings::{Anchor, DrawingId, PluginTool, Tool};
use lines_core::levels::{LevelMap, MAX_GROUPS, MAX_KINDS, MAX_LEVELS, MAX_MARKERS};
use lines_core::{Price, UtcMillis};
use lines_feed::levels::{DrawPrices, DrawTool, DrawToolContext, DrawView};

/// The id a placement's preview takes (real drawings' ids start at 1).
pub const PREVIEW: DrawingId = 0;

/// A new map revision, unique in the process (a chart's tools rebuilt for a new contract never
/// repeat one the levels layer holds).
fn next_rev() -> u64 {
    static REV: AtomicU64 = AtomicU64::new(1);
    REV.fetch_add(1, Ordering::Relaxed)
}

/// One drawing's points and what its tool drew for them (`None`: the tool refused, or its map
/// didn't fit the chart's tick grid; logged, nothing drawn).
struct Cached {
    id: DrawingId,
    a: Anchor,
    b: Anchor,
    map: Option<LevelMap>,
    /// How many of `map`'s levels (from the first) the merged map took ([`merge`]: the rest
    /// didn't fit its limits, so they aren't drawn, nor hit).
    merged: usize,
}

struct Entry {
    tool: Arc<dyn DrawTool>,
    cache: Vec<Cached>,
    /// Every drawing's levels in one map (`None`: no drawings of this tool on the chart).
    merged: Option<Arc<LevelMap>>,
    rev: u64,
    /// The merged map's kinds the levels layer draws now (bit per kind index, hover-only ones
    /// included): only those are hit-tested. All until the layer says otherwise.
    drawn_kinds: u64,
    /// The chart's bars its drawings were drawn with ([`prices_key`]; a tool that
    /// [`DrawTool::reads_prices`] draws every drawing again when it moves).
    prices_key: u64,
    /// Draw every drawing again next update (the tool's own state changed: [`ToolMaps::redraw`]).
    stale: bool,
    /// The drawings last reported to [`DrawTool::changed`] (`None`: none seen yet; the first
    /// set seen is the baseline, never reported).
    sent: Option<Vec<(Anchor, Anchor)>>,
    /// The last merge left something out (the map limits): it is merged again, drawings in
    /// view first, when which drawings are in view changes ([`in_view_key`]).
    dropped: bool,
    in_view: u64,
    /// The tool's overlay ([`DrawTool::overlay`]) and the key it was asked at.
    overlay: Option<LevelMap>,
    overlay_key: Option<u64>,
}

/// A chart's plugin tools and their drawings' maps.
pub struct ToolMaps {
    entries: Vec<Entry>,
    ctx: DrawToolContext,
    /// Bumped by every change of any merged map.
    rev: u64,
}

impl ToolMaps {
    /// The tools serving `ctx`'s product (none for an option contract: its prices are not the
    /// underlying's).
    pub fn new(ctx: DrawToolContext) -> ToolMaps {
        let option = ctx.contract.is_some_and(|c| c.option_key().is_some());
        let entries = if option {
            Vec::new()
        } else {
            lines_feed::levels::draw_tools_serving(&ctx.product)
                .into_iter()
                .map(|tool| Entry {
                    tool,
                    cache: Vec::new(),
                    merged: None,
                    rev: 0,
                    drawn_kinds: u64::MAX,
                    prices_key: 0,
                    stale: false,
                    sent: None,
                    dropped: false,
                    in_view: 0,
                    overlay: None,
                    overlay_key: None,
                })
                .collect()
        };
        ToolMaps {
            entries,
            ctx,
            rev: 0,
        }
    }

    /// The chart's contract changed (a roll): every drawing is drawn again for it.
    pub fn set_contract(&mut self, contract: Option<ContractKey>) {
        if self.ctx.contract != contract {
            let product = self.ctx.product.clone();
            *self = ToolMaps::new(DrawToolContext { product, contract });
            self.rev = next_rev();
        }
    }

    /// The chart's market, as its tools draw for it.
    pub fn context(&self) -> &DrawToolContext {
        &self.ctx
    }

    /// Tool `id`, if it serves this chart.
    pub fn tool(&self, id: &str) -> Option<&Arc<dyn DrawTool>> {
        self.entries.iter().map(|e| &e.tool).find(|t| t.id() == id)
    }

    /// Tool `id`'s state changed ([`lines_feed::levels::ToolEffect::Redraw`]): its drawings are
    /// drawn again at the next update.
    pub fn redraw(&mut self, id: &str) {
        for e in self.entries.iter_mut().filter(|e| e.tool.id() == id) {
            e.stale = true;
        }
    }

    /// Whether any serving tool's overlay key moved since the last frame (an async overlay
    /// finished reading, or a live overlay is due to refresh). Allocation-free.
    pub fn overlay_changed(&self, view: Option<DrawView>) -> bool {
        let ctx = &self.ctx;
        self.entries
            .iter()
            .any(|e| view.map(|v| e.tool.overlay_key(ctx, &v)) != e.overlay_key)
    }

    /// Report each tool's saved drawings (`committed`: the product's drawings, no preview or
    /// drag) to it when they differ from what it was last told ([`DrawTool::changed`]). The
    /// first set a chart sees is its baseline. Allocates only on a change.
    pub fn report<I>(&mut self, committed: I)
    where
        I: Iterator<Item = (Tool, Anchor, Anchor)> + Clone,
    {
        for e in &mut self.entries {
            let id = e.tool.id();
            let mine = committed
                .clone()
                .filter(move |(t, _, _)| matches!(t, Tool::Plugin(p) if p.id == id))
                .map(|(_, a, b)| (a, b));
            if e.sent
                .as_ref()
                .is_some_and(|s| mine.clone().eq(s.iter().copied()))
            {
                continue;
            }
            let now: Vec<(Anchor, Anchor)> = mine.collect();
            if e.sent.is_some() {
                e.tool.changed(&self.ctx, &now);
            }
            e.sent = Some(now);
        }
    }

    /// The tools for the toolbar, in registration order.
    pub fn tools(&self) -> impl Iterator<Item = (Tool, &'static str)> + '_ {
        self.entries
            .iter()
            .map(|e| (tool_of(&*e.tool), e.tool.icon()))
    }

    /// Bumped whenever a merged map changes.
    pub fn revision(&self) -> u64 {
        self.rev
    }

    /// Each tool, its merged map (`None`: no drawings) and that map's revision.
    pub fn maps(&self) -> impl Iterator<Item = (&Arc<dyn DrawTool>, Option<&Arc<LevelMap>>, u64)> {
        self.entries
            .iter()
            .map(|e| (&e.tool, e.merged.as_ref(), e.rev))
    }

    /// Tool `id`'s merged map, if it has drawings here.
    pub fn map(&self, id: &str) -> Option<&Arc<LevelMap>> {
        self.entries
            .iter()
            .find(|e| e.tool.id() == id)?
            .merged
            .as_ref()
    }

    /// Which of tool `id`'s merged kinds the levels layer draws (`kinds(id)`: a bit per kind
    /// index; 0: none, its levels hidden), so hit-testing finds only what is drawn.
    pub fn set_drawn_kinds(&mut self, kinds: impl Fn(&str) -> u64) {
        for e in &mut self.entries {
            e.drawn_kinds = kinds(e.tool.id());
        }
    }

    /// What drawing `id` of tool `tool` drew (`None`: nothing, or not one of these tools'),
    /// how many of its levels (from the first) the merged map holds (the rest aren't drawn),
    /// and whether its kind `slug` is drawn now. No allocation.
    pub fn drawing_map(
        &self,
        tool: &str,
        id: DrawingId,
    ) -> Option<(&LevelMap, usize, impl Fn(&str) -> bool + '_)> {
        let e = self.entries.iter().find(|e| e.tool.id() == tool)?;
        let c = e.cache.iter().find(|c| c.id == id)?;
        let map = c.map.as_ref()?;
        let shown = move |slug: &str| {
            e.merged
                .as_ref()
                .and_then(|m| m.kind_index(slug))
                .is_some_and(|k| k < 64 && e.drawn_kinds & (1u64 << k) != 0)
        };
        Some((map, c.merged, shown))
    }

    /// This frame's plugin drawings, in drawing order: `(id, tool, a, b)` (a drag's drawing
    /// where it is now, a placement's preview as [`PREVIEW`]). Calls a tool only for a drawing
    /// whose points changed, and merges a tool's map again only then; with nothing changed it
    /// compares and returns false without allocating. `tick`: the chart's tick (maps off the
    /// grid are refused).
    /// `view`: what the chart shows (a tool's overlay; when a tool's drawings don't all fit one
    /// map, those in view go first).
    pub fn update<I>(
        &mut self,
        drawings: I,
        tick: i64,
        prices: &dyn ChartBarsPrices,
        view: Option<DrawView>,
    ) -> bool
    where
        I: Iterator<Item = (DrawingId, Tool, Anchor, Anchor)> + Clone,
    {
        let ctx = &self.ctx;
        let mut any = false;
        let key = prices.key();
        let visible = view.map(|v| v.visible);
        for e in &mut self.entries {
            let id = e.tool.id();
            // The overlay, asked again when its key moves (none without a view: hidden).
            let ok = view.map(|v| e.tool.overlay_key(ctx, &v));
            let overlay_moved = ok != e.overlay_key;
            if overlay_moved {
                e.overlay_key = ok;
                e.overlay = view
                    .and_then(|v| e.tool.overlay(ctx, &v))
                    .and_then(|m| checked(m, tick).ok());
            }
            // Drawn again from scratch (no cached map reused): the tool's state changed, or the
            // bars it reads did.
            let force = e.stale || (e.tool.reads_prices() && e.prices_key != key);
            e.stale = false;
            e.prices_key = key;
            let mine = drawings
                .clone()
                .filter(move |(_, t, _, _)| matches!(t, Tool::Plugin(p) if p.id == id));
            let same = mine
                .clone()
                .map(|(d, _, a, b)| (d, a, b))
                .eq(e.cache.iter().map(|c| (c.id, c.a, c.b)));
            if same && !force {
                // Everything fit (or the same drawings are in view), the overlay unmoved:
                // nothing to do.
                if !overlay_moved && (!e.dropped || in_view_key(&e.cache, visible) == e.in_view) {
                    continue;
                }
                e.in_view = in_view_key(&e.cache, visible);
                let (merged, dropped) = merge(
                    &*e.tool,
                    ctx,
                    &mut e.cache,
                    tick,
                    visible,
                    e.overlay.as_ref(),
                );
                e.dropped = dropped;
                if merged.as_ref() != e.merged.as_deref() {
                    e.merged = merged.map(Arc::new);
                    e.rev = next_rev();
                    any = true;
                }
                continue;
            }
            let mut cache = Vec::with_capacity(e.cache.len() + 1);
            for (d, _, a, b) in mine {
                let old = e
                    .cache
                    .iter()
                    .position(|c| !force && c.id == d && c.a == a && c.b == b);
                let map = match old {
                    Some(i) => e.cache.swap_remove(i).map,
                    None => drawn(&*e.tool, ctx, a, b, tick, prices),
                };
                cache.push(Cached {
                    id: d,
                    a,
                    b,
                    map,
                    merged: 0,
                });
            }
            e.cache = cache;
            e.in_view = in_view_key(&e.cache, visible);
            let (merged, dropped) = merge(
                &*e.tool,
                ctx,
                &mut e.cache,
                tick,
                visible,
                e.overlay.as_ref(),
            );
            e.dropped = dropped;
            if merged.as_ref() != e.merged.as_deref() {
                e.merged = merged.map(Arc::new);
                e.rev = next_rev();
                any = true;
            }
        }
        if any {
            self.rev = next_rev();
        }
        any
    }
}

/// The drawing tool `t` as a `Tool`.
pub fn tool_of(t: &dyn DrawTool) -> Tool {
    Tool::Plugin(PluginTool {
        id: t.id(),
        label: t.label(),
    })
}

/// What `tool` draws for `a` and `b`, checked as a level source's map would be (rebuilt through
/// the builder against the chart's tick); a refusal is logged at debug level.
fn drawn(
    tool: &dyn DrawTool,
    ctx: &DrawToolContext,
    a: Anchor,
    b: Anchor,
    tick: i64,
    prices: &dyn DrawPrices,
) -> Option<LevelMap> {
    let r = tool
        .draw_with(ctx, a, b, prices)
        .and_then(|m| checked(m, tick).map_err(|e| e.to_string()));
    match r {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::debug!(tool = tool.id(), "draw tool drew nothing: {e}");
            None
        }
    }
}

/// `m` rebuilt through `LevelMapBuilder` (the validation a level source's map gets).
fn checked(m: LevelMap, tick: i64) -> Result<LevelMap, lines_core::levels::LevelError> {
    let mut b = LevelMap::builder(&m.source, m.product, m.as_of);
    for l in &m.levels {
        let kind = m.kinds.get(usize::from(l.kind)).map_or("", String::as_str);
        b = b.level(kind, l.shape, l.label.as_deref(), l.style, l.meta.clone());
        if let Some(t) = l.from {
            b = b.from(t);
        }
        if let Some(t) = l.to {
            b = b.to(t);
        }
        if let Some(g) = l.group.and_then(|g| m.groups.get(usize::from(g))) {
            b = b.group(g);
        }
    }
    for k in &m.markers {
        b = b.marker(k.at, k.price, k.label.as_deref());
        if let Some(g) = k.group.and_then(|g| m.groups.get(usize::from(g))) {
            b = b.group(g);
        }
        if k.hollow {
            b = b.hollow();
        }
        if k.joined {
            b = b.joined();
        }
    }
    b.build(tick)
}

/// Merge priority for `c` against `visible` (lower is kept first when over the map limits):
/// `0` when its points overlap `visible`, `1` when a bounded level (`to: Some(_)`) spans
/// `visible` (e.g. a morning session ruler extending to session close), `2` otherwise.
fn view_rank(c: &Cached, visible: Option<(UtcMillis, UtcMillis)>) -> u8 {
    let Some((lo, hi)) = visible else {
        return 0;
    };
    if c.a.time.min(c.b.time) <= hi && c.a.time.max(c.b.time) >= lo {
        return 0;
    }
    if c.map.as_ref().is_some_and(|m| {
        m.levels
            .iter()
            .any(|l| l.to.is_some_and(|t| t >= lo) && l.from.is_none_or(|f| f <= hi))
    }) {
        return 1;
    }
    2
}

/// A key of which drawings are in view (no allocation).
fn in_view_key(parts: &[Cached], visible: Option<(UtcMillis, UtcMillis)>) -> u64 {
    parts.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, c| {
        let rank = u64::from(view_rank(c, visible));
        (h ^ (c.id.wrapping_mul(4) | rank)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The group the tool's overlay ([`DrawTool::overlay`]) takes in the merged map (no drawing's).
const OVERLAY_GROUP: &str = "overlay";

/// Add map `m` to `out` within the limits, its groups named `name(g)`: how many of its levels
/// (from the first) were taken, and whether anything didn't fit.
fn merge_one(
    out: &mut LevelMap,
    m: &LevelMap,
    name: impl Fn(Option<u16>) -> String,
) -> (usize, bool) {
    let group = |g: Option<u16>, out: &mut LevelMap| -> Option<u16> {
        let name = name(g);
        if let Some(i) = out.groups.iter().position(|x| *x == name) {
            return Some(i as u16);
        }
        (out.groups.len() < MAX_GROUPS).then(|| {
            out.groups.push(name);
            (out.groups.len() - 1) as u16
        })
    };
    let dropped = m.levels.len() + out.levels.len() > MAX_LEVELS
        || m.markers.len() + out.markers.len() > MAX_MARKERS
        || out.groups.len() >= MAX_GROUPS;
    let mut taken = 0;
    for l in &m.levels {
        if out.levels.len() >= MAX_LEVELS {
            break;
        }
        taken += 1;
        let Some(slug) = m.kinds.get(usize::from(l.kind)) else {
            continue;
        };
        let kind = match out.kinds.iter().position(|k| k == slug) {
            Some(i) => i as u16,
            None if out.kinds.len() < MAX_KINDS => {
                out.kinds.push(slug.clone());
                (out.kinds.len() - 1) as u16
            }
            None => continue,
        };
        let mut l = l.clone();
        l.kind = kind;
        l.group = group(l.group, out);
        out.levels.push(l);
    }
    for k in &m.markers {
        if out.markers.len() >= MAX_MARKERS {
            break;
        }
        let mut k = k.clone();
        k.group = group(k.group, out);
        out.markers.push(k);
    }
    (taken, dropped)
}

/// One map of the tool's overlay and every drawing's levels: kinds by slug, each drawing's
/// levels and markers one event (pointing at one highlights that drawing; its own groups stay
/// apart), within the map limits (what doesn't fit is left out: the overlay first, then the
/// drawings in `visible`, then the rest in order). `None` with neither; and whether anything
/// was left out.
fn merge(
    tool: &dyn DrawTool,
    ctx: &DrawToolContext,
    parts: &mut [Cached],
    tick: i64,
    visible: Option<(UtcMillis, UtcMillis)>,
    overlay: Option<&LevelMap>,
) -> (Option<LevelMap>, bool) {
    if parts.is_empty() && overlay.is_none() {
        return (None, false);
    }
    let mut dropped = false;
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by_key(|&i| view_rank(&parts[i], visible));
    let as_of = parts
        .iter()
        .filter_map(|c| c.map.as_ref().map(|m| m.as_of))
        .chain(overlay.map(|m| m.as_of))
        .max()
        .unwrap_or(lines_core::UtcMillis(0));
    let Ok(mut out) = LevelMap::builder(tool.id(), ctx.product.id, as_of).build(tick) else {
        return (None, false);
    };
    out.contract = ctx.contract;
    if let Some(m) = overlay {
        dropped |= merge_one(&mut out, m, |g| match g {
            Some(g) => format!("{OVERLAY_GROUP}/{g}"),
            None => OVERLAY_GROUP.to_owned(),
        })
        .1;
    }
    for i in order {
        let c = &mut parts[i];
        c.merged = 0;
        let Some(m) = &c.map else { continue };
        let id = c.id;
        let (taken, d) = merge_one(&mut out, m, |g| match g {
            Some(g) => format!("d{id}/{g}"),
            None => format!("d{id}"),
        });
        c.merged = taken;
        dropped |= d;
    }
    (Some(out), dropped)
}

/// The chart's prices for its drawing tools, and a key that moves when its bars do.
pub trait ChartBarsPrices: DrawPrices {
    /// Moves when a bar is added or dropped (not on every trade).
    fn key(&self) -> u64;
}

/// [`ChartBarsPrices`] over a chart's bars and its market clock.
pub struct BarsPrices<'a, 'b>(pub &'b TimeAxis<'a>, pub Option<UtcMillis>);

impl DrawPrices for BarsPrices<'_, '_> {
    fn first_beyond(
        &self,
        after: UtcMillis,
        before: UtcMillis,
        price: Price,
        above: bool,
    ) -> Option<UtcMillis> {
        let bars = self.0.view.bars;
        let from = bars.partition_point(|b| b.time <= after);
        bars[from..]
            .iter()
            .take_while(|b| b.time < before)
            .find(|b| if above { b.high > price } else { b.low < price })
            .map(|b| b.time)
    }
    fn covered(&self) -> Option<(UtcMillis, UtcMillis)> {
        // Only trade-built bars (e.g. tick bars) can settle a sub-bar `after` time: on coarse
        // time bars (1m, 15m, 1h, 1D), `first_beyond` skips the entire bucket holding `after`.
        let tb = self.0.view.trades?;
        let bars = self.0.view.bars;
        let (first, last) = (bars.first()?, bars.last()?);
        let hi = if tb.is_partial_at(bars.len() - 1, self.1) {
            self.1.map_or(last.time, |n| n.max(last.time))
        } else {
            last.time
        };
        Some((first.time, hi))
    }
    fn has_bars(&self, from: UtcMillis, to: UtcMillis) -> bool {
        let bars = self.0.view.bars;
        let i = bars.partition_point(|b| b.time < from);
        bars.get(i).is_some_and(|b| b.time < to)
    }
}

impl ChartBarsPrices for BarsPrices<'_, '_> {
    fn key(&self) -> u64 {
        let bars = self.0.view.bars;
        let first = bars.first().map_or(0, |b| b.time.0 as u64);
        (bars.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ first
    }
}

/// No bars (a hidden chart).
pub struct NoBars;

impl DrawPrices for NoBars {
    fn first_beyond(&self, _: UtcMillis, _: UtcMillis, _: Price, _: bool) -> Option<UtcMillis> {
        None
    }
    fn covered(&self) -> Option<(UtcMillis, UtcMillis)> {
        None
    }
}

impl ChartBarsPrices for NoBars {
    fn key(&self) -> u64 {
        0
    }
}

/// `--plugin-drawing "TOOL-ID TIME@PRICE TIME@PRICE"` (screenshots): the registered tool and
/// its two points on `product`'s grid (ISO times; prices as the chart shows them, exact).
#[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
pub fn parse_spec(
    spec: &str,
    product: &lines_core::Product,
) -> Result<(Tool, Anchor, Anchor), String> {
    let mut words = spec.split_whitespace();
    let (Some(id), Some(a), Some(b), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return Err(format!(
            "{spec:?}: expected \"TOOL-ID TIME@PRICE TIME@PRICE\""
        ));
    };
    let tool = lines_feed::levels::draw_tool(id)
        .filter(|t| t.serves(product))
        .ok_or_else(|| format!("no drawing tool {id:?} for {}", product.symbol))?;
    let point = |w: &str| -> Result<Anchor, String> {
        let (t, p) = w
            .split_once('@')
            .ok_or_else(|| format!("{w:?}: expected TIME@PRICE"))?;
        let time = lines_core::UtcMillis::parse_iso(t).ok_or_else(|| format!("bad time {t:?}"))?;
        let units = product
            .price
            .scale
            .parse_decimal(p)
            .map_err(|e| format!("bad price {p:?}: {e:?}"))?;
        if units % product.price.tick.max(1) != 0 {
            return Err(format!("price {p} is off the tick grid"));
        }
        Ok(Anchor {
            time,
            price: lines_core::Price(units),
        })
    };
    Ok((tool_of(&*tool), point(a)?, point(b)?))
}

#[cfg(test)]
mod tests;
