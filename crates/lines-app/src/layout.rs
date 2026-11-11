//! A tab's layout as a value: a split tree of panes (decision 0016).
//!
//! [`Layout<T>`] is either one leaf or a split of two layouts along an axis at a ratio. The
//! live workspace uses `Layout<PaneId>` (panes live elsewhere, keyed by id); the saved file
//! uses `Layout<PaneDoc>` (each leaf describes its pane). Everything here is pure geometry and
//! tree surgery, unit-tested: placing leaves in a rectangle, splitting and closing a leaf,
//! dragging a divider, moving focus to the neighbour in a direction, and the presets.
//!
//! The workspace renders exactly the rectangles [`Layout::place`] computes, so hit-testing
//! (dividers, focus) and drawing can never disagree.

use serde::{Deserialize, Serialize};

/// What kind of pane a leaf holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneKind {
    Chart,
    Dom,
    Tape,
    /// The trade journal (roadmap 8c; workspace format 3).
    Journal,
    /// The strategy panel (decision 0025; workspace format 4).
    Strategies,
    /// An underlying's option chain (decision 0028; workspace format 6).
    Chain,
    /// Every open position on every account (workspace format 8).
    Positions,
}

impl PaneKind {
    pub fn label(self) -> &'static str {
        match self {
            PaneKind::Chart => "Chart",
            PaneKind::Dom => "DOM",
            PaneKind::Tape => "Time & sales",
            PaneKind::Journal => "Journal",
            PaneKind::Strategies => "Strategies",
            PaneKind::Chain => "Option chain",
            PaneKind::Positions => "Positions",
        }
    }
}

/// Panes sharing a link group switch symbol together (`None` = unlinked). Groups are
/// `0..LINK_GROUPS`, shown as A, B, C, D with a colour each.
pub type LinkGroup = Option<u8>;

pub const LINK_GROUPS: u8 = 4;

/// "A".."D", or "-" for unlinked.
pub fn link_letter(link: LinkGroup) -> &'static str {
    match link {
        Some(0) => "A",
        Some(1) => "B",
        Some(2) => "C",
        Some(3) => "D",
        _ => "-",
    }
}

/// The link group a new tab's panes take: the first (A, B, …) no other tab uses, so a symbol
/// change in one tab never ripples into another; `None` (unlinked) when every group is in use.
/// `used` holds every other tab's panes' groups (unlinked ones included, ignored).
pub fn first_free_group(used: &[LinkGroup]) -> LinkGroup {
    (0..LINK_GROUPS).find(|g| !used.contains(&Some(*g)))
}

/// [`first_free_group`] over every tab's groups (`per_tab`), leaving out the tab `except` (the
/// one being replaced: closing a window's last tab, its fresh tab doesn't count it as in use).
pub fn free_group_for_new_tab(per_tab: &[Vec<LinkGroup>], except: Option<usize>) -> LinkGroup {
    let used: Vec<LinkGroup> = per_tab
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != except)
        .flat_map(|(_, g)| g.iter().copied())
        .collect();
    first_free_group(&used)
}

/// The next link setting: unlinked → A → B → C → D → unlinked.
pub fn next_link(link: LinkGroup) -> LinkGroup {
    match link {
        None => Some(0),
        Some(g) if g + 1 < LINK_GROUPS => Some(g + 1),
        Some(_) => None,
    }
}

/// `Row`: side by side (first on the left). `Column`: stacked (first on top).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Row,
    Column,
}

/// Smallest and largest share a split gives its first child.
pub const MIN_RATIO: f32 = 0.05;
pub const MAX_RATIO: f32 = 0.95;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout<T> {
    Pane(T),
    Split {
        axis: Axis,
        /// The first child's share of the space, `MIN_RATIO..=MAX_RATIO`.
        ratio: f32,
        first: Box<Layout<T>>,
        second: Box<Layout<T>>,
    },
}

/// Which child of a split: a path from the root to a node is a list of these.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Side {
    First,
    Second,
}

/// A rectangle in logical px, origin top-left.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    pub fn centre(&self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }
}

/// A leaf's rectangle: `index` is its position in [`Layout::leaves`] order.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Placed {
    pub index: usize,
    pub rect: Rect,
}

/// A split's draggable boundary.
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    /// Path to the split node.
    pub path: Vec<Side>,
    pub axis: Axis,
    /// The whole split's rectangle (the ratio is relative to it).
    pub area: Rect,
    /// The boundary's position: x for `Row`, y for `Column`.
    pub at: f32,
    /// The least extent each side keeps (height for `Column`, width for `Row`): the boundary
    /// stops there.
    pub min_first: f32,
    pub min_second: f32,
}

impl Divider {
    /// The ratio that puts the boundary at pointer position `x, y`.
    pub fn ratio_at(&self, x: f32, y: f32) -> f32 {
        let r = match self.axis {
            Axis::Row => {
                let rightmost = self.area.x + (self.area.w - self.min_second).max(self.min_first);
                // A position that isn't a number stays one (`clamp_ratio` makes it 0.5).
                let x = if x.is_finite() {
                    x.min(rightmost).max(self.area.x + self.min_first)
                } else {
                    x
                };
                (x - self.area.x) / self.area.w.max(1.0)
            }
            Axis::Column => {
                let lowest = self.area.y + (self.area.h - self.min_second).max(self.min_first);
                let y = y.min(lowest).max(self.area.y + self.min_first);
                (y - self.area.y) / self.area.h.max(1.0)
            }
        };
        clamp_ratio(r)
    }

