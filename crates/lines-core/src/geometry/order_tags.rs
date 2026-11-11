//! Chart order lines (roadmap 8, phase 1): where order and position tags go, which orders are
//! off the plot and where their edge markers sit, the position tag's add-stop / add-target
//! handles, and what the pointer hits. Pure and allocation-free (fixed output buffers), so the
//! chart runs it in paint (`tests/no_alloc.rs`).
//!
//! - **Lines** sit at their price's y. **Tags** (role, side, quantity, price, P&L at that price,
//!   and a `×` box) are right-aligned at the plot's right edge, centred on their line; tags that
//!   would overlap are pushed apart vertically (in price order) and kept inside the band left
//!   between the edge markers, so every tag stays readable and clickable. On the left
//!   ([`TagSide::Left`], the trader's choice) they start at the plot's left edge in the position
//!   tag's column, mirrored (`×` at the outer, left end), below the top-left legend block and
//!   never over the position tag or its handles ([`Column`]).
//! - **Edge markers**: orders whose price is above or below the plot get one marker at that
//!   edge (the nearest such order, with how many there are); clicking it scrolls that price
//!   into view.
//! - **Handles**: the position's tag carries up to two handles, one above it (add a target for
//!   a long, a stop for a short) and one below; dragging one away places that exit.
//! - **Hit-testing** is by box: `×` (cancel / flatten), tag body (drag to move), handle (drag to
//!   add), edge marker (scroll). Lines themselves are not targets: a 1 px line is too easy to
//!   catch by accident while panning.
//! - **Snapping** is the crosshair's ([`super::snap_price`]): the tick nearest the pointer.
//! - **Groups** ([`group_orders`]): orders of one kind whose tags can't sit at their own levels
//!   share one tag; zooming in splits them back. No order is ever left without a tag.
//! - **Sizes** come from [`Metrics`] (the UI zoom's; [`Metrics::BASE`] is 100 %). Every laid-out
//!   box carries its own size, so a hit test always matches the frame that drew it.

use super::snap;
use crate::{Price, Viewport};

/// Most tags a chart lays out; [`group_orders`] keeps every order within this (and the band).
pub const MAX_TAGS: usize = 32;
/// Tag height and the `×` box's width, logical px.
pub const TAG_H: f32 = 20.0;
/// The `×` box: wider than its glyph, so a cancel is aimed, not grazed.
pub const CLOSE_W: f32 = 22.0;
/// Gap between stacked tags and between a tag and the plot's right edge.
pub const TAG_GAP: f32 = 2.0;
/// A handle's size (logical px).
pub const HANDLE_W: f32 = 26.0;
pub const HANDLE_H: f32 = 20.0;
/// Space between a handle and the position tag.
pub const HANDLE_GAP: f32 = 3.0;
/// A tag's or edge marker's horizontal text padding, both sides together.
pub const TEXT_PAD: f32 = 12.0;

/// The tags' sizes at one scale (the UI zoom): the constants above times `scale`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Metrics {
    /// The factor the sizes are at (1.0: the constants).
    pub scale: f32,
    pub tag_h: f32,
    pub close_w: f32,
    pub tag_gap: f32,
    pub handle_w: f32,
    pub handle_h: f32,
    pub handle_gap: f32,
    pub text_pad: f32,
}

impl Metrics {
    /// 100 %: the constants.
    pub const BASE: Metrics = Metrics {
        scale: 1.0,
        tag_h: TAG_H,
        close_w: CLOSE_W,
        tag_gap: TAG_GAP,
        handle_w: HANDLE_W,
        handle_h: HANDLE_H,
        handle_gap: HANDLE_GAP,
        text_pad: TEXT_PAD,
    };

    /// The sizes at `scale` (a non-finite or non-positive one is 100 %), each in whole logical
    /// px and the tag's height even, so a tag centered on its (snapped) line and everything
    /// placed from it keeps its edges on device pixels (`docs/design.md`: crisp at every DPI).
    pub fn at(scale: f32) -> Metrics {
        if !(scale.is_finite() && scale > 0.0) {
            return Metrics::BASE;
        }
        let px = |v: f32| (v * scale).round();
        Metrics {
            scale,
            tag_h: (TAG_H * scale * 0.5).round() * 2.0,
            close_w: px(CLOSE_W),
            tag_gap: px(TAG_GAP),
            handle_w: px(HANDLE_W),
            handle_h: px(HANDLE_H),
            handle_gap: px(HANDLE_GAP),
            text_pad: px(TEXT_PAD),
        }
    }
}

impl Metrics {
    /// The sizes at the UI zoom `zoom`: hit targets never under their 100 % size (`zoom::hit`'s
    /// rule), so below 100 % only the text shrinks.
    pub fn for_ui_zoom(zoom: f32) -> Metrics {
        Metrics::at(zoom.max(1.0))
    }
}

impl Default for Metrics {
    fn default() -> Metrics {
        Metrics::BASE
    }
}

/// Which edge of the plot order tags (and their edge markers) sit at.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum TagSide {
    /// Beside the price axis (the default).
    #[default]
    Right,
    /// At the plot's left edge, in the position tag's column, mirrored.
    Left,
}

/// The tag column: its side and, on the left, what it keeps clear of. On the right both are
/// ignored (nothing else lives there).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Column {
    pub side: TagSide,
    /// The top-left block's bottom (the readout and legends, plot-local): tags and the top edge
    /// marker stay below it.
    pub top_clear: f32,
    /// A span `(top, bottom)` of the column already taken (the position tag with its handles):
    /// tags step around it.
    pub taken: Option<(f32, f32)>,
    /// The taken box's right edge (plot-local): an edge marker or the add-exit preview that
    /// can't step around it vertically sits right of it instead.
    pub taken_right: f32,
}

impl Column {
    pub const RIGHT: Column = Column {
        side: TagSide::Right,
        top_clear: 0.0,
        taken: None,
        taken_right: 0.0,
    };

    fn left(&self) -> bool {
        self.side == TagSide::Left
    }

    /// The left column's x: the position tag's (`chart_view::draw_position_tag`).
    pub fn left_x(m: &Metrics) -> f32 {
        (4.0 * m.scale).round()
    }

    /// Where a box `top`..`top + h` goes on the left: the column's x, or right of the taken
    /// box when it would meet it (a gap kept), so it never covers the position tag.
    pub fn left_x_at(&self, top: f32, h: f32, m: &Metrics) -> f32 {
        match self.taken {
            Some((a, b)) if top < b + m.tag_gap && top + h + m.tag_gap > a => {
                self.taken_right + m.tag_gap
            }
            _ => Column::left_x(m),
        }
    }

