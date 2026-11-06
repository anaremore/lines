//! Window layout as a value: a split tree of panes (roadmap 5; the workspace milestone extends
//! it with tabs, more windows, drag-to-resize and saving).
//!
//! A [`Layout`] is either one pane or a split of two layouts along an axis at a ratio. It is
//! plain data: the workspace view renders whatever tree it is given, and nothing in a pane
//! knows where it sits. Panes in the same link group follow each other's symbol changes.

/// What kind of pane a leaf holds.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum PaneKind {
    Chart,
    Dom,
    Tape,
}

/// Panes sharing a link group switch symbol together (`None` = independent).
pub type LinkGroup = Option<u8>;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PaneSpec {
    pub kind: PaneKind,
    pub link: LinkGroup,
}

/// `Row`: side by side (first on the left). `Column`: stacked (first on top).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Axis {
    Row,
    Column,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Layout {
    Pane(PaneSpec),
    Split {
        axis: Axis,
        /// The first child's share of the space, `0 < ratio < 1`.
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

/// A leaf's rectangle in a layout of a given size (logical px, origin top-left). Pure geometry
/// for tests and for the workspace milestone's split dragging; rendering uses flex ratios.
#[allow(dead_code)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Placed {
    /// Index of the leaf in [`Layout::panes`] order.
    pub index: usize,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Layout {
    pub fn pane(kind: PaneKind, link: LinkGroup) -> Layout {
        Layout::Pane(PaneSpec { kind, link })
    }

    pub fn split(axis: Axis, ratio: f32, first: Layout, second: Layout) -> Layout {
        Layout::Split {
            axis,
            ratio: ratio.clamp(0.05, 0.95),
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// The default trading layout: chart | (DOM over time & sales), all linked.
    pub fn trading() -> Layout {
        let link = Some(0);
        Layout::split(
            Axis::Row,
            0.68,
            Layout::pane(PaneKind::Chart, link),
            Layout::split(
                Axis::Column,
                0.64,
                Layout::pane(PaneKind::Dom, link),
                Layout::pane(PaneKind::Tape, link),
            ),
        )
    }

    pub fn chart_only() -> Layout {
        Layout::pane(PaneKind::Chart, Some(0))
    }

    /// Leaves in order (depth-first, first before second).
    pub fn panes(&self) -> Vec<PaneSpec> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PaneSpec>) {
        match self {
            Layout::Pane(p) => out.push(*p),
            Layout::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }

    #[allow(dead_code)]
    /// Every leaf's rectangle in a `w × h` area (what the rendered tree produces, minus
    /// separators). Leaves tile the area exactly.
    pub fn place(&self, w: f32, h: f32) -> Vec<Placed> {
        let mut out = Vec::new();
        let mut next = 0;
        self.place_into(0.0, 0.0, w, h, &mut next, &mut out);
        out
    }

    #[allow(dead_code)]
    fn place_into(&self, x: f32, y: f32, w: f32, h: f32, next: &mut usize, out: &mut Vec<Placed>) {
        match self {
            Layout::Pane(_) => {
                out.push(Placed {
                    index: *next,
                    x,
                    y,
                    w,
                    h,
                });
                *next += 1;
            }
            Layout::Split {
                axis,
                ratio,
                first,
                second,
            } => match axis {
                Axis::Row => {
                    let a = (w * ratio).round();
                    first.place_into(x, y, a, h, next, out);
                    second.place_into(x + a, y, w - a, h, next, out);
                }
                Axis::Column => {
                    let a = (h * ratio).round();
                    first.place_into(x, y, w, a, next, out);
                    second.place_into(x, y + a, w, h - a, next, out);
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trading_layout_is_chart_beside_dom_over_tape() {
        let l = Layout::trading();
        let kinds: Vec<_> = l.panes().iter().map(|p| p.kind).collect();
        assert_eq!(kinds, vec![PaneKind::Chart, PaneKind::Dom, PaneKind::Tape]);
        let r = l.place(1000.0, 800.0);
        assert_eq!((r[0].x, r[0].w, r[0].h), (0.0, 680.0, 800.0));
        assert_eq!((r[1].x, r[1].y, r[1].w, r[1].h), (680.0, 0.0, 320.0, 512.0));
        assert_eq!((r[2].y, r[2].h), (512.0, 288.0));
        assert!(l.panes().iter().all(|p| p.link == Some(0)));
    }

    #[test]
    fn leaves_tile_the_area_exactly() {
        // Two charts side by side, each with its own DOM (e.g. NQ and ES).
        let pair = |link| {
            Layout::split(
                Axis::Row,
                0.7,
                Layout::pane(PaneKind::Chart, link),
                Layout::pane(PaneKind::Dom, link),
            )
        };
        let l = Layout::split(Axis::Row, 0.5, pair(Some(1)), pair(Some(2)));
        let placed = l.place(1601.0, 900.0);
        assert_eq!(placed.len(), 4);
        let area: f32 = placed.iter().map(|p| p.w * p.h).sum();
        assert_eq!(area, 1601.0 * 900.0);
        let links: Vec<_> = l.panes().iter().map(|p| p.link).collect();
        assert_eq!(links, vec![Some(1), Some(1), Some(2), Some(2)]);
    }

    #[test]
    fn ratios_are_kept_inside_the_area() {
        let Layout::Split { ratio, .. } = Layout::split(
            Axis::Column,
            7.0,
            Layout::pane(PaneKind::Tape, None),
            Layout::pane(PaneKind::Dom, None),
        ) else {
            panic!()
        };
        assert_eq!(ratio, 0.95);
    }
}