    /// The divider's grab handle, `grab` px wide, centred on the boundary.
    pub fn handle(&self, grab: f32) -> Rect {
        match self.axis {
            Axis::Row => Rect::new(self.at - grab * 0.5, self.area.y, grab, self.area.h),
            Axis::Column => Rect::new(self.area.x, self.at - grab * 0.5, self.area.w, grab),
        }
    }
}

/// Whether a left press at (`x`, `y`) starts dragging the pane at `pane` out by its header:
/// inside the header's own row (`header_h` px tall from the pane's top) and outside every
/// divider's grab band (`grabs`, `Divider::handle`), which always takes the press first. A
/// band reaches a few px into the panes on both sides of its line, over a header right under
/// it; a press there is a resize, never a pop-out.
pub fn header_drag_starts(pane: Rect, header_h: f32, x: f32, y: f32, grabs: &[Rect]) -> bool {
    Rect::new(pane.x, pane.y, pane.w, header_h).contains(x, y)
        && !grabs.iter().any(|g| g.contains(x, y))
}

/// Focus movement.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Ratios from files or drags: finite and inside the allowed range.
pub fn clamp_ratio(r: f32) -> f32 {
    if r.is_finite() {
        r.clamp(MIN_RATIO, MAX_RATIO)
    } else {
        0.5
    }
}