    /// The highest a box goes in the column: below the left column's top-left block.
    pub fn top_floor(&self) -> f32 {
        if self.left() { self.top_clear } else { 0.0 }
    }
}

/// One line to tag: an id the caller chooses (order id; the position uses its own), its
/// price, and the tag's text width (measured by the caller, logical px).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LineIn {
    pub id: u64,
    pub price: Price,
    pub text_w: f32,
}

/// A laid-out tag (plot-local logical px).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct TagBox {
    pub id: u64,
    /// The line's price (stacking order: higher prices higher, also when lines share a pixel).
    pub price: Price,
    /// The line's y (the price), which the tag may be pushed away from.
    pub line_y: f32,
    pub left: f32,
    pub top: f32,
    pub w: f32,
    /// Its height ([`Metrics::tag_h`] when laid out).
    pub h: f32,
    /// Where the `×` box starts, and its width: the tag's right end, or its left end when
    /// `mirrored`.
    pub close_left: f32,
    pub close_w: f32,
    /// Laid out on the left ([`TagSide::Left`]): `×` at the left (outer) end, the text after
    /// it, and controls that sit beside a tag (the resize control, a staged entry's handles)
    /// on its right (inner) side.
    pub mirrored: bool,
}

impl TagBox {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.left + self.w && y >= self.top && y < self.top + self.h
    }

    /// Whether `x` (inside the tag) is on its `×` box.
    pub fn on_close(&self, x: f32) -> bool {
        x >= self.close_left && x < self.close_left + self.close_w
    }

    /// Where the text part starts (after the `×` box when mirrored).
    pub fn body_left(&self) -> f32 {
        if self.mirrored {
            self.left + self.close_w
        } else {
            self.left
        }
    }

    /// The `×` box's divider: the edge it shares with the text part.
    pub fn divider_x(&self) -> f32 {
        if self.mirrored {
            self.close_left + self.close_w
        } else {
            self.close_left
        }
    }

    /// The inner end (toward the candles): the left end on the right, the right end mirrored.
    pub fn inner_x(&self) -> f32 {
        if self.mirrored {
            self.left + self.w
        } else {
            self.left
        }
    }
}

/// An order off the plot: the marker at the top (`above`) or bottom edge, for the nearest such
/// order (its id and price) and how many orders are beyond that edge.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct EdgeMarker {
    pub id: u64,
    pub price: Price,
    pub n: usize,
    pub above: bool,
    pub left: f32,
    pub top: f32,
    pub w: f32,
    pub h: f32,
}

impl EdgeMarker {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.left + self.w && y >= self.top && y < self.top + self.h
    }
}

/// A handle on the position's tag: drag it away to place that exit.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Handle {
    /// Above the tag (higher prices) or below.
    pub above: bool,
    pub left: f32,
    pub top: f32,
    pub w: f32,
    pub h: f32,
}

impl Handle {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.left + self.w && y >= self.top && y < self.top + self.h
    }
}

/// What the pointer is on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TagHit {
    /// The tag's body: drag to move that order.
    Body(u64),
    /// The `×`: cancel that order (or flatten, for the position's tag).
    Close(u64),
    /// A position handle: `true` above the tag.
    Handle(bool),
    /// An edge marker: scroll to this price.
    Edge(Price),
}

/// Edge markers for `lines` whose price is off the plot `[0, plot_h]`: `[top, bottom]`, the
/// nearest off-plot order at each edge, on the column's side (right-aligned at `plot_w`, or at
/// the left column's x below its top-left block) with the width the caller measured for each
/// marker's text (`text_w`), sized by `m`. Allocation-free.
pub fn edge_markers(
    vp: &Viewport,
    lines: &[LineIn],
    plot_w: f32,
    plot_h: f32,
    text_w: [f32; 2],
    col: &Column,
    m: &Metrics,
) -> [Option<EdgeMarker>; 2] {
    let mut out: [Option<EdgeMarker>; 2] = [None, None];
    for l in lines {
        let y = vp.price_to_y(l.price.0 as f64);
        if !y.is_finite() {
            continue;
        }
        let k = if y < 0.0 {
            0
        } else if y > plot_h as f64 {
            1
        } else {
            continue;
        };
        let nearer = |m: &EdgeMarker| {
            if k == 0 {
                l.price.0 < m.price.0
            } else {
                l.price.0 > m.price.0
            }
        };
        match &mut out[k] {
            Some(m) => {
                m.n += 1;
                if nearer(m) {
                    m.id = l.id;
                    m.price = l.price;
                }
            }
            slot @ None => {
                *slot = Some(EdgeMarker {
                    id: l.id,
                    price: l.price,
                    n: 1,
                    above: k == 0,
                    ..EdgeMarker::default()
                })
            }
        }
    }
    for (k, e) in out.iter_mut().enumerate() {
        if let Some(e) = e {
            e.w = (text_w[k] + m.text_pad).min(plot_w);
            e.h = m.tag_h;
            e.top = if k == 0 {
                top_floor(col, m)
            } else {
                plot_h - m.tag_gap - m.tag_h
            };
            // On the left, never over the position tag: beside it instead.
            e.left = if col.left() {
                col.left_x_at(e.top, e.h, m).min((plot_w - e.w).max(0.0))
            } else {
                plot_w - m.tag_gap - e.w
            };
        }
    }
    out
}

/// The highest a top edge marker goes in the column: the left one stays below its top-left
/// block.
fn top_floor(col: &Column, m: &Metrics) -> f32 {
    if col.left() && col.top_clear > 0.0 {
        col.top_clear + m.tag_gap
    } else {
        m.tag_gap
    }
}

/// The band tags may use, leaving room for the edge markers (and, on the left, the top-left
/// block): `(top, bottom)`.
pub fn tag_band(
    markers: &[Option<EdgeMarker>; 2],
    plot_h: f32,
    col: &Column,
    m: &Metrics,
) -> (f32, f32) {
    let top = match markers[0] {
        Some(e) => e.top + e.h + m.tag_gap,
        None if col.left() && col.top_clear > 0.0 => col.top_clear + m.tag_gap,
        None => 0.0,
    };
    let bottom = if markers[1].is_some() {
        plot_h - m.tag_gap - m.tag_h - m.tag_gap
    } else {
        plot_h
    };
    (top, bottom.max(top))
}

