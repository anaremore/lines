//! Resizable table columns (DOM, time & sales): pure, allocation-free layout and edits.
//!
//! A table is a fixed list of columns ([`ColumnSpec`]s, at most [`MAX_COLS`]); a
//! [`ColumnSet`] holds the trader's choices: an explicit width per column (`None` = the
//! default) and whether it is shown. Each frame the pane measures what its columns must hold
//! (`needed`: the widest current text plus padding) and calls [`ColumnSet::layout`], which
//! never makes a column narrower than that: numbers are never truncated (docs/design.md §4),
//! a column grows when its content does. One column (the *fill* column, or the last shown one
//! if it is hidden) takes the space that is left. Everything here is plain arithmetic on fixed
//! arrays, so it can run in paint and during live updates without allocating.
//!
//! Dividers sit at the right edge of every shown column but the last, and follow the pointer.
//! Columns on the far side of the fill column don't move when one is resized, so a divider
//! is *anchored* on the side away from the fill column: left of it, a divider is the right
//! edge of a left-anchored column (that column is resized, growing as the pointer moves
//! right); right of it, the left edge of a right-anchored column (that column is resized,
//! shrinking as the pointer moves right). A column grows only as far as the fill column can
//! give way (down to its own minimum), so a divider stops at a limit rather than pushing
//! columns past the table's edge.

/// Most columns a table has.
pub const MAX_COLS: usize = 8;

/// Widest a column can be dragged.
pub const MAX_WIDTH: f32 = 800.0;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ColumnSpec {
    /// Never narrower than this (logical px), whatever the content.
    pub min_w: f32,
    pub default_w: f32,
    /// The price column can't be hidden.
    pub hideable: bool,
    pub shown_by_default: bool,
}

/// One column's place in a frame (x relative to the table's left edge).
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Cell {
    pub x: f32,
    pub w: f32,
    pub shown: bool,
}