impl<T> Layout<T> {
    pub fn split(axis: Axis, ratio: f32, first: Layout<T>, second: Layout<T>) -> Layout<T> {
        Layout::Split {
            axis,
            ratio: clamp_ratio(ratio),
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// Leaves in order (depth-first, first before second).
    pub fn leaves(&self) -> Vec<&T> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect<'a>(&'a self, out: &mut Vec<&'a T>) {
        match self {
            Layout::Pane(p) => out.push(p),
            Layout::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }

    pub fn leaf_count(&self) -> usize {
        match self {
            Layout::Pane(_) => 1,
            Layout::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }

    /// The same tree with every leaf mapped.
    pub fn map<U>(&self, f: &mut impl FnMut(&T) -> U) -> Layout<U> {
        match self {
            Layout::Pane(p) => Layout::Pane(f(p)),
            Layout::Split {
                axis,
                ratio,
                first,
                second,
            } => Layout::Split {
                axis: *axis,
                ratio: *ratio,
                first: Box::new(first.map(f)),
                second: Box::new(second.map(f)),
            },
        }
    }

    /// Clamp every ratio (e.g. after reading a file).
    pub fn normalise(&mut self) {
        if let Layout::Split {
            ratio,
            first,
            second,
            ..
        } = self
        {
            *ratio = clamp_ratio(*ratio);
            first.normalise();
            second.normalise();
        }
    }

    #[cfg(test)]
    /// Every leaf's rectangle in `area`. Leaves tile the area exactly; edges are whole px.
    pub fn place(&self, area: Rect) -> Vec<Placed> {
        self.place_where(area, &|_| true, &|_| 0.0, &|_| 0.0)
    }

    /// The rectangles of the leaves for which `shown` is true; the others take no space.
    /// A split with one side wholly hidden gives the other side its whole rectangle, and its
    /// ratio is kept, so showing the hidden side again restores exactly the old geometry.
    /// `min_h` is the least height a leaf keeps, ratio or not, wherever the space allows; the ratio
    /// itself isn't touched, so a taller window restores the saved split.
    /// `Placed::index` is the leaf's position among *all* leaves.
    pub fn place_where(
        &self,
        area: Rect,
        shown: &dyn Fn(&T) -> bool,
        min_h: &dyn Fn(&T) -> f32,
        min_w: &dyn Fn(&T) -> f32,
    ) -> Vec<Placed> {
        let mut out = Vec::new();
        let mut next = 0;
        self.walk(
            area,
            shown,
            min_h,
            min_w,
            &mut next,
            &mut Vec::new(),
            &mut |node, rect, _, index| {
                if let Layout::Pane(_) = node {
                    out.push(Placed { index, rect });
                }
            },
        );
        out
    }

    #[cfg(test)]
    /// Every split's boundary in `area`, outermost first.
    pub fn dividers(&self, area: Rect) -> Vec<Divider> {
        self.dividers_where(area, &|_| true, &|_| 0.0, &|_| 0.0)
    }

    /// The boundaries between shown leaves: a split with a wholly hidden side has none.
    pub fn dividers_where(
        &self,
        area: Rect,
        shown: &dyn Fn(&T) -> bool,
        min_h: &dyn Fn(&T) -> f32,
        min_w: &dyn Fn(&T) -> f32,
    ) -> Vec<Divider> {
        let mut out = Vec::new();
        let mut next = 0;
        self.walk(
            area,
            shown,
            min_h,
            min_w,
            &mut next,
            &mut Vec::new(),
            &mut |node, rect, path, _| {
                if let Layout::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } = node
                    && first.any(shown)
                    && second.any(shown)
                {
                    let (min_first, min_second) = match axis {
                        Axis::Row => (
                            first.min_width(shown, min_w),
                            second.min_width(shown, min_w),
                        ),
                        Axis::Column => (
                            first.min_height(shown, min_h),
                            second.min_height(shown, min_h),
                        ),
                    };
                    let (a, _) = cut(rect, *axis, *ratio, min_first, min_second);
                    out.push(Divider {
                        path: path.to_vec(),
                        axis: *axis,
                        area: rect,
                        at: match axis {
                            Axis::Row => a.right(),
                            Axis::Column => a.bottom(),
                        },
                        min_first,
                        min_second,
                    });
                }
            },
        );
        out
    }

    /// The least width this subtree needs: its panes side by side (a row split) add up.
    fn min_width(&self, shown: &dyn Fn(&T) -> bool, min_w: &dyn Fn(&T) -> f32) -> f32 {
        match self {
            Layout::Pane(t) => {
                if shown(t) {
                    min_w(t)
                } else {
                    0.0
                }
            }
            Layout::Split {
                axis,
                first,
                second,
                ..
            } => {
                let (a, b) = (
                    first.min_width(shown, min_w),
                    second.min_width(shown, min_w),
                );
                match axis {
                    Axis::Row => a + b,
                    Axis::Column => a.max(b),
                }
            }
        }
    }

    /// The least height this node keeps: stacked leaves add up, side by side the tallest counts.
    fn min_height(&self, shown: &dyn Fn(&T) -> bool, min_h: &dyn Fn(&T) -> f32) -> f32 {
        match self {
            Layout::Pane(t) => {
                if shown(t) {
                    min_h(t)
                } else {
                    0.0
                }
            }
            Layout::Split {
                axis,
                first,
                second,
                ..
            } => {
                let (a, b) = (
                    first.min_height(shown, min_h),
                    second.min_height(shown, min_h),
                );
                match axis {
                    Axis::Row => a.max(b),
                    Axis::Column => a + b,
                }
            }
        }
    }

    /// Whether any leaf here is shown.
    pub fn any(&self, shown: &dyn Fn(&T) -> bool) -> bool {
        match self {
            Layout::Pane(p) => shown(p),
            Layout::Split { first, second, .. } => first.any(shown) || second.any(shown),
        }
    }

    /// Visit shown nodes with their rectangles; `next` counts leaves (hidden ones too).
    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        rect: Rect,
        shown: &dyn Fn(&T) -> bool,
        min_h: &dyn Fn(&T) -> f32,
        min_w: &dyn Fn(&T) -> f32,
        next: &mut usize,
        path: &mut Vec<Side>,
        f: &mut impl FnMut(&Layout<T>, Rect, &[Side], usize),
    ) {
        if !self.any(shown) {
            *next += self.leaf_count();
            return;
        }
        f(self, rect, path, *next);
        match self {
            Layout::Pane(_) => *next += 1,
            Layout::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b) = match (first.any(shown), second.any(shown)) {
                    (true, true) => {
                        let (mf, ms) = match axis {
                            Axis::Row => (
                                first.min_width(shown, min_w),
                                second.min_width(shown, min_w),
                            ),
                            Axis::Column => (
                                first.min_height(shown, min_h),
                                second.min_height(shown, min_h),
                            ),
                        };
                        cut(rect, *axis, *ratio, mf, ms)
                    }
                    _ => (rect, rect),
                };
                path.push(Side::First);
                first.walk(a, shown, min_h, min_w, next, path, f);
                path.pop();
                path.push(Side::Second);
                second.walk(b, shown, min_h, min_w, next, path, f);
                path.pop();
            }
        }
    }

    fn node_mut(&mut self, path: &[Side]) -> Option<&mut Layout<T>> {
        let Some((side, rest)) = path.split_first() else {
            return Some(self);
        };
        match self {
            Layout::Pane(_) => None,
            Layout::Split { first, second, .. } => match side {
                Side::First => first.node_mut(rest),
                Side::Second => second.node_mut(rest),
            },
        }
    }

    /// Set the ratio of the split at `path` (clamped). `false` if `path` isn't a split.
    pub fn set_ratio(&mut self, path: &[Side], r: f32) -> bool {
        match self.node_mut(path) {
            Some(Layout::Split { ratio, .. }) => {
                *ratio = clamp_ratio(r);
                true
            }
            _ => false,
        }
    }

    #[cfg(test)]
    /// The leaf whose rectangle lies next to leaf `from` in `dir`: of the leaves across that
    /// edge that overlap it, the one overlapping most (ties: the one nearest its centre).
    pub fn neighbour(&self, from: usize, dir: Direction, area: Rect) -> Option<usize> {
        self.neighbour_where(from, dir, area, &|_| true, &|_| 0.0, &|_| 0.0)
    }

    /// [`Layout::neighbour`] among shown leaves (`from` and the result index all leaves).
    pub fn neighbour_where(
        &self,
        from: usize,
        dir: Direction,
        area: Rect,
        shown: &dyn Fn(&T) -> bool,
        min_h: &dyn Fn(&T) -> f32,
        min_w: &dyn Fn(&T) -> f32,
    ) -> Option<usize> {
        let placed = self.place_where(area, shown, min_h, min_w);
        let src = placed.iter().find(|p| p.index == from)?.rect;
        let (cx, cy) = src.centre();
        let eps = 0.5;
        placed
            .iter()
            .filter(|p| p.index != from)
            .filter_map(|p| {
                let r = p.rect;
                let (adjacent, overlap, dist) = match dir {
                    Direction::Left => (
                        (r.right() - src.x).abs() < eps,
                        span_overlap(r.y, r.bottom(), src.y, src.bottom()),
                        (r.centre().1 - cy).abs(),
                    ),
                    Direction::Right => (
                        (r.x - src.right()).abs() < eps,
                        span_overlap(r.y, r.bottom(), src.y, src.bottom()),
                        (r.centre().1 - cy).abs(),
                    ),
                    Direction::Up => (
                        (r.bottom() - src.y).abs() < eps,
                        span_overlap(r.x, r.right(), src.x, src.right()),
                        (r.centre().0 - cx).abs(),
                    ),
                    Direction::Down => (
                        (r.y - src.bottom()).abs() < eps,
                        span_overlap(r.x, r.right(), src.x, src.right()),
                        (r.centre().0 - cx).abs(),
                    ),
                };
                (adjacent && overlap > 0.0).then_some((p.index, overlap, dist))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1).then(b.2.total_cmp(&a.2)))
            .map(|(i, _, _)| i)
    }
}