/// Lay out the tags of `lines` on a plot `plot_w` × `plot_h`, on the column's side (right-aligned
/// at `plot_w`, or mirrored at the left column's x), inside the band `band` (`(top, bottom)`,
/// from [`tag_band`]), into `out`; returns how many (at most `out.len()`, [`MAX_TAGS`] and what
/// the band holds, [`tags_fit_in`]). Every order on the plot gets a tag, clamped into the band
/// (off the plot: the edge markers stand for them). Overlapping tags are pushed down in price
/// order (highest price first), then the stack is pulled back up if it ran off the bottom; it
/// never leaves the band. On the left, a tag that would meet the column's taken span (the
/// position tag) steps past it, below on the way down and above on the way back up. Sized by
/// `m`. Allocation-free.
#[allow(clippy::too_many_arguments)]
pub fn layout_tags(
    vp: &Viewport,
    lines: &[LineIn],
    plot_w: f32,
    plot_h: f32,
    band: (f32, f32),
    scale_factor: f32,
    col: &Column,
    m: &Metrics,
    out: &mut [TagBox],
) -> usize {
    let (lo, hi) = band;
    let (tag_h, gap) = (m.tag_h, m.tag_gap);
    let cap = out.len().min(tags_fit_in(band, col, m));
    let taken = col.taken.filter(|_| col.left());
    // Whether a tag at `top` would meet the taken span (a gap kept on each side).
    let meets = |top: f32| taken.is_some_and(|(a, b)| top < b + gap && top + tag_h + gap > a);
    let mut n = 0;
    for l in lines {
        if n == cap {
            break;
        }
        let y = vp.price_to_y(l.price.0 as f64);
        if !(y.is_finite() && (0.0..=plot_h as f64).contains(&y)) {
            continue;
        }
        let line_y = snap(y, scale_factor);
        let w = (l.text_w + m.text_pad + m.close_w).min(plot_w);
        let (left, close_left) = if col.left() {
            let left = Column::left_x(m).min((plot_w - w).max(0.0));
            (left, left)
        } else {
            (plot_w - gap - w, plot_w - gap - m.close_w)
        };
        out[n] = TagBox {
            id: l.id,
            price: l.price,
            line_y,
            left,
            top: line_y - tag_h * 0.5,
            w,
            h: tag_h,
            close_left,
            close_w: m.close_w,
            mirrored: col.left(),
        };
        n += 1;
    }
    let tags = &mut out[..n];
    // Price order, top of the plot first (insertion sort: few tags, no allocation).
    for i in 1..tags.len() {
        let mut j = i;
        while j > 0
            && (tags[j - 1].line_y > tags[j].line_y
                || (tags[j - 1].line_y == tags[j].line_y && tags[j - 1].price.0 < tags[j].price.0))
        {
            tags.swap(j - 1, j);
            j -= 1;
        }
    }
    // Push apart downwards, then pull the whole stack back inside the band.
    for i in 0..tags.len() {
        let floor = if i == 0 {
            lo
        } else {
            tags[i - 1].top + tag_h + gap
        };
        tags[i].top = tags[i].top.max(floor);
        // Past the taken span: above it for a line above its middle when there is room under
        // the tag before, else below it.
        if meets(tags[i].top)
            && let Some((a, b)) = taken
        {
            let above = a - gap - tag_h;
            tags[i].top = if tags[i].line_y < (a + b) * 0.5 && above >= floor {
                above
            } else {
                b + gap
            };
        }
    }
    // (`cap` keeps the stack within the band, so the pull-up below never passes `lo`, but for
    // the one tag a band too short for it still gets.)
    let mut ceiling = hi;
    for t in tags.iter_mut().rev() {
        if t.top + tag_h > ceiling {
            t.top = (ceiling - tag_h).max(0.0);
            if meets(t.top)
                && let Some((a, _)) = taken
            {
                t.top = (a - gap - tag_h).max(lo);
            }
        }
        ceiling = t.top - gap;
    }
    n
}

/// A tag's handles (the position's, a staged entry's): `want[0]` above it, `want[1]` below, at
/// the tag's inner end (its left end; a mirrored tag's right end), sized by `m`.
pub fn handles(tag: &TagBox, want: [bool; 2], m: &Metrics) -> [Option<Handle>; 2] {
    let (w, h) = (m.handle_w, m.handle_h);
    let left = if tag.mirrored {
        tag.left + tag.w - w
    } else {
        tag.left
    };
    [
        want[0].then_some(Handle {
            above: true,
            left,
            top: tag.top - m.handle_gap - h,
            w,
            h,
        }),
        want[1].then_some(Handle {
            above: false,
            left,
            top: tag.top + tag.h + m.handle_gap,
            w,
            h,
        }),
    ]
}

/// What is at plot-local (`x`, `y`): handles first, then the edge markers, then a tag's `×`
/// box, then its body.
pub fn hit(
    tags: &[TagBox],
    markers: &[Option<EdgeMarker>; 2],
    handles: &[Option<Handle>; 2],
    x: f32,
    y: f32,
) -> Option<TagHit> {
    if let Some(h) = handles.iter().flatten().find(|h| h.contains(x, y)) {
        return Some(TagHit::Handle(h.above));
    }
    if let Some(m) = markers.iter().flatten().find(|m| m.contains(x, y)) {
        return Some(TagHit::Edge(m.price));
    }
    let t = tags.iter().find(|t| t.contains(x, y))?;
    Some(if t.on_close(x) {
        TagHit::Close(t.id)
    } else {
        TagHit::Body(t.id)
    })
}

/// The exit a position handle drags out: for a long, above is the target and below the stop;
/// for a short the other way round. `true`: target.
pub fn handle_is_target(position_qty: i64, above: bool) -> bool {
    (position_qty > 0) == above
}

/// Dragging the position line itself to `to` (roadmap 8, phase 2): which exit it sets, by
/// direction from the average price `avg`: toward profit a target, toward loss a stop
/// (`Some(true)`: target); at the average, nothing yet. The order is checked against the
/// market when released (`chart_trade::exit_command`).
pub fn position_drag_exit(position_qty: i64, avg: f64, to: Price) -> Option<bool> {
    let p = to.0 as f64;
    if position_qty == 0 || p == avg || !avg.is_finite() {
        return None;
    }
    Some((p > avg) == (position_qty > 0))
}

/// Whether the handle for a target / stop is offered: only while the position has none (a
/// working order on the exit side of that kind), as the owner asked ("when the bracket has
/// none"). `[above, below]` for the given position.
pub fn wanted_handles(position_qty: i64, has_target: bool, has_stop: bool) -> [bool; 2] {
    if position_qty == 0 {
        return [false, false];
    }
    let above_is_target = handle_is_target(position_qty, true);
    let (above, below) = if above_is_target {
        (!has_target, !has_stop)
    } else {
        (!has_stop, !has_target)
    };
    [above, below]
}

/// The viewport's price centre that brings `price` into view (the price at the plot's
/// middle), for an edge marker's click: the span stays, only the centre moves.
pub fn centre_on(vp: &Viewport, price: Price) -> (f64, f64) {
    let half = (vp.price_hi - vp.price_lo) * 0.5;
    let p = price.0 as f64;
    (p - half, p + half)
}

