//! Chart trading geometry (roadmap 8): where order and position tags go, what the pointer
//! hits, where fill marks sit. Pure and allocation-free (fixed output buffers), so the chart
//! can run it in paint (`tests/no_alloc.rs`).
//!
//! - **Lines** sit at their price's y. **Tags** (`side type qty @ price` and a `×` box) are
//!   right-aligned at the plot's right edge, centred on their line; tags that would overlap are
//!   pushed apart vertically (in price order) and kept inside the plot, so every tag stays
//!   readable and clickable however close the orders are.
//! - **Hit-testing** is by tag: the `×` box (cancel, or flatten for the position) or the body
//!   (drag to move the order). Lines themselves are not targets: a 1 px line is too easy to
//!   catch by accident while panning.
//! - **Snapping** is the crosshair's ([`crate::geometry::snap_price`]): the tick nearest the
//!   pointer, the price the crosshair tag shows.

use crate::geometry::{slot_x, snap, time_slot};
use crate::{Bar, Price, UtcMillis, Viewport};

/// Most tags a chart lays out (orders + position); more are not drawn.
pub const MAX_TAGS: usize = 32;
/// Tag height and the `×` box's width, logical px.
pub const TAG_H: f32 = 16.0;
pub const CLOSE_W: f32 = 16.0;
/// Gap between stacked tags and between a tag and the plot's right edge.
pub const TAG_GAP: f32 = 2.0;

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

/// What the pointer is on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TagHit {
    /// The tag's body: drag to move that order.
    Body(u64),
    /// The `×`: cancel that order (or flatten, for the position's tag).
    Close(u64),
}

/// Lay out `lines`' tags in a plot `plot_w` × `plot_h`, right-aligned at `plot_w`, into `out`;
/// returns how many (at most `out.len()` and [`MAX_TAGS`]). Lines whose price is off the plot
/// get no tag. Overlapping tags are pushed down in price order (highest price first), then the
/// stack is pulled back up if it ran off the bottom. Allocation-free.
pub fn layout_tags(
    vp: &Viewport,
    lines: &[LineIn],
    plot_w: f32,
    plot_h: f32,
    scale_factor: f32,
    out: &mut [TagBox],
) -> usize {
    let cap = out.len().min(MAX_TAGS);
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
    // Push apart downwards, then pull the whole stack back inside the plot.
    for i in 0..tags.len() {
        let floor = if i == 0 {
            0.0
        } else {
            tags[i - 1].top + TAG_H + TAG_GAP
        };
        tags[i].top = tags[i].top.max(floor);
    }
    let mut ceiling = plot_h;
    for t in tags.iter_mut().rev() {
        if t.top + TAG_H > ceiling {
            t.top = ceiling - TAG_H;
        }
        ceiling = t.top - TAG_GAP;
    }
    n
}

/// The tag (and part) at plot-local (`x`, `y`), if any. The `×` box wins over the body.
pub fn hit_tag(tags: &[TagBox], x: f32, y: f32) -> Option<TagHit> {
    let t = tags.iter().find(|t| t.contains(x, y))?;
    Some(if x >= t.close_left {
        TagHit::Close(t.id)
    } else {
        TagHit::Body(t.id)
    })
}