impl<T: PartialEq> Layout<T> {
    /// Leaf position of `leaf` in [`Layout::leaves`] order.
    pub fn index_of(&self, leaf: &T) -> Option<usize> {
        self.leaves().iter().position(|l| *l == leaf)
    }

    /// Replace leaf `target` with a split holding it and `new` (new second: right or below).
    /// `Err` gives both back if `target` isn't a leaf here.
    pub fn split_leaf(self, target: &T, axis: Axis, new: T) -> Result<Layout<T>, (Layout<T>, T)> {
        self.split_leaf_at(target, axis, new, 0.5)
    }

    /// [`Layout::split_leaf`] giving `target` the share `share`.
    pub fn split_leaf_at(
        self,
        target: &T,
        axis: Axis,
        new: T,
        share: f32,
    ) -> Result<Layout<T>, (Layout<T>, T)> {
        match self {
            Layout::Pane(p) if &p == target => Ok(Layout::split(
                axis,
                share,
                Layout::Pane(p),
                Layout::Pane(new),
            )),
            Layout::Pane(p) => Err((Layout::Pane(p), new)),
            Layout::Split {
                axis: a,
                ratio,
                first,
                second,
            } => match first.split_leaf_at(target, axis, new, share) {
                Ok(f) => Ok(Layout::Split {
                    axis: a,
                    ratio,
                    first: Box::new(f),
                    second,
                }),
                Err((f, new)) => match second.split_leaf_at(target, axis, new, share) {
                    Ok(s) => Ok(Layout::Split {
                        axis: a,
                        ratio,
                        first: Box::new(f),
                        second: Box::new(s),
                    }),
                    Err((s, new)) => Err((
                        Layout::Split {
                            axis: a,
                            ratio,
                            first: Box::new(f),
                            second: Box::new(s),
                        },
                        new,
                    )),
                },
            },
        }
    }

    /// Remove leaf `target`; its sibling takes the parent split's place. Returns the layout
    /// without it, or `None` if `target` was the only leaf (or isn't here: then `self`).
    pub fn remove_leaf(self, target: &T) -> Result<Option<Layout<T>>, Layout<T>> {
        match self {
            Layout::Pane(p) if &p == target => Ok(None),
            Layout::Pane(p) => Err(Layout::Pane(p)),
            Layout::Split {
                axis,
                ratio,
                first,
                second,
            } => match first.remove_leaf(target) {
                Ok(None) => Ok(Some(*second)),
                Ok(Some(f)) => Ok(Some(Layout::Split {
                    axis,
                    ratio,
                    first: Box::new(f),
                    second,
                })),
                Err(f) => match second.remove_leaf(target) {
                    Ok(None) => Ok(Some(f)),
                    Ok(Some(s)) => Ok(Some(Layout::Split {
                        axis,
                        ratio,
                        first: Box::new(f),
                        second: Box::new(s),
                    })),
                    Err(s) => Err(Layout::Split {
                        axis,
                        ratio,
                        first: Box::new(f),
                        second: Box::new(s),
                    }),
                },
            },
        }
    }
}

/// Split `r` along `axis` at `ratio`, at a whole pixel. A `Column` boundary keeps `min_first` above it and `min_second` below it; a space too
/// short for both keeps the first's, and the second has what's left.
fn cut(r: Rect, axis: Axis, ratio: f32, min_first: f32, min_second: f32) -> (Rect, Rect) {
    match axis {
        Axis::Row => {
            let a = (r.w * ratio)
                .round()
                .min((r.w - min_second).floor())
                .max(min_first)
                .min(r.w);
            (
                Rect::new(r.x, r.y, a, r.h),
                Rect::new(r.x + a, r.y, r.w - a, r.h),
            )
        }
        Axis::Column => {
            let a = (r.h * ratio)
                .round()
                .min((r.h - min_second).floor())
                .max(min_first)
                .min(r.h);
            (
                Rect::new(r.x, r.y, r.w, a),
                Rect::new(r.x, r.y + a, r.w, r.h - a),
            )
        }
    }
}