// ---- grouped tags -------------------------------------------------------------------------

/// A group that holds what no longer fits under its own kind (only on a plot too short for one
/// tag per kind): orders of several kinds, shown by count and range, no average.
pub const MIXED_KEY: u16 = u16::MAX;

/// One order on the plot, for grouping: `key` is its kind (orders of one kind may share a tag),
/// `qty` what is left of it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupIn {
    pub id: u64,
    pub price: Price,
    pub qty: i64,
    pub key: u16,
}

/// One tag's orders: `orders[first..first + n]` of the slice given to [`group_orders`] (which it
/// sorted). `n == 1` is an ordinary tag. The id is its highest-priced order's, so a tag id always
/// names an order. Quantity and notional are exact.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TagGroup {
    pub id: u64,
    pub key: u16,
    pub first: usize,
    pub n: usize,
    pub qty: i64,
    /// Sum of price × quantity, in price units × quantity units.
    pub notional: i128,
    pub lo: Price,
    pub hi: Price,
}

impl TagGroup {
    /// The average price rounded to a price unit, half away from zero, for the label and the
    /// tag's place only: never an order's price. The exact value is `notional / qty`.
    pub fn average(&self) -> Price {
        round_average(self.notional, self.qty).unwrap_or(self.hi)
    }

    /// Whether this tag stands for several orders.
    pub fn is_group(&self) -> bool {
        self.n > 1
    }
}

/// `notional / qty` rounded to the nearest integer, half away from zero; `None` without quantity.
pub fn round_average(notional: i128, qty: i64) -> Option<Price> {
    if qty <= 0 {
        return None;
    }
    let q = qty as i128;
    let half = if notional >= 0 { q / 2 } else { -(q / 2) };
    let rounded = (notional + half) / q;
    Some(Price(i64::try_from(rounded).ok()?))
}

/// How many tags the band holds (what [`layout_tags`] lays out): at least one, however short
/// the plot, so orders on it always have a way in; at most `MAX_TAGS`.
pub fn tags_fit(band: (f32, f32), m: &Metrics) -> usize {
    let fits = (((band.1 - band.0) + m.tag_gap) / (m.tag_h + m.tag_gap))
        .floor()
        .max(0.0) as usize;
    fits.clamp(1, MAX_TAGS)
}

/// How many tags the column holds in `band`: [`tags_fit`], less the slots its taken span
/// (left only) uses.
pub fn tags_fit_in(band: (f32, f32), col: &Column, m: &Metrics) -> usize {
    let fit = tags_fit(band, m);
    match col.taken.filter(|_| col.left()) {
        // The room above the taken span and below it, each counted whole.
        Some((a, b)) if b > band.0 && a < band.1 => {
            let slot = m.tag_h + m.tag_gap;
            let room = |from: f32, to: f32| ((to - from).max(0.0) / slot).floor() as usize;
            (room(band.0, a) + room(b, band.1)).clamp(1, fit)
        }
        _ => fit,
    }
}