impl Cell {
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ColumnSet {
    n: usize,
    fill: usize,
    width: [Option<f32>; MAX_COLS],
    shown: [bool; MAX_COLS],
}

/// A width from a file or a drag: finite, positive, capped.
fn sane(w: f32) -> Option<f32> {
    (w.is_finite() && w > 0.0).then(|| w.min(MAX_WIDTH))
}

impl ColumnSet {
    /// Defaults for `specs`; column `fill` takes the leftover space.
    pub fn new(specs: &[ColumnSpec], fill: usize) -> ColumnSet {
        assert!(specs.len() <= MAX_COLS && fill < specs.len());
        let mut shown = [false; MAX_COLS];
        for (i, s) in specs.iter().enumerate() {
            shown[i] = s.shown_by_default;
        }
        ColumnSet {
            n: specs.len(),
            fill,
            width: [None; MAX_COLS],
            shown,
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    pub fn is_shown(&self, i: usize) -> bool {
        i < self.n && self.shown[i]
    }

    /// The explicit width, if the trader set one.
    pub fn width(&self, i: usize) -> Option<f32> {
        self.width.get(i).copied().flatten()
    }

    /// Whether everything is at its default (nothing to save).
    pub fn is_default(&self, specs: &[ColumnSpec]) -> bool {
        (0..self.n).all(|i| self.width[i].is_none() && self.shown[i] == specs[i].shown_by_default)
    }

    pub fn reset(&mut self, specs: &[ColumnSpec]) {
        *self = ColumnSet::new(specs, self.fill);
    }

    /// Restore saved choices (unusable widths are ignored; a column that can't be hidden
    /// stays shown).
    pub fn restore(
        &mut self,
        specs: &[ColumnSpec],
        i: usize,
        width: Option<f32>,
        shown: Option<bool>,
    ) {
        if i >= self.n {
            return;
        }
        self.width[i] = width.and_then(sane).map(|w| w.max(specs[i].min_w));
        if let Some(s) = shown {
            self.shown[i] = s || !specs[i].hideable;
        }
    }

    /// Show or hide column `i`. `false` if it can't be hidden (the price column, or the last
    /// shown column).
    pub fn set_shown(&mut self, specs: &[ColumnSpec], i: usize, shown: bool) -> bool {
        if i >= self.n {
            return false;
        }
        if !shown {
            let others = (0..self.n).filter(|&j| j != i && self.shown[j]).count();
            if !specs[i].hideable || others == 0 {
                return false;
            }
        }
        self.shown[i] = shown;
        true
    }

    /// The column that takes the leftover space: the fill column, else the last shown one.
    pub fn fill_index(&self) -> Option<usize> {
        if self.shown[self.fill] {
            Some(self.fill)
        } else {
            (0..self.n).rev().find(|&i| self.shown[i])
        }
    }

    /// Column `i`'s width before the leftover is added: its set (or default) width, never
    /// less than its minimum or what its content needs.
    pub fn base_width(&self, specs: &[ColumnSpec], i: usize, needed: &[f32; MAX_COLS]) -> f32 {
        self.width[i]
            .unwrap_or(specs[i].default_w)
            .max(specs[i].min_w)
            .max(needed[i])
    }

    /// Place the shown columns in a table `total` px wide. Columns past the right edge are
    /// still placed (the pane clips them); none is squeezed below what its content needs.
    pub fn layout(
        &self,
        specs: &[ColumnSpec],
        needed: &[f32; MAX_COLS],
        total: f32,
        out: &mut [Cell; MAX_COLS],
    ) {
        let fill = self.fill_index();
        let fixed: f32 = (0..self.n)
            .filter(|&i| self.shown[i] && Some(i) != fill)
            .map(|i| self.base_width(specs, i, needed))
            .sum();
        let mut x = 0.0;
        for (i, cell) in out.iter_mut().enumerate() {
            if i >= self.n || !self.shown[i] {
                *cell = Cell {
                    x,
                    w: 0.0,
                    shown: false,
                };
                continue;
            }
            let base = self.base_width(specs, i, needed);
            let w = if Some(i) == fill {
                base.max(total - fixed)
            } else {
                base
            };
            *cell = Cell { x, w, shown: true };
            x += w;
        }
    }

    /// The divider within `slop` px of `x` (table coordinates): the index of the shown column
    /// whose right edge it is. The last shown column has none.
    pub fn divider_at(&self, cells: &[Cell; MAX_COLS], x: f32, slop: f32) -> Option<usize> {
        let last = (0..self.n).rev().find(|&i| self.shown[i])?;
        (0..self.n)
            .filter(|&i| self.shown[i] && i != last)
            .find(|&i| (cells[i].right() - x).abs() <= slop)
    }

    /// The next shown column after `i`.
    fn next_shown(&self, i: usize) -> Option<usize> {
        (i + 1..self.n).find(|&j| self.shown[j])
    }

    /// The column a drag (or double-click) on divider `d` resizes, and whether it grows with
    /// the pointer moving right (`true`) or left: the column on the divider's anchored side
    /// (see the module docs), so the divider follows the pointer.
    pub fn divider_column(&self, d: usize) -> Option<(usize, bool)> {
        let right = self.next_shown(d)?;
        if !self.is_shown(d) {
            return None;
        }
        if self.fill_index()? <= d {
            // Right of the fill column: right-anchored columns; resize the right neighbour.
            Some((right, false))
        } else {
            // Left of the fill column: left-anchored columns; resize the left one.
            Some((d, true))
        }
    }

    /// The widest column `i` may become in a drag: what it has plus what the fill column can
    /// give up without going below its own width (minimum or content). Past this the divider
    /// would stop following the pointer and push columns off the table.
    pub fn grow_limit(
        &self,
        specs: &[ColumnSpec],
        cells: &[Cell; MAX_COLS],
        i: usize,
        needed: &[f32; MAX_COLS],
    ) -> f32 {
        match self.fill_index() {
            Some(f) if f != i && i < self.n => {
                let slack = (cells[f].w - self.base_width(specs, f, needed)).max(0.0);
                (cells[i].w + slack).min(MAX_WIDTH)
            }
            _ => MAX_WIDTH,
        }
    }

    /// Set column `i`'s width, clamped to `[max(min, needed), MAX_WIDTH]`. Returns the width
    /// set.
    pub fn set_width(
        &mut self,
        specs: &[ColumnSpec],
        i: usize,
        w: f32,
        needed: &[f32; MAX_COLS],
    ) -> f32 {
        if i >= self.n {
            return 0.0;
        }
        let lo = specs[i].min_w.max(needed[i]);
        let w = if w.is_finite() { w } else { lo };
        let w = w.clamp(lo, MAX_WIDTH.max(lo));
        self.width[i] = Some(w);
        w
    }

    /// Drag divider `d` by `dx` px (from where it was in `cells`). Returns the column resized
    /// and its new width.
    pub fn drag(
        &mut self,
        specs: &[ColumnSpec],
        cells: &[Cell; MAX_COLS],
        d: usize,
        dx: f32,
        needed: &[f32; MAX_COLS],
    ) -> Option<(usize, f32)> {
        let (i, grows) = self.divider_column(d)?;
        let w = cells[i].w + if grows { dx } else { -dx };
        let w = w.min(self.grow_limit(specs, cells, i, needed));
        Some((i, self.set_width(specs, i, w, needed)))
    }

    /// Fit column `i` to its widest current content.
    pub fn auto_fit(&mut self, specs: &[ColumnSpec], i: usize, needed: &[f32; MAX_COLS]) -> f32 {
        let fit = needed[i].max(specs[i].min_w);
        self.set_width(specs, i, fit, needed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Like the DOM: orders (hidden), bid, price (always shown), ask, volume (fill).
    fn specs() -> [ColumnSpec; 5] {
        let c = |min_w, default_w, hideable, shown| ColumnSpec {
            min_w,
            default_w,
            hideable,
            shown_by_default: shown,
        };
        [
            c(16.0, 22.0, true, false),
            c(36.0, 64.0, true, true),
            c(48.0, 76.0, false, true),
            c(36.0, 64.0, true, true),
            c(36.0, 64.0, true, true),
        ]
    }

    fn needed(v: &[f32]) -> [f32; MAX_COLS] {
        let mut n = [0.0; MAX_COLS];
        n[..v.len()].copy_from_slice(v);
        n
    }

    fn cells(set: &ColumnSet, need: &[f32; MAX_COLS], total: f32) -> [Cell; MAX_COLS] {
        let mut out = [Cell::default(); MAX_COLS];
        set.layout(&specs(), need, total, &mut out);
        out
    }

    #[test]
    fn defaults_and_fill() {
        let set = ColumnSet::new(&specs(), 4);
        let c = cells(&set, &needed(&[]), 400.0);
        assert!(!c[0].shown, "orders hidden by default");
        assert_eq!((c[1].x, c[1].w), (0.0, 64.0));
        assert_eq!((c[2].x, c[2].w), (64.0, 76.0));
        assert_eq!((c[3].x, c[3].w), (140.0, 64.0));
        assert_eq!((c[4].x, c[4].w), (204.0, 196.0), "volume fills the rest");
        // Narrower than the columns: nothing is squeezed (the pane clips).
        let c = cells(&set, &needed(&[]), 200.0);
        assert_eq!(c[4].w, 64.0);
        assert!(set.is_default(&specs()));
    }

    #[test]
    fn content_is_never_truncated() {
        let mut set = ColumnSet::new(&specs(), 4);
        // A price that needs 90 px widens the column beyond its 76 px default...
        let need = needed(&[0.0, 0.0, 90.0, 0.0, 0.0]);
        assert_eq!(cells(&set, &need, 400.0)[2].w, 90.0);
        // ...and a set width can't go below it, nor below the minimum.
        assert_eq!(set.set_width(&specs(), 2, 10.0, &need), 90.0);
        assert_eq!(set.set_width(&specs(), 1, 5.0, &needed(&[])), 36.0);
        assert_eq!(set.set_width(&specs(), 1, 5_000.0, &needed(&[])), MAX_WIDTH);
        assert_eq!(set.set_width(&specs(), 1, f32::NAN, &needed(&[])), 36.0);
    }

    #[test]
    fn dragging_dividers() {
        let mut set = ColumnSet::new(&specs(), 4);
        let need = needed(&[]);
        let c = cells(&set, &need, 400.0);
        // The bid|price divider at x = 64 resizes bid.
        assert_eq!(set.divider_at(&c, 66.0, 3.0), Some(1));
        assert_eq!(set.drag(&specs(), &c, 1, 20.0, &need), Some((1, 84.0)));
        let c = cells(&set, &need, 400.0);
        assert_eq!((c[1].w, c[2].x), (84.0, 84.0));
        assert_eq!(
            c[4].w,
            400.0 - 84.0 - 76.0 - 64.0,
            "the fill column gives way"
        );
        // Dragged past the minimum: clamped.
        assert_eq!(set.drag(&specs(), &c, 1, -500.0, &need), Some((1, 36.0)));
        // No divider after the last column, or far from any edge.
        let c = cells(&set, &need, 400.0);
        assert_eq!(set.divider_at(&c, 399.0, 3.0), None);
        assert_eq!(set.divider_at(&c, 20.0, 3.0), None);
    }

    #[test]
    fn a_fill_column_on_the_left_resizes_its_neighbour() {
        // Like time & sales: time (fill), price, size, side.
        let t = [
            ColumnSpec {
                min_w: 72.0,
                default_w: 96.0,
                hideable: true,
                shown_by_default: true,
            },
            ColumnSpec {
                min_w: 48.0,
                default_w: 76.0,
                hideable: false,
                shown_by_default: true,
            },
        ];
        let mut set = ColumnSet::new(&t, 0);
        let need = needed(&[]);
        let mut c = [Cell::default(); MAX_COLS];
        set.layout(&t, &need, 300.0, &mut c);
        assert_eq!((c[0].w, c[1].x, c[1].w), (224.0, 224.0, 76.0));
        assert_eq!(set.divider_column(0), Some((1, false)));
        // Dragging the divider left 30 px widens the price column by 30.
        assert_eq!(set.drag(&t, &c, 0, -30.0, &need), Some((1, 106.0)));
        set.layout(&t, &need, 300.0, &mut c);
        assert_eq!((c[0].w, c[1].w), (194.0, 106.0));
    }

    /// Like time & sales: time (fill, on the left), price, size, side.
    fn tape_specs() -> [ColumnSpec; 4] {
        let c = |min_w, default_w, hideable| ColumnSpec {
            min_w,
            default_w,
            hideable,
            shown_by_default: true,
        };
        [
            c(48.0, 96.0, true),
            c(48.0, 76.0, false),
            c(28.0, 52.0, true),
            c(16.0, 40.0, true),
        ]
    }

    /// Drag every divider of `set` both ways by `dx` from a fresh layout `total` px wide and
    /// check the divider ends under the pointer.
    fn every_divider_follows(specs: &[ColumnSpec], set: &ColumnSet, total: f32, dx: f32) {
        let need = needed(&[]);
        let mut cells = [Cell::default(); MAX_COLS];
        set.layout(specs, &need, total, &mut cells);
        let last = (0..set.len()).rev().find(|&i| set.is_shown(i)).unwrap();
        for d in (0..set.len()).filter(|&d| set.is_shown(d) && d != last) {
            for dx in [-dx, dx] {
                let mut s = *set;
                let x0 = cells[d].right();
                s.drag(specs, &cells, d, dx, &need).unwrap();
                let mut after = [Cell::default(); MAX_COLS];
                s.layout(specs, &need, total, &mut after);
                assert!(
                    (after[d].right() - (x0 + dx)).abs() < 1e-3,
                    "divider {d} dragged {dx}: at {}, pointer at {}",
                    after[d].right(),
                    x0 + dx
                );
                // Nothing pushed past the table's edge.
                assert!((after[last].right() - total).abs() < 1e-3);
            }
        }
    }

    #[test]
    fn dividers_follow_the_pointer_left_anchored() {
        // DOM-like: the fill column (volume) is last: every column is left-anchored.
        let s = specs();
        let mut set = ColumnSet::new(&s, 4);
        every_divider_follows(&s, &set, 600.0, 20.0);
        // With the orders column shown and volume hidden (ask fills, still last).
        set.set_shown(&s, 0, true);
        set.set_shown(&s, 4, false);
        every_divider_follows(&s, &set, 600.0, 4.0);
    }

    #[test]
    fn dividers_follow_the_pointer_right_anchored() {
        // T&S-like: the fill column (time) is first: every column after it is right-anchored.
        // This is the owner's bug: these dividers moved against the pointer.
        let s = tape_specs();
        let mut set = ColumnSet::new(&s, 0);
        every_divider_follows(&s, &set, 500.0, 20.0);
        // Fill in the middle (time hidden: side fills, last; then size hidden too).
        set.set_shown(&s, 0, false);
        every_divider_follows(&s, &set, 500.0, 10.0);
    }

    #[test]
    fn a_fill_in_the_middle_anchors_each_side() {
        // Fill is column 2 of 4: column 0|1 is left-anchored, 2|3 right-anchored.
        let c = |min_w, default_w| ColumnSpec {
            min_w,
            default_w,
            hideable: true,
            shown_by_default: true,
        };
        let s = [c(30.0, 80.0), c(30.0, 80.0), c(30.0, 80.0), c(30.0, 80.0)];
        let set = ColumnSet::new(&s, 2);
        assert_eq!(set.divider_column(0), Some((0, true)));
        assert_eq!(set.divider_column(1), Some((1, true)));
        assert_eq!(set.divider_column(2), Some((3, false)));
        every_divider_follows(&s, &set, 500.0, 15.0);
    }

    #[test]
    fn dragging_stops_at_a_limit_instead_of_running_away() {
        let s = tape_specs();
        let mut set = ColumnSet::new(&s, 0);
        let need = needed(&[0.0, 70.0]); // a price needs 70 px
        let mut c = [Cell::default(); MAX_COLS];
        set.layout(&s, &need, 400.0, &mut c);
        // price|size dragged far right: size shrinks to its minimum, then the divider stops.
        let (i, w) = set.drag(&s, &c, 1, 500.0, &need).unwrap();
        assert_eq!((i, w), (2, 28.0));
        // time|price dragged far right: price shrinks to what its numbers need (70), never less.
        let (i, w) = set.drag(&s, &c, 0, 500.0, &need).unwrap();
        assert_eq!((i, w), (1, 70.0));
        // price|size dragged far left: size grows only as far as time can give way (to 96).
        set.reset(&s);
        set.layout(&s, &need, 400.0, &mut c);
        let (i, w) = set.drag(&s, &c, 1, -500.0, &need).unwrap();
        assert_eq!(i, 2);
        let mut after = [Cell::default(); MAX_COLS];
        set.layout(&s, &need, 400.0, &mut after);
        assert_eq!(after[0].w, 96.0, "time at its own width");
        assert_eq!(after[3].right(), 400.0, "nothing pushed off the table");
        assert_eq!(w, c[2].w + (c[0].w - 96.0));
    }

    #[test]
    fn hide_show_and_reset() {
        let s = specs();
        let mut set = ColumnSet::new(&s, 4);
        assert!(!set.set_shown(&s, 2, false), "price can't be hidden");
        assert!(set.set_shown(&s, 4, false));
        assert!(set.set_shown(&s, 0, true));
        assert_eq!(set.fill_index(), Some(3), "the last shown column fills");
        let c = cells(&set, &needed(&[]), 400.0);
        assert!(c[0].shown && !c[4].shown);
        assert_eq!(c[3].right(), 400.0);
        set.set_width(&s, 1, 90.0, &needed(&[]));
        assert!(!set.is_default(&s));
        set.reset(&s);
        assert_eq!(set, ColumnSet::new(&s, 4));
        // Never hide the last shown column.
        let mut only = ColumnSet::new(&s, 4);
        for i in [1, 3, 4] {
            assert!(only.set_shown(&s, i, false));
        }
        assert!(!only.set_shown(&s, 2, false));
        assert!(only.is_shown(2));
    }

    #[test]
    fn auto_fit_uses_the_widest_content() {
        let s = specs();
        let mut set = ColumnSet::new(&s, 4);
        // Sizes need 41 px: the bid column (64 by default) shrinks to fit them.
        assert_eq!(set.auto_fit(&s, 1, &needed(&[0.0, 41.0])), 41.0);
        // Nothing to show yet: the minimum.
        assert_eq!(set.auto_fit(&s, 3, &needed(&[])), 36.0);
    }

    #[test]
    fn restoring_saved_choices() {
        let s = specs();
        let mut set = ColumnSet::new(&s, 4);
        set.restore(&s, 1, Some(120.0), Some(false));
        set.restore(&s, 2, Some(-3.0), Some(false)); // bad width ignored; price stays shown
        set.restore(&s, 3, Some(1.0), None); // below the minimum: the minimum
        set.restore(&s, 9, Some(50.0), None); // no such column: ignored
        assert_eq!((set.width(1), set.is_shown(1)), (Some(120.0), false));
        assert_eq!((set.width(2), set.is_shown(2)), (None, true));
        assert_eq!(set.width(3), Some(36.0));
    }
}