fn span_overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    (a1.min(b1) - a0.max(b0)).max(0.0)
}

/// Built-in layouts (decision 0016: a grid is just a generated split tree).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Preset {
    /// One chart.
    Chart,
    /// Two charts side by side, linked separately (A, B).
    OneByTwo,
    /// Four charts in a 2×2 grid (A, B, C, D).
    TwoByTwo,
    /// A large chart on the left (A), two stacked on the right (B, C).
    OnePlusTwo,
    /// Chart beside the DOM, time & sales under the DOM, all one group (A).
    Trading,
}

impl Preset {
    pub const ALL: [Preset; 5] = [
        Preset::OneByTwo,
        Preset::TwoByTwo,
        Preset::OnePlusTwo,
        Preset::Trading,
        Preset::Chart,
    ];

    /// The name used on the command line and in menus.
    pub fn name(self) -> &'static str {
        match self {
            Preset::Chart => "chart",
            Preset::OneByTwo => "1x2",
            Preset::TwoByTwo => "2x2",
            Preset::OnePlusTwo => "1+2",
            Preset::Trading => "trading",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Preset::Chart => "Chart",
            Preset::OneByTwo => "1×2 charts",
            Preset::TwoByTwo => "2×2 charts",
            Preset::OnePlusTwo => "1+2 charts",
            Preset::Trading => "Chart + DOM + T&S",
        }
    }

    pub fn from_name(s: &str) -> Option<Preset> {
        Preset::ALL
            .into_iter()
            .find(|p| p.name().eq_ignore_ascii_case(s))
    }

    /// The tree, with each leaf made by `leaf(kind, link)`.
    pub fn build<T>(self, mut leaf: impl FnMut(PaneKind, LinkGroup) -> T) -> Layout<T> {
        use PaneKind::*;
        let mut p = |k, l| Layout::Pane(leaf(k, l));
        match self {
            Preset::Chart => p(Chart, Some(0)),
            Preset::OneByTwo => Layout::split(Axis::Row, 0.5, p(Chart, Some(0)), p(Chart, Some(1))),
            Preset::TwoByTwo => {
                let top = Layout::split(Axis::Row, 0.5, p(Chart, Some(0)), p(Chart, Some(1)));
                let bottom = Layout::split(Axis::Row, 0.5, p(Chart, Some(2)), p(Chart, Some(3)));
                Layout::split(Axis::Column, 0.5, top, bottom)
            }
            Preset::OnePlusTwo => {
                let big = p(Chart, Some(0));
                let right = Layout::split(Axis::Column, 0.5, p(Chart, Some(1)), p(Chart, Some(2)));
                Layout::split(Axis::Row, 0.62, big, right)
            }
            Preset::Trading => trading_group(Some(0), &mut leaf),
        }
    }
}

/// Chart | (DOM over time & sales), all in `link`: one market's trading trio.
pub fn trading_group<T>(
    link: LinkGroup,
    leaf: &mut impl FnMut(PaneKind, LinkGroup) -> T,
) -> Layout<T> {
    let chart = Layout::Pane(leaf(PaneKind::Chart, link));
    let dom = Layout::Pane(leaf(PaneKind::Dom, link));
    let tape = Layout::Pane(leaf(PaneKind::Tape, link));
    Layout::split(
        Axis::Row,
        0.68,
        chart,
        Layout::split(Axis::Column, 0.64, dom, tape),
    )
}

#[cfg(test)]
mod tests {
    /// The owner's bug: dragging the divider between a DOM and the time & sales under it
    /// popped the T&S out. A press in a divider's grab band, right over the pane header
    /// under (or beside) it, is the divider's; only the header row past the band drags the
    /// pane out. Column and row splits, at 100 % and 150 % header heights.
    #[test]
    fn a_divider_takes_the_press_over_the_header_under_it() {
        use super::*;
        const GRAB: f32 = 6.0;
        let area = Rect::new(0.0, 0.0, 400.0, 600.0);
        for header_h in [27.0f32, 27.0 * 1.5] {
            // DOM above, T&S below: the T&S header sits right under the line.
            let l = Layout::split(Axis::Column, 0.5, Layout::Pane(0u32), Layout::Pane(1u32));
            let placed = l.place(area);
            let grabs: Vec<Rect> = l.dividers(area).iter().map(|d| d.handle(GRAB)).collect();
            let below = placed[1].rect;
            let (x, top) = (below.x + below.w * 0.5, below.y);
            for dy in [0.0, 1.0, 2.0, 2.9] {
                assert!(
                    !header_drag_starts(below, header_h, x, top + dy, &grabs),
                    "{dy} px under the line, header {header_h}: the divider's"
                );
            }
            assert!(header_drag_starts(below, header_h, x, top + 10.0, &grabs));
            assert!(header_drag_starts(
                below,
                header_h,
                x,
                top + header_h - 1.0,
                &grabs
            ));
            assert!(!header_drag_starts(
                below,
                header_h,
                x,
                top + header_h + 1.0,
                &grabs
            ));
            // Side by side: the band beside the line wins over the right pane's header.
            let l = Layout::split(Axis::Row, 0.5, Layout::Pane(0u32), Layout::Pane(1u32));
            let placed = l.place(area);
            let grabs: Vec<Rect> = l.dividers(area).iter().map(|d| d.handle(GRAB)).collect();
            let right = placed[1].rect;
            let y = right.y + header_h * 0.5;
            assert!(!header_drag_starts(
                right,
                header_h,
                right.x + 1.0,
                y,
                &grabs
            ));
            assert!(header_drag_starts(
                right,
                header_h,
                right.x + 20.0,
                y,
                &grabs
            ));
        }
    }