/// The tags for the orders on the plot, at most `cap` (from [`tags_fit_in`]) and `out.len()`: every
/// order is in exactly one, so none is hidden. Orders of one kind whose tags can't both sit at
/// their own levels (`m.tag_h + m.tag_gap` apart) share a tag; zooming in splits them back. When that
/// leaves too many tags for the band, the distance grows until it fits; if one tag per kind
/// still doesn't, the last tag takes everything left ([`MIXED_KEY`]). Sorts `orders` by kind,
/// then highest price first. Allocation-free.
pub fn group_orders(
    vp: &Viewport,
    orders: &mut [GroupIn],
    plot_h: f32,
    cap: usize,
    m: &Metrics,
    out: &mut [TagGroup],
) -> usize {
    let cap = cap.min(out.len());
    if cap == 0 || orders.is_empty() {
        return 0;
    }
    orders.sort_unstable_by_key(|o| (o.key, std::cmp::Reverse(o.price.0), o.id));
    let joins = |a: &GroupIn, b: &GroupIn, apart: f64| {
        a.key == b.key
            && (vp.price_to_y(a.price.0 as f64) - vp.price_to_y(b.price.0 as f64)).abs() < apart
    };
    let count = |apart: f64| {
        1 + orders
            .windows(2)
            .filter(|w| !joins(&w[0], &w[1], apart))
            .count()
    };
    let mut apart = (m.tag_h + m.tag_gap) as f64;
    while count(apart) > cap && apart <= plot_h as f64 {
        apart *= 2.0;
    }
    let add = |g: &mut TagGroup, o: &GroupIn| {
        g.n += 1;
        g.qty += o.qty;
        g.notional += o.price.0 as i128 * o.qty as i128;
        g.lo = g.lo.min(o.price);
        g.hi = g.hi.max(o.price);
    };
    let mut n = 0;
    for (i, o) in orders.iter().enumerate() {
        let same = i > 0 && joins(&orders[i - 1], o, apart);
        if same || n == cap {
            if !same {
                out[n - 1].key = MIXED_KEY;
            }
            add(&mut out[n - 1], o);
        } else {
            out[n] = TagGroup {
                id: o.id,
                key: o.key,
                first: i,
                lo: o.price,
                hi: o.price,
                ..TagGroup::default()
            };
            add(&mut out[n], o);
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::super::snap_price;
    use super::*;

    const M: Metrics = Metrics::BASE;

    fn vp() -> Viewport {
        // 0..100 px tall from price 610_000 (top) to 600_000 (bottom): 100 units a pixel.
        Viewport {
            width: 400.0,
            height: 100.0,
            price_lo: 600_000.0,
            price_hi: 610_000.0,
            ..Viewport::default()
        }
    }

    fn line(id: u64, price: i64) -> LineIn {
        LineIn {
            id,
            price: Price(price),
            text_w: 80.0,
        }
    }

    const FULL: (f32, f32) = (0.0, 100.0);

    #[test]
    fn tags_sit_on_their_line_at_the_right_edge() {
        let v = vp();
        let mut out = [TagBox::default(); 4];
        let n = layout_tags(
            &v,
            &[line(7, 605_000)],
            400.0,
            100.0,
            FULL,
            1.0,
            &Column::RIGHT,
            &M,
            &mut out,
        );
        assert_eq!(n, 1);
        let t = out[0];
        assert_eq!(t.line_y, snap(v.price_to_y(605_000.0), 1.0));
        assert!((t.top + TAG_H * 0.5 - t.line_y).abs() < 0.01, "centred");
        assert_eq!(t.left + t.w, 400.0 - TAG_GAP, "right-aligned");
        assert_eq!(t.close_left, 400.0 - TAG_GAP - CLOSE_W);
        // Off the plot: no tag.
        assert_eq!(
            layout_tags(
                &v,
                &[line(1, 620_000)],
                400.0,
                100.0,
                FULL,
                1.0,
                &Column::RIGHT,
                &M,
                &mut out
            ),
            0
        );
    }

    #[test]
    fn close_tags_never_overlap_and_stay_inside_the_band() {
        let v = vp();
        let mut out = [TagBox::default(); 8];
        // Three orders a tick apart (1 px apart on screen), plus one at the bottom edge.
        let lines = [
            line(1, 605_000),
            line(2, 605_025),
            line(3, 604_975),
            line(4, 600_000),
        ];
        let band = (16.0, 84.0);
        let n = layout_tags(
            &v,
            &lines,
            400.0,
            100.0,
            band,
            1.0,
            &Column::RIGHT,
            &M,
            &mut out,
        );
        // The order at the very bottom (y 100) is outside the band: an edge marker's job.
        assert_eq!(n, 3);
        let tags = &out[..n];
        for i in 0..n {
            for j in i + 1..n {
                let (a, b) = (tags[i], tags[j]);
                assert!(
                    a.top + TAG_H <= b.top || b.top + TAG_H <= a.top,
                    "{a:?} / {b:?}"
                );
            }
            assert!(tags[i].top >= band.0 && tags[i].top + TAG_H <= band.1);
        }
        let top_of = |id| tags.iter().find(|t| t.id == id).unwrap().top;
        assert!(
            top_of(2) < top_of(1) && top_of(1) < top_of(3),
            "price order kept"
        );
    }

    /// Code review: an order on the plot but outside the band (under an edge marker) still
    /// gets a tag, clamped into the band; a stack taller than the band never leaves it.
    #[test]
    fn tags_stay_in_the_band_and_none_is_lost_under_a_marker() {
        let v = vp();
        let mut out = [TagBox::default(); 32];
        // y 2: on the plot, above a band that starts at 20.
        let n = layout_tags(
            &v,
            &[line(1, 609_800)],
            400.0,
            100.0,
            (16.0, 84.0),
            1.0,
            &Column::RIGHT,
            &M,
            &mut out,
        );
        assert_eq!(n, 1, "not lost under the marker");
        assert!(out[0].top >= 16.0);
        // Twelve orders in a band that holds three: three tags, all inside.
        let many: Vec<LineIn> = (0..12).map(|i| line(i, 605_000 + i as i64 * 25)).collect();
        let n = layout_tags(
            &v,
            &many,
            400.0,
            100.0,
            (16.0, 84.0),
            1.0,
            &Column::RIGHT,
            &M,
            &mut out,
        );
        assert_eq!(n, 3);
        for t in &out[..n] {
            assert!(t.top >= 16.0 && t.top + TAG_H <= 84.0, "{t:?}");
        }
    }

    /// The owner's screenshot: a long with its target off the plot was invisible everywhere.
    /// Orders off the plot get a marker at their edge: the nearest one and how many.
    #[test]
    fn off_plot_orders_get_edge_markers() {
        let v = vp();
        let lines = [
            line(1, 612_000), // above (y < 0)
            line(2, 615_000), // further above
            line(3, 605_000), // on the plot
            line(4, 595_000), // below
        ];
        let m = edge_markers(&v, &lines, 400.0, 100.0, [90.0, 90.0], &Column::RIGHT, &M);
        let top = m[0].unwrap();
        assert_eq!(
            (top.id, top.price, top.n, top.above),
            (1, Price(612_000), 2, true)
        );
        assert_eq!(top.top, TAG_GAP);
        assert_eq!(top.left + top.w, 400.0 - TAG_GAP);
        let bottom = m[1].unwrap();
        assert_eq!((bottom.id, bottom.n, bottom.above), (4, 1, false));
        assert_eq!(bottom.top + TAG_H, 100.0 - TAG_GAP);
        // Tags keep clear of the markers.
        let (lo, hi) = tag_band(&m, 100.0, &Column::RIGHT, &M);
        assert!(lo >= top.top + TAG_H && hi <= bottom.top);
        assert_eq!(
            edge_markers(
                &v,
                &lines[2..3],
                400.0,
                100.0,
                [90.0; 2],
                &Column::RIGHT,
                &M
            ),
            [None, None]
        );
        // Clicking a marker centres its price, keeping the span.
        let (lo, hi) = centre_on(&v, Price(612_000));
        assert_eq!(((lo + hi) / 2.0, hi - lo), (612_000.0, 10_000.0));
    }

    #[test]
    fn hits_tell_handles_markers_close_and_body_apart() {
        let v = vp();
        let mut out = [TagBox::default(); 2];
        let n = layout_tags(
            &v,
            &[line(9, 605_000)],
            400.0,
            100.0,
            FULL,
            1.0,
            &Column::RIGHT,
            &M,
            &mut out,
        );
        let t = out[0];
        let y = t.top + TAG_H * 0.5;
        let none: [Option<EdgeMarker>; 2] = [None, None];
        let no_h: [Option<Handle>; 2] = [None, None];
        assert_eq!(
            hit(&out[..n], &none, &no_h, t.left + 2.0, y),
            Some(TagHit::Body(9))
        );
        assert_eq!(
            hit(&out[..n], &none, &no_h, t.close_left + 2.0, y),
            Some(TagHit::Close(9))
        );
        assert_eq!(hit(&out[..n], &none, &no_h, t.left - 1.0, y), None);
        assert_eq!(
            hit(&out[..n], &none, &no_h, 10.0, t.line_y),
            None,
            "lines aren't targets"
        );
        let h = handles(&t, [true, true], &M);
        let up = h[0].unwrap();
        assert_eq!(
            hit(&out[..n], &none, &h, up.left + 1.0, up.top + 1.0),
            Some(TagHit::Handle(true))
        );
        let down = h[1].unwrap();
        assert_eq!(
            hit(&out[..n], &none, &h, down.left + 1.0, down.top + 1.0),
            Some(TagHit::Handle(false))
        );
        let m = edge_markers(
            &v,
            &[line(5, 612_000)],
            400.0,
            100.0,
            [60.0, 60.0],
            &Column::RIGHT,
            &M,
        );
        let mk = m[0].unwrap();
        assert_eq!(
            hit(&out[..n], &m, &no_h, mk.left + 1.0, mk.top + 1.0),
            Some(TagHit::Edge(Price(612_000)))
        );
    }

    /// Long: above is the target, below the stop; short: the other way. Handles show only for
    /// an exit the position doesn't have yet.
    #[test]
    fn handles_follow_the_position_side() {
        assert!(handle_is_target(2, true) && !handle_is_target(2, false));
        assert!(!handle_is_target(-1, true) && handle_is_target(-1, false));
        assert_eq!(wanted_handles(2, false, false), [true, true]);
        assert_eq!(
            wanted_handles(2, false, true),
            [true, false],
            "long with a stop: target only"
        );
        assert_eq!(
            wanted_handles(-1, true, false),
            [true, false],
            "short with a target: stop above"
        );
        assert_eq!(wanted_handles(0, false, false), [false, false]);
    }

    /// Phase 2: dragging the position line up from a long's average sets a target, down a
    /// stop; a short the other way round; back at the average, nothing.
    #[test]
    fn position_line_drag_picks_the_exit_by_direction() {
        let avg = 581_000.0;
        assert_eq!(position_drag_exit(2, avg, Price(581_500)), Some(true));
        assert_eq!(position_drag_exit(2, avg, Price(580_500)), Some(false));
        assert_eq!(position_drag_exit(-1, avg, Price(580_500)), Some(true));
        assert_eq!(position_drag_exit(-1, avg, Price(581_500)), Some(false));
        assert_eq!(position_drag_exit(2, avg, Price(581_000)), None);
        assert_eq!(position_drag_exit(0, avg, Price(581_500)), None);
        // Review: a fractional average (fills at 100 and 101) never rounds a valid exit away.
        assert_eq!(position_drag_exit(2, 100.5, Price(101)), Some(true));
        assert_eq!(position_drag_exit(2, 100.5, Price(100)), Some(false));
    }

    #[test]
    fn drags_snap_like_the_crosshair() {
        let v = vp();
        let y = v.price_to_y(605_010.0);
        assert_eq!(snap_price(&v, y, 25), Some(Price(605_000)));
        let y = v.price_to_y(605_013.0);
        assert_eq!(snap_price(&v, y, 25), Some(Price(605_025)));
    }

    fn grp(id: u64, price: i64, qty: i64, key: u16) -> GroupIn {
        GroupIn {
            id,
            price: Price(price),
            qty,
            key,
        }
    }

    /// A viewport with `per_unit` px per price unit, the plot 400 px tall around 582_000.
    fn zoom(per_unit: f64) -> Viewport {
        let mut vp = Viewport::default();
        vp.set_size(800.0, 400.0);
        vp.price_lo = 582_000.0 - 200.0 / per_unit;
        vp.price_hi = 582_000.0 + 200.0 / per_unit;
        vp
    }

    /// Tags that can't sit at their own levels collapse; zooming in splits them back; kinds
    /// never mix; every order stays in exactly one tag.
    #[test]
    fn orders_too_close_share_a_tag_and_zooming_in_splits_them() {
        let orders = [
            grp(1, 582_275, 10, 1),
            grp(2, 582_300, 10, 1),
            grp(3, 582_275, 5, 1),
            grp(4, 582_300, 3, 2),
        ];
        let mut out = [TagGroup::default(); MAX_TAGS];
        let mut a = orders;
        // 25 units apart at 0.1 px per unit: 2.5 px, they can't both fit.
        let n = group_orders(&zoom(0.1), &mut a, 400.0, MAX_TAGS, &M, &mut out);
        assert_eq!(n, 2, "{out:?}");
        assert_eq!((out[0].key, out[0].n, out[0].qty), (1, 3, 25));
        assert_eq!((out[0].lo, out[0].hi), (Price(582_275), Price(582_300)));
        assert_eq!(
            (out[1].key, out[1].n),
            (2, 1),
            "another kind keeps its own tag"
        );
        // 25 units apart at 2 px per unit: 50 px: their own levels, but the equal prices stay.
        let mut b = orders;
        let n = group_orders(&zoom(2.0), &mut b, 400.0, MAX_TAGS, &M, &mut out);
        assert_eq!(n, 3, "{out:?}");
        assert_eq!((out[0].hi, out[0].n), (Price(582_300), 1));
        assert_eq!((out[1].hi, out[1].n, out[1].qty), (Price(582_275), 2, 15));
        let total: usize = out[..n].iter().map(|g| g.n).sum();
        assert_eq!(total, orders.len(), "every order in exactly one tag");
    }

    /// The average is exact until the label: integers, rounded half away from zero only there.
    #[test]
    fn the_average_is_exact_and_rounded_only_for_the_label() {
        let mut orders = [grp(1, 582_275, 1, 1), grp(2, 582_300, 2, 1)];
        let mut out = [TagGroup::default(); 4];
        let n = group_orders(&zoom(0.01), &mut orders, 400.0, 4, &M, &mut out);
        assert_eq!(n, 1);
        assert_eq!(out[0].notional, 582_275 + 2 * 582_300);
        assert_eq!(out[0].average(), Price(582_292), "582291.67 rounds up");
        assert_eq!(
            round_average(5, 2),
            Some(Price(3)),
            "half rounds away from zero"
        );
        assert_eq!(round_average(-5, 2), Some(Price(-3)));
        assert_eq!(round_average(7, 0), None);
        // Past i64 × i64: no overflow on the way.
        let big = TagGroup {
            notional: i64::MAX as i128 * 1_000,
            qty: 1_000,
            hi: Price(1),
            ..TagGroup::default()
        };
        assert_eq!(big.average(), Price(i64::MAX));
    }

    /// Too many kinds or tags for the band: the distance grows, then the last tag takes the
    /// rest; never more tags than fit, and no order left out.
    #[test]
    fn nothing_is_dropped_when_the_band_is_short() {
        let mut out = [TagGroup::default(); MAX_TAGS];
        // 70 orders of one kind, 2 px apart at this zoom... spread enough to need growth.
        let mut many: Vec<GroupIn> = (0..70)
            .map(|i| grp(i as u64 + 1, 580_000 + i * 60, 2, 1))
            .collect();
        let vp = zoom(0.09);
        let n = group_orders(&vp, &mut many, 400.0, 3, &M, &mut out);
        assert!((1..=3).contains(&n), "{n}");
        assert_eq!(out[..n].iter().map(|g| g.n).sum::<usize>(), 70);
        assert_eq!(out[..n].iter().map(|g| g.qty).sum::<i64>(), 140);
        // Four kinds on a band for 2: the second tag is the mixed one.
        let mut kinds = [
            grp(1, 582_000, 1, 1),
            grp(2, 582_000, 1, 2),
            grp(3, 582_000, 1, 3),
            grp(4, 582_000, 1, 4),
        ];
        let n = group_orders(&zoom(1.0), &mut kinds, 400.0, 2, &M, &mut out);
        assert_eq!(n, 2);
        assert_eq!((out[0].key, out[0].n), (1, 1));
        assert_eq!((out[1].key, out[1].n), (MIXED_KEY, 3));
        // The tag's orders are a contiguous run of the sorted slice.
        assert_eq!(kinds[out[1].first].key, 2);
        // No room, no orders: nothing, no panic.
        assert_eq!(
            group_orders(&zoom(1.0), &mut kinds, 400.0, 0, &M, &mut out),
            0
        );
        assert_eq!(group_orders(&zoom(1.0), &mut [], 400.0, 3, &M, &mut out), 0);
    }

    #[test]
    fn tags_fit_counts_the_band_and_the_limit() {
        assert_eq!(tags_fit((0.0, TAG_H), &M), 1);
        assert_eq!(tags_fit((0.0, 2.0 * TAG_H + TAG_GAP), &M), 2);
        assert_eq!(tags_fit((0.0, 10.0), &M), 1, "always a way in");
        assert_eq!(tags_fit((0.0, 100_000.0), &M), MAX_TAGS);
    }

    /// UI zoom: at 150 % tags, `×` boxes, markers and handles are 1.5×, a band holds fewer
    /// tags, and every hit test matches the scaled boxes (owner bug: they stayed at 100 %).
    #[test]
    fn tags_scale_with_the_ui_zoom_and_hits_follow() {
        let z = Metrics::at(1.5);
        assert_eq!(Metrics::at(1.0), Metrics::BASE);
        assert_eq!(Metrics::at(f32::NAN), Metrics::BASE);
        assert_eq!(
            Metrics::for_ui_zoom(0.8),
            Metrics::BASE,
            "never under 100 %"
        );
        assert_eq!(Metrics::for_ui_zoom(1.5), z);
        assert_eq!((z.tag_h, z.close_w, z.tag_gap), (30.0, 33.0, 3.0));
        assert_eq!((z.handle_w, z.handle_h, z.handle_gap), (39.0, 30.0, 5.0));
        // Every size is whole and the tag's height even at any zoom (crisp edges).
        for pct in 80..=150 {
            let m = Metrics::at(pct as f32 / 100.0);
            for v in [
                m.tag_h * 0.5,
                m.close_w,
                m.tag_gap,
                m.handle_w,
                m.handle_h,
                m.handle_gap,
            ] {
                assert_eq!(v, v.round(), "{pct} %: {m:?}");
            }
        }
        let v = vp();
        let mut out = [TagBox::default(); 4];
        let n = layout_tags(
            &v,
            &[line(9, 605_000)],
            400.0,
            100.0,
            FULL,
            1.0,
            &Column::RIGHT,
            &z,
            &mut out,
        );
        assert_eq!(n, 1);
        let t = out[0];
        assert_eq!(t.h, 30.0);
        assert!((t.top + 15.0 - t.line_y).abs() < 0.01, "centred");
        assert_eq!(t.w, 80.0 + 18.0 + 33.0, "text, padding and × all scaled");
        assert_eq!(t.left + t.w, 400.0 - 3.0);
        assert_eq!(t.close_left, 400.0 - 3.0 - 33.0);
        let none: [Option<EdgeMarker>; 2] = [None, None];
        let no_h: [Option<Handle>; 2] = [None, None];
        let tags = &out[..n];
        // The scaled tag's lower half is hit (a 100 % box would end at top + 20).
        assert_eq!(
            hit(tags, &none, &no_h, t.left + 1.0, t.top + 25.0),
            Some(TagHit::Body(9))
        );
        assert_eq!(hit(tags, &none, &no_h, t.left + 1.0, t.top + 30.0), None);
        // The × is 33 px: 30 px left of the tag's right end is still the ×.
        assert_eq!(
            hit(tags, &none, &no_h, t.left + t.w - 30.0, t.top + 1.0),
            Some(TagHit::Close(9))
        );
        assert_eq!(
            hit(tags, &none, &no_h, t.close_left - 0.5, t.top + 1.0),
            Some(TagHit::Body(9))
        );
        // Handles: 39 × 30, 5 px off the tag.
        let h = handles(&t, [true, true], &z);
        let (up, down) = (h[0].unwrap(), h[1].unwrap());
        assert_eq!((up.w, up.h, up.top), (39.0, 30.0, t.top - 5.0 - 30.0));
        assert_eq!(down.top, t.top + 30.0 + 5.0);
        assert_eq!(
            hit(tags, &none, &h, up.left + 38.0, up.top + 29.0),
            Some(TagHit::Handle(true))
        );
        assert_eq!(
            hit(tags, &none, &h, down.left + 38.0, down.top + 29.0),
            Some(TagHit::Handle(false))
        );
        // Edge markers: 30 px tall, padding scaled, the band leaves room for them.
        let m = edge_markers(
            &v,
            &[line(5, 612_000)],
            400.0,
            100.0,
            [60.0, 60.0],
            &Column::RIGHT,
            &z,
        );
        let mk = m[0].unwrap();
        assert_eq!((mk.top, mk.h, mk.w), (3.0, 30.0, 60.0 + 18.0));
        assert_eq!(
            hit(tags, &m, &no_h, mk.left + 1.0, mk.top + 29.0),
            Some(TagHit::Edge(Price(612_000)))
        );
        assert_eq!(tag_band(&m, 100.0, &Column::RIGHT, &z).0, 3.0 + 30.0 + 3.0);
        // Fewer tags per band; orders group sooner.
        assert_eq!(tags_fit((0.0, 100.0), &M), 4);
        assert_eq!(tags_fit((0.0, 100.0), &z), 3);
        let mut orders = [grp(1, 582_000, 1, 1), grp(2, 582_025, 1, 1)];
        let mut g = [TagGroup::default(); 4];
        // 25 units at 1 px a unit: room at 100 % (22 px apart), not at 150 % (33).
        assert_eq!(
            group_orders(&zoom(1.0), &mut orders, 400.0, 4, &M, &mut g),
            2
        );
        assert_eq!(
            group_orders(&zoom(1.0), &mut orders, 400.0, 4, &z, &mut g),
            1
        );
    }

    fn left(top_clear: f32, taken: Option<(f32, f32)>) -> Column {
        Column {
            side: TagSide::Left,
            top_clear,
            taken,
            taken_right: 120.0,
        }
    }

    #[test]
    fn left_tags_sit_in_the_position_tags_column_mirrored() {
        let v = vp();
        let mut out = [TagBox::default(); 4];
        let col = left(0.0, None);
        let n = layout_tags(
            &v,
            &[line(7, 605_000)],
            400.0,
            100.0,
            FULL,
            1.0,
            &col,
            &M,
            &mut out,
        );
        assert_eq!(n, 1);
        let t = out[0];
        assert!(t.mirrored);
        assert_eq!(t.left, Column::left_x(&M), "the position tag's x");
        assert!(
            (t.top + TAG_H * 0.5 - t.line_y).abs() < 0.01,
            "centred on its line"
        );
        // × at the outer (left) end, the text after it.
        assert_eq!((t.close_left, t.close_w), (t.left, CLOSE_W));
        assert_eq!(t.body_left(), t.left + CLOSE_W);
        assert_eq!(t.divider_x(), t.left + CLOSE_W);
        assert_eq!(t.inner_x(), t.left + t.w);
        // The right side is unchanged: × at the right end.
        let n = layout_tags(
            &v,
            &[line(7, 605_000)],
            400.0,
            100.0,
            FULL,
            1.0,
            &Column::RIGHT,
            &M,
            &mut out,
        );
        assert_eq!(n, 1);
        let r = out[0];
        assert!(!r.mirrored);
        assert_eq!(
            (r.close_left + r.close_w, r.body_left(), r.inner_x()),
            (400.0 - TAG_GAP, r.left, r.left)
        );
    }

    #[test]
    fn left_tags_hit_their_close_box_at_the_left_end() {
        let v = vp();
        let mut out = [TagBox::default(); 4];
        let col = left(0.0, None);
        let n = layout_tags(
            &v,
            &[line(9, 605_000)],
            400.0,
            100.0,
            FULL,
            1.0,
            &col,
            &M,
            &mut out,
        );
        let t = out[0];
        let y = t.top + TAG_H * 0.5;
        let none: [Option<EdgeMarker>; 2] = [None, None];
        let no_h: [Option<Handle>; 2] = [None, None];
        assert_eq!(
            hit(&out[..n], &none, &no_h, t.left + 2.0, y),
            Some(TagHit::Close(9))
        );
        assert_eq!(
            hit(&out[..n], &none, &no_h, t.left + CLOSE_W - 0.5, y),
            Some(TagHit::Close(9))
        );
        assert_eq!(
            hit(&out[..n], &none, &no_h, t.left + CLOSE_W, y),
            Some(TagHit::Body(9)),
            "the body starts after ×"
        );
        assert_eq!(
            hit(&out[..n], &none, &no_h, t.left + t.w - 1.0, y),
            Some(TagHit::Body(9)),
            "the right end drags"
        );
        assert_eq!(hit(&out[..n], &none, &no_h, t.left + t.w, y), None);
        // A mirrored tag's handles sit at its inner (right) end.
        let h = handles(&t, [true, true], &M);
        let up = h[0].unwrap();
        assert_eq!(up.left + up.w, t.left + t.w);
        assert_eq!(
            hit(&out[..n], &none, &h, up.left + 1.0, up.top + 1.0),
            Some(TagHit::Handle(true))
        );
    }

    #[test]
    fn left_tags_stay_below_the_legend_and_off_the_position_tag() {
        let v = vp();
        let mut out = [TagBox::default(); 8];
        // The legend's block ends at 20; the position tag (with its handles) takes 40..62.
        let col = left(20.0, Some((40.0, 62.0)));
        // Lines at y 10 (under the legend), 45 and 50 (on the position tag), and 80.
        let lines = [
            line(1, 609_000),
            line(2, 605_500),
            line(3, 605_000),
            line(4, 602_000),
        ];
        let markers = edge_markers(&v, &lines, 400.0, 100.0, [60.0; 2], &col, &M);
        let band = tag_band(&markers, 100.0, &col, &M);
        assert_eq!(band.0, 20.0 + TAG_GAP, "below the legend");
        let n = layout_tags(&v, &lines, 400.0, 100.0, band, 1.0, &col, &M, &mut out);
        assert!(n >= 1);
        let tags = &out[..n];
        for (i, t) in tags.iter().enumerate() {
            assert!(t.top >= band.0, "{i}: under the legend");
            assert!(t.top + t.h <= band.1, "{i}: inside the band");
            assert!(
                t.top + t.h <= 40.0 || t.top >= 62.0,
                "{i}: off the position tag: {}",
                t.top
            );
            if i > 0 {
                assert!(
                    t.top >= tags[i - 1].top + TAG_H + TAG_GAP - 0.01,
                    "{i}: no overlap"
                );
            }
        }
        // A line above the position tag's middle steps above it; one below steps below.
        // (No legend here: under one ending at 20, the 18 px above the position tag hold none.)
        let open = left(0.0, Some((40.0, 62.0)));
        let near = [line(5, 605_600), line(6, 604_600)];
        let n = layout_tags(&v, &near, 400.0, 100.0, FULL, 1.0, &open, &M, &mut out);
        assert_eq!(n, 2);
        assert_eq!(out[0].top + TAG_H, 40.0 - TAG_GAP, "above the position tag");
        assert_eq!(out[1].top, 62.0 + TAG_GAP, "below it");
        // The column holds fewer tags: the position tag's slots are taken.
        assert!(tags_fit_in(band, &col, &M) < tags_fit(band, &M));
        assert_eq!(
            tags_fit_in(band, &Column::RIGHT, &M),
            tags_fit(band, &M),
            "the right column has nothing taken"
        );
    }

    #[test]
    fn left_edge_markers_sit_at_the_left_below_the_legend() {
        let v = vp();
        let col = left(20.0, None);
        let m = edge_markers(
            &v,
            &[line(5, 612_000), line(6, 590_000)],
            400.0,
            100.0,
            [60.0; 2],
            &col,
            &M,
        );
        let top = m[0].unwrap();
        let bottom = m[1].unwrap();
        assert_eq!(
            (top.left, bottom.left),
            (Column::left_x(&M), Column::left_x(&M))
        );
        assert_eq!(top.top, 20.0 + TAG_GAP, "below the legend");
        assert_eq!(bottom.top, 100.0 - TAG_GAP - TAG_H);
        assert_eq!(tag_band(&m, 100.0, &col, &M).0, top.top + TAG_H + TAG_GAP);
        // A marker that would cover the position tag sits right of it instead.
        let busy = left(20.0, Some((15.0, 40.0)));
        let m = edge_markers(&v, &[line(5, 612_000)], 400.0, 100.0, [60.0; 2], &busy, &M);
        assert_eq!(m[0].unwrap().left, 120.0 + TAG_GAP);
        assert_eq!(
            busy.left_x_at(60.0, TAG_H, &M),
            Column::left_x(&M),
            "clear of it"
        );
    }

    #[test]
    fn the_left_column_counts_the_room_around_the_position_tag() {
        // 22 px slots: above 22..65 holds 1, below 131..152 holds none.
        let col = left(20.0, Some((65.0, 131.0)));
        assert_eq!(tags_fit_in((22.0, 152.0), &col, &M), 1);
        // Never none: one tag, at worst, on a crowded plot.
        let full = left(0.0, Some((0.0, 100.0)));
        assert_eq!(tags_fit_in(FULL, &full, &M), 1);
    }
}
