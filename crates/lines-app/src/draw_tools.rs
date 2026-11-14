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
use lines_core::drawings::{Anchor, DrawingId, PluginTool, Tool};
use lines_core::levels::{LevelMap, MAX_GROUPS, MAX_KINDS, MAX_LEVELS, MAX_MARKERS};
use lines_feed::levels::{DrawTool, DrawToolContext};

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
}

struct Entry {
    tool: Arc<dyn DrawTool>,
    cache: Vec<Cached>,
    /// Every drawing's levels in one map (`None`: no drawings of this tool on the chart).
    merged: Option<Arc<LevelMap>>,
    rev: u64,
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

    /// This frame's plugin drawings, in drawing order: `(id, tool, a, b)` (a drag's drawing
    /// where it is now, a placement's preview as [`PREVIEW`]). Calls a tool only for a drawing
    /// whose points changed, and merges a tool's map again only then; with nothing changed it
    /// compares and returns false without allocating. `tick`: the chart's tick (maps off the
    /// grid are refused).
    pub fn update<I>(&mut self, drawings: I, tick: i64) -> bool
    where
        I: Iterator<Item = (DrawingId, Tool, Anchor, Anchor)> + Clone,
    {
        let ctx = &self.ctx;
        let mut any = false;
        for e in &mut self.entries {
            let id = e.tool.id();
            let mine = drawings
                .clone()
                .filter(move |(_, t, _, _)| matches!(t, Tool::Plugin(p) if p.id == id));
            let same = mine
                .clone()
                .map(|(d, _, a, b)| (d, a, b))
                .eq(e.cache.iter().map(|c| (c.id, c.a, c.b)));
            if same {
                continue;
            }
            let mut cache = Vec::with_capacity(e.cache.len() + 1);
            for (d, _, a, b) in mine {
                let old = e
                    .cache
                    .iter()
                    .position(|c| c.id == d && c.a == a && c.b == b);
                let map = match old {
                    Some(i) => e.cache.swap_remove(i).map,
                    None => drawn(&*e.tool, ctx, a, b, tick),
                };
                cache.push(Cached { id: d, a, b, map });
            }
            e.cache = cache;
            let merged = merge(&*e.tool, ctx, &e.cache, tick);
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
) -> Option<LevelMap> {
    let r = tool
        .draw(ctx, a, b)
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

/// One map of every drawing's levels: kinds by slug, each drawing's levels and markers one event
/// (pointing at one highlights that drawing; its own groups stay apart), within the map limits
/// (what doesn't fit is left out). `None` with no drawings.
fn merge(
    tool: &dyn DrawTool,
    ctx: &DrawToolContext,
    parts: &[Cached],
    tick: i64,
) -> Option<LevelMap> {
    if parts.is_empty() {
        return None;
    }
    let as_of = parts
        .iter()
        .filter_map(|c| c.map.as_ref().map(|m| m.as_of))
        .max()
        .unwrap_or(lines_core::UtcMillis(0));
    let mut out = LevelMap::builder(tool.id(), ctx.product.id, as_of)
        .build(tick)
        .ok()?;
    out.contract = ctx.contract;
    for c in parts {
        let Some(m) = &c.map else { continue };
        let group = |g: Option<u16>, out: &mut LevelMap| -> Option<u16> {
            let name = match g {
                Some(g) => format!("d{}/{g}", c.id),
                None => format!("d{}", c.id),
            };
            if let Some(i) = out.groups.iter().position(|x| *x == name) {
                return Some(i as u16);
            }
            (out.groups.len() < MAX_GROUPS).then(|| {
                out.groups.push(name);
                (out.groups.len() - 1) as u16
            })
        };
        for l in &m.levels {
            if out.levels.len() >= MAX_LEVELS {
                break;
            }
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
            l.group = group(l.group, &mut out);
            out.levels.push(l);
        }
        for k in &m.markers {
            if out.markers.len() >= MAX_MARKERS {
                break;
            }
            let mut k = k.clone();
            k.group = group(k.group, &mut out);
            out.markers.push(k);
        }
    }
    Some(out)
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
