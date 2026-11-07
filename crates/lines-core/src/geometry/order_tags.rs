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

use super::snap;
use crate::{Price, Viewport};

/// Most tags a chart lays out (orders + position); more are not drawn.
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

    #[test]
    fn drags_snap_like_the_crosshair() {
        let v = vp();
        let y = v.price_to_y(605_010.0);
        assert_eq!(snap_price(&v, y, 25), Some(Price(605_000)));
        let y = v.price_to_y(605_013.0);
        assert_eq!(snap_price(&v, y, 25), Some(Price(605_025)));
    }
}