/// Where a fill mark goes: the centre of the bar slot containing `time` and `price`'s y
/// (plot-local), or `None` off the plot or between bars.
pub fn fill_mark(
    vp: &Viewport,
    bars: &[Bar],
    period_ms: i64,
    time: UtcMillis,
    price: Price,
    plot_h: f32,
    scale_factor: f32,
) -> Option<(f32, f32)> {
    let slot = time_slot(bars, time, period_ms)?;
    let x = slot_x(vp, slot as f64, scale_factor);
    let y = vp.price_to_y(price.0 as f64);
    let inside =
        x.is_finite() && x >= 0.0 && x <= vp.width as f32 && (0.0..=plot_h as f64).contains(&y);
    inside.then(|| (x, snap(y, scale_factor)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Qty;
    use crate::geometry::snap_price;

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

    #[test]
    fn tags_sit_on_their_line_at_the_right_edge() {
        let v = vp();
        let mut out = [TagBox::default(); 4];
        let n = layout_tags(&v, &[line(7, 605_000)], 400.0, 100.0, 1.0, &mut out);
        assert_eq!(n, 1);
        let t = out[0];
        assert_eq!(t.line_y, snap(v.price_to_y(605_000.0), 1.0));
        assert!(
            (t.top + TAG_H * 0.5 - t.line_y).abs() < 0.01,
            "centred on the line"
        );
        assert_eq!(t.left + t.w, 400.0 - TAG_GAP, "right-aligned");
        assert_eq!(t.close_left, 400.0 - TAG_GAP - CLOSE_W);
        // Off the plot: no tag.
        assert_eq!(
            layout_tags(&v, &[line(1, 620_000)], 400.0, 100.0, 1.0, &mut out),
            0
        );
    }

    #[test]
    fn close_tags_never_overlap_and_stay_inside() {
        let v = vp();
        let mut out = [TagBox::default(); 8];
        // Three orders a tick apart (1 px apart on screen), plus one at the bottom edge.
        let lines = [
            line(1, 605_000),
            line(2, 605_025),
            line(3, 604_975),
            line(4, 600_000),
        ];
        let n = layout_tags(&v, &lines, 400.0, 100.0, 1.0, &mut out);
        assert_eq!(n, 4);
        let tags = &out[..n];
        for i in 0..n {
            for j in i + 1..n {
                let (a, b) = (tags[i], tags[j]);
                assert!(
                    a.top + TAG_H <= b.top || b.top + TAG_H <= a.top,
                    "{a:?} overlaps {b:?}"
                );
            }
            assert!(
                tags[i].top >= 0.0 && tags[i].top + TAG_H <= 100.0,
                "{:?}",
                tags[i]
            );
        }
        // Higher price, higher tag (price order kept).
        let top_of = |id| tags.iter().find(|t| t.id == id).unwrap().top;
        assert!(top_of(2) < top_of(1) && top_of(1) < top_of(3));
    }

    #[test]
    fn hits_tell_body_from_close() {
        let v = vp();
        let mut out = [TagBox::default(); 2];
        let n = layout_tags(&v, &[line(9, 605_000)], 400.0, 100.0, 1.0, &mut out);
        let t = out[0];
        let y = t.top + TAG_H * 0.5;
        assert_eq!(hit_tag(&out[..n], t.left + 2.0, y), Some(TagHit::Body(9)));
        assert_eq!(
            hit_tag(&out[..n], t.close_left + 2.0, y),
            Some(TagHit::Close(9))
        );
        assert_eq!(hit_tag(&out[..n], t.left - 1.0, y), None, "left of the tag");
        assert_eq!(
            hit_tag(&out[..n], t.left + 2.0, t.top - 1.0),
            None,
            "above it"
        );
        // The line itself (far left on the same y) is not a target.
        assert_eq!(hit_tag(&out[..n], 10.0, t.line_y), None);
    }

    #[test]
    fn chart_orders_snap_like_the_crosshair() {
        let v = vp();
        let tick = 25;
        // The pointer between ticks: the nearest tick, as the crosshair's price tag shows.
        let y = v.price_to_y(605_010.0);
        assert_eq!(snap_price(&v, y, tick), Some(Price(605_000)));
        let y = v.price_to_y(605_013.0);
        assert_eq!(snap_price(&v, y, tick), Some(Price(605_025)));
    }

    #[test]
    fn fill_marks_sit_on_their_bar_and_price() {
        let mut v = vp();
        v.bar_px = 10.0;
        let bars: Vec<Bar> = (0..5)
            .map(|i| Bar {
                time: UtcMillis(i * 60_000),
                open: Price(605_000),
                high: Price(605_100),
                low: Price(604_900),
                close: Price(605_000),
                volume: Qty(1),
            })
            .collect();
        v.right_index = 4.0;
        let at = fill_mark(
            &v,
            &bars,
            60_000,
            UtcMillis(2 * 60_000 + 5_000),
            Price(605_000),
            100.0,
            1.0,
        );
        let (x, y) = at.expect("on the plot");
        assert_eq!(x, slot_x(&v, 2.0, 1.0));
        assert_eq!(y, snap(v.price_to_y(605_000.0), 1.0));
        assert_eq!(
            fill_mark(&v, &bars, 60_000, UtcMillis(0), Price(700_000), 100.0, 1.0),
            None,
            "off the plot"
        );
    }
}