    /// A row split holds each side's least width, as a column split holds heights (a DOM
    /// above 100 % UI zoom).
    #[test]
    fn a_row_split_holds_each_sides_least_width() {
        let l = Layout::split(Axis::Row, 0.75, Layout::Pane(0u32), Layout::Pane(1u32));
        let min_w = |id: &u32| if *id == 1 { 360.0 } else { 0.0 };
        let placed = l.place_where(
            Rect::new(0.0, 0.0, 700.0, 500.0),
            &|_| true,
            &|_| 0.0,
            &min_w,
        );
        assert_eq!(
            placed[1].rect.w, 360.0,
            "the second side keeps its least width"
        );
        assert_eq!(placed[0].rect.w, 340.0);
        let wide = l.place_where(
            Rect::new(0.0, 0.0, 2000.0, 500.0),
            &|_| true,
            &|_| 0.0,
            &min_w,
        );
        assert_eq!(wide[1].rect.w, 500.0, "with room, the ratio as saved");
        let d = &l.dividers_where(
            Rect::new(0.0, 0.0, 700.0, 500.0),
            &|_| true,
            &|_| 0.0,
            &min_w,
        )[0];
        assert_eq!(d.at, 340.0, "the divider where the panes meet");
        assert_eq!(
            d.ratio_at(600.0, 10.0),
            340.0 / 700.0,
            "a drag stops at the least width"
        );
    }

    use super::*;

    /// A stack of a tall-minimum pane (1) over a free one (2), at `ratio`.
    fn stack(ratio: f32) -> Layout<u32> {
        Layout::split(Axis::Column, ratio, Layout::Pane(1), Layout::Pane(2))
    }

    fn min_of_one(id: &u32) -> f32 {
        if *id == 1 { 91.0 } else { 0.0 }
    }

    fn heights(l: &Layout<u32>, h: f32) -> Vec<f32> {
        l.place_where(
            Rect::new(0.0, 0.0, 800.0, h),
            &|_| true,
            &min_of_one,
            &|_| 0.0,
        )
        .iter()
        .map(|p| p.rect.h)
        .collect()
    }

    /// A small ratio keeps the pane's minimum, tiling exactly; the ratio is untouched, so a taller
    /// window gets the saved split back.
    #[test]
    fn a_minimum_height_holds_against_the_ratio() {
        let l = stack(0.05);
        assert_eq!(heights(&l, 800.0), [91.0, 709.0]);
        assert_eq!(
            heights(&l, 2000.0),
            [100.0, 1900.0],
            "the ratio's own share"
        );
        let Layout::Split { ratio, .. } = l else {
            panic!()
        };
        assert_eq!(ratio, 0.05);
        // On the second side too.
        let l = Layout::split(Axis::Column, 0.95, Layout::Pane(2), Layout::Pane(1));
        assert_eq!(heights(&l, 800.0), [709.0, 91.0]);
        // Hidden: no claim on the space.
        let placed = l.place_where(
            Rect::new(0.0, 0.0, 800.0, 800.0),
            &|id| *id != 1,
            &min_of_one,
            &|_| 0.0,
        );
        assert_eq!(placed[0].rect.h, 800.0);
    }

    /// Nested: stacked minimums add up, side by side the tallest counts, however deep.
    #[test]
    fn minimums_add_up_through_nested_splits() {
        let beside = Layout::split(Axis::Row, 0.5, Layout::Pane(1), Layout::Pane(2));
        let l = Layout::split(Axis::Column, 0.05, beside, Layout::Pane(2));
        assert_eq!(heights(&l, 800.0), [91.0, 91.0, 709.0]);
        let two = Layout::split(Axis::Column, 0.05, Layout::Pane(1), Layout::Pane(1));
        let l = Layout::split(Axis::Column, 0.05, two, Layout::Pane(2));
        assert_eq!(heights(&l, 800.0), [91.0, 91.0, 618.0]);
    }

    /// Too short for every minimum: the rectangles still tile the space, the first keeps its
    /// minimum and the second has what's left (nothing, past that).
    #[test]
    fn a_space_too_short_keeps_the_first_minimum() {
        let two = Layout::split(Axis::Column, 0.5, Layout::Pane(1), Layout::Pane(1));
        assert_eq!(heights(&two, 150.0), [91.0, 59.0]);
        assert_eq!(heights(&two, 60.0), [60.0, 0.0]);
    }

