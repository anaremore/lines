//! Chart order lines (roadmap 8, phase 1): where order and position tags go, which orders are
//! off the plot and where their edge markers sit, the position tag's add-stop / add-target
//! handles, and what the pointer hits. Pure and allocation-free (fixed output buffers), so the
//! chart runs it in paint (`tests/no_alloc.rs`).
//!
//! - **Lines** sit at their price's y. **Tags** (role, side, quantity, price, P&L at that price,
//!   and a `×` box) are right-aligned at the plot's right edge, centred on their line; tags that
//!   would overlap are pushed apart vertically (in price order) and kept inside the band left
//!   between the edge markers, so every tag stays readable and clickable.
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
    /// Where the `×` box starts (it runs to `left + w`).
    pub close_left: f32,
}

impl TagBox {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.left + self.w && y >= self.top && y < self.top + TAG_H
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
}

impl EdgeMarker {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.left + self.w && y >= self.top && y < self.top + TAG_H
    }
}

/// A handle on the position's tag: drag it away to place that exit.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Handle {
    /// Above the tag (higher prices) or below.
    pub above: bool,
    pub left: f32,
    pub top: f32,
}

impl Handle {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.left + HANDLE_W && y >= self.top && y < self.top + HANDLE_H
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
/// nearest off-plot order at each edge, right-aligned at `plot_w` with the width the caller
/// measured for each marker's text (`text_w`). Allocation-free.
pub fn edge_markers(
    vp: &Viewport,
    lines: &[LineIn],
    plot_w: f32,
    plot_h: f32,
    text_w: [f32; 2],
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
    for (k, m) in out.iter_mut().enumerate() {
        if let Some(m) = m {
            m.w = (text_w[k] + 12.0).min(plot_w);
            m.left = plot_w - TAG_GAP - m.w;
            m.top = if k == 0 {
                TAG_GAP
            } else {
                plot_h - TAG_GAP - TAG_H
            };
        }
    }
    out
}

/// The band tags may use, leaving room for the edge markers: `(top, bottom)`.
pub fn tag_band(markers: &[Option<EdgeMarker>; 2], plot_h: f32) -> (f32, f32) {
    let top = if markers[0].is_some() {
        TAG_GAP + TAG_H + TAG_GAP
    } else {
        0.0
    };
    let bottom = if markers[1].is_some() {
        plot_h - TAG_GAP - TAG_H - TAG_GAP
    } else {
        plot_h
    };
    (top, bottom.max(top))
}

/// Lay out the tags of `lines` on a plot `plot_w` × `plot_h`, right-aligned at `plot_w`, inside
/// the band `band` (`(top, bottom)`, from [`tag_band`]), into `out`; returns how many (at most
/// `out.len()`, [`MAX_TAGS`] and what the band holds). Every order on the plot gets a tag,
/// clamped into the band (off the plot: the edge markers stand for them). Overlapping tags are
/// pushed down in price order (highest price first), then the stack is pulled back up if it
/// ran off the bottom; it never leaves the band. Allocation-free.
pub fn layout_tags(
    vp: &Viewport,
    lines: &[LineIn],
    plot_w: f32,
    plot_h: f32,
    band: (f32, f32),
    scale_factor: f32,
    out: &mut [TagBox],
) -> usize {
    let (lo, hi) = band;
    let fits = (((hi - lo) + TAG_GAP) / (TAG_H + TAG_GAP)).floor().max(0.0) as usize;
    let cap = out.len().min(MAX_TAGS).min(fits);
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
        let w = (l.text_w + 12.0 + CLOSE_W).min(plot_w);
        out[n] = TagBox {
            id: l.id,
            price: l.price,
            line_y,
            left: plot_w - TAG_GAP - w,
            top: line_y - TAG_H * 0.5,
            w,
            close_left: plot_w - TAG_GAP - CLOSE_W,
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
            tags[i - 1].top + TAG_H + TAG_GAP
        };
        tags[i].top = tags[i].top.max(floor);
    }
    // (`cap` keeps the stack within the band, so the pull-up below never passes `lo`.)
    let mut ceiling = hi;
    for t in tags.iter_mut().rev() {
        if t.top + TAG_H > ceiling {
            t.top = ceiling - TAG_H;
        }
        ceiling = t.top - TAG_GAP;
    }
    n
}