    /// A drag stops at the minimum, on both sides, and the boundary drawn agrees.
    #[test]
    fn a_divider_stops_at_the_minimum() {
        let two = Layout::split(Axis::Column, 0.5, Layout::Pane(1), Layout::Pane(1));
        let area = Rect::new(0.0, 10.0, 800.0, 400.0);
        let d = &two.dividers_where(area, &|_| true, &min_of_one, &|_| 0.0)[0];
        assert_eq!(d.at, 210.0);
        assert_eq!(d.ratio_at(0.0, 10.0), 91.0 / 400.0, "dragged to the top");
        assert_eq!(
            d.ratio_at(0.0, 410.0),
            (400.0 - 91.0) / 400.0,
            "to the bottom"
        );
        assert_eq!(d.ratio_at(0.0, 110.0), 0.25, "free between");
        let free = Layout::split(Axis::Row, 0.5, Layout::Pane(1), Layout::Pane(1));
        let d = &free.dividers_where(area, &|_| true, &min_of_one, &|_| 0.0)[0];
        assert_eq!(d.ratio_at(0.0, 0.0), MIN_RATIO, "width is not limited");
    }

    fn area(w: f32, h: f32) -> Rect {
        Rect::new(0.0, 0.0, w, h)
    }

    fn kinds(l: &Layout<(PaneKind, LinkGroup)>) -> Vec<PaneKind> {
        l.leaves().iter().map(|p| p.0).collect()
    }

    #[test]
    fn trading_preset_is_chart_beside_dom_over_tape() {
        let l = Preset::Trading.build(|k, g| (k, g));
        assert_eq!(
            kinds(&l),
            vec![PaneKind::Chart, PaneKind::Dom, PaneKind::Tape]
        );
        let r = l.place(area(1000.0, 800.0));
        assert_eq!(r[0].rect, Rect::new(0.0, 0.0, 680.0, 800.0));
        assert_eq!(r[1].rect, Rect::new(680.0, 0.0, 320.0, 512.0));
        assert_eq!(r[2].rect, Rect::new(680.0, 512.0, 320.0, 288.0));
        assert!(l.leaves().iter().all(|p| p.1 == Some(0)));
    }

    #[test]
    fn leaves_tile_the_area_exactly_in_every_preset() {
        for preset in Preset::ALL {
            let l = preset.build(|k, g| (k, g));
            let a = Rect::new(10.0, 20.0, 1601.0, 899.0);
            let placed = l.place(a);
            assert_eq!(placed.len(), l.leaf_count(), "{preset:?}");
            let total: f32 = placed.iter().map(|p| p.rect.w * p.rect.h).sum();
            assert_eq!(total, a.w * a.h, "{preset:?}");
            for p in &placed {
                assert!(p.rect.x >= a.x && p.rect.right() <= a.right(), "{preset:?}");
                assert!(
                    p.rect.y >= a.y && p.rect.bottom() <= a.bottom(),
                    "{preset:?}"
                );
                assert_eq!(p.rect.x.fract(), 0.0, "whole-pixel edges");
            }
        }
    }

    #[test]
    fn presets_have_the_documented_shapes_and_links() {
        let links = |p: Preset| -> Vec<LinkGroup> {
            p.build(|k, g| (k, g))
                .leaves()
                .iter()
                .map(|x| x.1)
                .collect()
        };
        assert_eq!(links(Preset::OneByTwo), vec![Some(0), Some(1)]);
        assert_eq!(
            links(Preset::TwoByTwo),
            vec![Some(0), Some(1), Some(2), Some(3)]
        );
        assert_eq!(links(Preset::OnePlusTwo), vec![Some(0), Some(1), Some(2)]);
        let grid = Preset::TwoByTwo
            .build(|k, g| (k, g))
            .place(area(800.0, 600.0));
        assert_eq!(grid[3].rect, Rect::new(400.0, 300.0, 400.0, 300.0));
        for p in Preset::ALL {
            assert_eq!(Preset::from_name(p.name()), Some(p));
        }
        assert_eq!(Preset::from_name("2X2"), Some(Preset::TwoByTwo));
        assert_eq!(Preset::from_name("nope"), None);
    }

    #[test]
    fn split_and_close_a_leaf() {
        let l = Layout::Pane(1);
        let l = l.split_leaf(&1, Axis::Row, 2).unwrap();
        let l = l.split_leaf(&2, Axis::Column, 3).unwrap();
        let (l, back) = l.split_leaf(&9, Axis::Row, 4).unwrap_err();
        assert_eq!(back, 4);
        assert_eq!(l.leaves(), vec![&1, &2, &3]);
        let r = l.place(area(800.0, 600.0));
        assert_eq!(r[1].rect, Rect::new(400.0, 0.0, 400.0, 300.0));
        assert_eq!(r[2].rect, Rect::new(400.0, 300.0, 400.0, 300.0));

        // Closing 2: 3 takes the right half.
        let l = l.remove_leaf(&2).unwrap().unwrap();
        assert_eq!(l.leaves(), vec![&1, &3]);
        assert_eq!(
            l.place(area(800.0, 600.0))[1].rect,
            Rect::new(400.0, 0.0, 400.0, 600.0)
        );
        // Not here: unchanged.
        let l = l.remove_leaf(&7).unwrap_err();
        assert_eq!(l.leaves(), vec![&1, &3]);
        let l = l.remove_leaf(&1).unwrap().unwrap();
        assert_eq!(l, Layout::Pane(3));
        assert_eq!(l.remove_leaf(&3), Ok(None), "the last leaf");
    }