/// The position tag's handles: `want[0]` above it, `want[1]` below, at the tag's left end.
pub fn handles(tag: &TagBox, want: [bool; 2]) -> [Option<Handle>; 2] {
    let left = tag.left;
    [
        want[0].then_some(Handle {
            above: true,
            left,
            top: tag.top - HANDLE_GAP - HANDLE_H,
        }),
        want[1].then_some(Handle {
            above: false,
            left,
            top: tag.top + TAG_H + HANDLE_GAP,
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
    Some(if x >= t.close_left {
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

/// How many tags the band holds, at most `MAX_TAGS` (what [`layout_tags`] lays out).
pub fn tags_fit(band: (f32, f32)) -> usize {
    let fits = (((band.1 - band.0) + TAG_GAP) / (TAG_H + TAG_GAP))
        .floor()
        .max(0.0) as usize;
    fits.min(MAX_TAGS)
}

/// The tags for the orders on the plot, at most `cap` (from [`tags_fit`]) and `out.len()`: every
/// order is in exactly one, so none is hidden. Orders of one kind whose tags can't both sit at
/// their own levels (`TAG_H + TAG_GAP` apart) share a tag; zooming in splits them back. When that
/// leaves too many tags for the band, the distance grows until it fits; if one tag per kind
/// still doesn't, the last tag takes everything left ([`MIXED_KEY`]). Sorts `orders` by kind,
/// then highest price first. Allocation-free.
pub fn group_orders(
    vp: &Viewport,
    orders: &mut [GroupIn],
    plot_h: f32,
    cap: usize,
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
    let mut apart = (TAG_H + TAG_GAP) as f64;
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
        let n = layout_tags(&v, &[line(7, 605_000)], 400.0, 100.0, FULL, 1.0, &mut out);
        assert_eq!(n, 1);
        let t = out[0];
        assert_eq!(t.line_y, snap(v.price_to_y(605_000.0), 1.0));
        assert!((t.top + TAG_H * 0.5 - t.line_y).abs() < 0.01, "centred");
        assert_eq!(t.left + t.w, 400.0 - TAG_GAP, "right-aligned");
        assert_eq!(t.close_left, 400.0 - TAG_GAP - CLOSE_W);
        // Off the plot: no tag.
        assert_eq!(
            layout_tags(&v, &[line(1, 620_000)], 400.0, 100.0, FULL, 1.0, &mut out),
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
        let n = layout_tags(&v, &lines, 400.0, 100.0, band, 1.0, &mut out);
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
            &mut out,
        );
        assert_eq!(n, 1, "not lost under the marker");
        assert!(out[0].top >= 16.0);
        // Twelve orders in a band that holds three: three tags, all inside.
        let many: Vec<LineIn> = (0..12).map(|i| line(i, 605_000 + i as i64 * 25)).collect();
        let n = layout_tags(&v, &many, 400.0, 100.0, (16.0, 84.0), 1.0, &mut out);
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
        let m = edge_markers(&v, &lines, 400.0, 100.0, [90.0, 90.0]);
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
        let (lo, hi) = tag_band(&m, 100.0);
        assert!(lo >= top.top + TAG_H && hi <= bottom.top);
        assert_eq!(
            edge_markers(&v, &lines[2..3], 400.0, 100.0, [90.0; 2]),
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
        let n = layout_tags(&v, &[line(9, 605_000)], 400.0, 100.0, FULL, 1.0, &mut out);
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
        let h = handles(&t, [true, true]);
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
        let m = edge_markers(&v, &[line(5, 612_000)], 400.0, 100.0, [60.0, 60.0]);
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
        let n = group_orders(&zoom(0.1), &mut a, 400.0, MAX_TAGS, &mut out);
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
        let n = group_orders(&zoom(2.0), &mut b, 400.0, MAX_TAGS, &mut out);
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
        let n = group_orders(&zoom(0.01), &mut orders, 400.0, 4, &mut out);
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
        let n = group_orders(&vp, &mut many, 400.0, 3, &mut out);
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
        let n = group_orders(&zoom(1.0), &mut kinds, 400.0, 2, &mut out);
        assert_eq!(n, 2);
        assert_eq!((out[0].key, out[0].n), (1, 1));
        assert_eq!((out[1].key, out[1].n), (MIXED_KEY, 3));
        // The tag's orders are a contiguous run of the sorted slice.
        assert_eq!(kinds[out[1].first].key, 2);
        // No room, no orders: nothing, no panic.
        assert_eq!(group_orders(&zoom(1.0), &mut kinds, 400.0, 0, &mut out), 0);
        assert_eq!(group_orders(&zoom(1.0), &mut [], 400.0, 3, &mut out), 0);
    }

    #[test]
    fn tags_fit_counts_the_band_and_the_limit() {
        assert_eq!(tags_fit((0.0, TAG_H)), 1);
        assert_eq!(tags_fit((0.0, 2.0 * TAG_H + TAG_GAP)), 2);
        assert_eq!(tags_fit((0.0, 10.0)), 0);
        assert_eq!(tags_fit((0.0, 100_000.0)), MAX_TAGS);
    }
}