    #[test]
    fn dragging_a_divider_sets_its_ratio_within_limits() {
        let mut l = Preset::Trading.build(|k, _| k);
        let a = area(1000.0, 800.0);
        let d = l.dividers(a);
        assert_eq!(d.len(), 2);
        assert_eq!(
            (d[0].axis, d[0].at, d[0].path.clone()),
            (Axis::Row, 680.0, vec![])
        );
        assert_eq!(
            (d[1].axis, d[1].at, d[1].path.clone()),
            (Axis::Column, 512.0, vec![Side::Second])
        );
        assert_eq!(d[0].handle(6.0), Rect::new(677.0, 0.0, 6.0, 800.0));
        // Drag the inner divider to y = 400 (of the right column, 0..800).
        let r = d[1].ratio_at(0.0, 400.0);
        assert!(l.set_ratio(&d[1].path, r));
        assert_eq!(l.dividers(a)[1].at, 400.0);
        // Dragged past the edge: clamped, never collapsing a pane.
        assert!(l.set_ratio(&d[0].path, d[0].ratio_at(-50.0, 0.0)));
        assert_eq!(l.dividers(a)[0].at, 50.0);
        assert!(!l.set_ratio(&[Side::First], 0.5), "a leaf has no ratio");
        assert_eq!(d[0].ratio_at(f32::NAN, 0.0), 0.5);
    }

    #[test]
    fn focus_moves_to_the_adjacent_pane() {
        // [0 | 1/2]: chart, DOM over tape.
        let l = Preset::Trading.build(|k, _| k);
        let a = area(1000.0, 800.0);
        assert_eq!(
            l.neighbour(0, Direction::Right, a),
            Some(1),
            "the larger overlap"
        );
        assert_eq!(l.neighbour(1, Direction::Down, a), Some(2));
        assert_eq!(l.neighbour(2, Direction::Up, a), Some(1));
        assert_eq!(l.neighbour(2, Direction::Left, a), Some(0));
        assert_eq!(l.neighbour(0, Direction::Left, a), None, "edge");
        assert_eq!(l.neighbour(1, Direction::Up, a), None);
        // 2×2: straight lines.
        let g = Preset::TwoByTwo.build(|k, _| k);
        assert_eq!(g.neighbour(0, Direction::Right, a), Some(1));
        assert_eq!(g.neighbour(0, Direction::Down, a), Some(2));
        assert_eq!(g.neighbour(3, Direction::Up, a), Some(1));
        assert_eq!(g.neighbour(3, Direction::Left, a), Some(2));
    }

    #[test]
    fn link_cycle_and_ratio_clamps() {
        let mut l = None;
        let mut seen = vec![];
        for _ in 0..5 {
            l = next_link(l);
            seen.push(link_letter(l));
        }
        assert_eq!(seen, vec!["A", "B", "C", "D", "-"]);
        let Layout::Split { ratio, .. } =
            Layout::split(Axis::Column, 7.0, Layout::Pane(1), Layout::Pane(2))
        else {
            panic!()
        };
        assert_eq!(ratio, MAX_RATIO);
        let mut bad = Layout::Split {
            axis: Axis::Row,
            ratio: f32::NAN,
            first: Box::new(Layout::Pane(1)),
            second: Box::new(Layout::Pane(2)),
        };
        bad.normalise();
        assert_eq!(bad.dividers(area(100.0, 10.0))[0].at, 50.0);
    }

    #[test]
    fn serialises_as_a_readable_tree() {
        let l = Layout::split(Axis::Row, 0.5, Layout::Pane(1u32), Layout::Pane(2u32));
        let s = serde_json::to_string(&l).unwrap();
        assert_eq!(
            s,
            r#"{"split":{"axis":"row","ratio":0.5,"first":{"pane":1},"second":{"pane":2}}}"#
        );
        assert_eq!(serde_json::from_str::<Layout<u32>>(&s).unwrap(), l);
    }

    /// A new tab takes the first group no other tab uses; with every group in use, none.
    #[test]
    fn a_new_tab_takes_the_first_free_group() {
        use super::first_free_group;
        assert_eq!(first_free_group(&[]), Some(0), "nothing in use: A");
        assert_eq!(
            first_free_group(&[Some(0), Some(0), None]),
            Some(1),
            "A in use: B"
        );
        assert_eq!(
            first_free_group(&[Some(0), Some(2)]),
            Some(1),
            "the first free, not the next"
        );
        assert_eq!(first_free_group(&[Some(1), Some(3)]), Some(0));
        assert_eq!(
            first_free_group(&[Some(0), Some(1), Some(2), Some(3)]),
            None,
            "every group in use: unlinked"
        );
        assert_eq!(
            first_free_group(&[None, None]),
            Some(0),
            "unlinked panes use no group"
        );
        // Every tab counts but the one being replaced: closing the only tab (in A), its fresh
        // tab takes A again.
        use super::free_group_for_new_tab;
        let tabs = vec![vec![Some(0), Some(0)], vec![Some(1)]];
        assert_eq!(free_group_for_new_tab(&tabs, None), Some(2));
        assert_eq!(free_group_for_new_tab(&tabs, Some(0)), Some(0));
        assert_eq!(free_group_for_new_tab(&[vec![Some(0)]], Some(0)), Some(0));
    }
}
