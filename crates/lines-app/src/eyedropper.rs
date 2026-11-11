//! The color picker's eyedropper: the color of what is drawn under the pointer, from the
//! chart's own paint (never the screen's pixels: Lines never captures the screen).
//!
//! While the dropper is armed ([`arm`]), the chart picking paints between [`begin`] and
//! [`end`], and its painters note what they draw (other charts paint on the same thread and
//! note nothing): a line's
//! segments ([`Stroke`]: studies, drawings) and filled boxes ([`rect`]: candle bodies and
//! wicks, levels), each with its color and a name. [`pick`] finds the topmost within a few
//! pixels of the pointer. Disarmed, nothing is noted (one flag read per painter). The marks
//! go into a buffer reserved when armed and never grown while painting (paint allocates
//! nothing); past its size, the rest isn't noted.

use std::cell::RefCell;

use gpui::{Bounds, Hsla, PathBuilder, Pixels, Point, Window};
use lines_core::style::Rgb;

/// How near the pointer a mark counts (logical px, beyond half its width).
pub const NEAR: f32 = 4.0;
/// Marks noted per frame at most.
const CAP: usize = 16_384;

#[derive(Copy, Clone, Debug, PartialEq)]
enum Shape {
    /// A segment `a`–`b` this wide.
    Seg {
        a: (f32, f32),
        b: (f32, f32),
        w: f32,
    },
    /// A filled box.
    Rect { min: (f32, f32), max: (f32, f32) },
}

/// One thing drawn: its shape, color and what it is.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Mark {
    shape: Shape,
    pub rgb: Rgb,
    pub what: &'static str,
}

#[derive(Default)]
struct Rec {
    /// The buffers are reserved (a dropper is armed somewhere).
    armed: bool,
    /// The chart painting now notes what it draws.
    on: bool,
    marks: Vec<Mark>,
    /// A [`Stroke`]'s segments until it is painted (then they get its color).
    pending: Vec<((f32, f32), (f32, f32))>,
}

thread_local! {
    static REC: RefCell<Rec> = RefCell::new(Rec::default());
}

/// Arm (or disarm) the dropper: reserve the buffers (or free them).
pub fn arm(on: bool) {
    REC.with_borrow_mut(|r| {
        if on == r.armed {
            return;
        }
        r.armed = on;
        r.on = false;
        r.marks = Vec::new();
        r.pending = Vec::new();
        if on {
            r.marks.reserve(CAP);
            r.pending.reserve(CAP);
        }
    });
}

/// A chart starts painting: `picking` (its editor's dropper is armed) forgets its last
/// frame's marks and notes this one's.
pub fn begin(picking: bool) {
    REC.with_borrow_mut(|r| {
        r.on = picking && r.armed;
        if r.on {
            r.marks.clear();
            r.pending.clear();
        }
    });
}

/// The chart finished painting.
pub fn end() {
    REC.with_borrow_mut(|r| r.on = false);
}

/// Whether the chart painting now notes what it draws.
pub fn armed() -> bool {
    REC.with_borrow(|r| r.on)
}

fn push(r: &mut Rec, m: Mark) {
    if r.marks.len() < r.marks.capacity() {
        r.marks.push(m);
    }
}

fn xy(p: Point<Pixels>) -> (f32, f32) {
    (f32::from(p.x), f32::from(p.y))
}

/// A filled box drawn in `color` (a candle's body or wick, a level line).
pub fn rect(b: Bounds<Pixels>, color: Hsla, what: &'static str) {
    REC.with_borrow_mut(|r| {
        if r.on {
            let m = Mark {
                shape: Shape::Rect {
                    min: xy(b.origin),
                    max: xy(b.bottom_right()),
                },
                rgb: crate::theme::rgb_of(color),
                what,
            };
            push(r, m);
        }
    });
}

/// A segment from `a` to `b` (window px), `w` wide, drawn in `color` (a drawing's line).
pub fn segment(a: (f32, f32), b: (f32, f32), w: f32, color: Hsla, what: &'static str) {
    REC.with_borrow_mut(|r| {
        if r.on {
            let m = Mark {
                shape: Shape::Seg { a, b, w },
                rgb: crate::theme::rgb_of(color),
                what,
            };
            push(r, m);
        }
    });
}

/// A stroked line that also notes its segments while the dropper is armed.
pub struct Stroke {
    b: PathBuilder,
    w: f32,
    on: bool,
    last: Option<Point<Pixels>>,
}

impl Stroke {
    /// `b` (a stroke `w` px wide), noting its segments if armed.
    pub fn new(b: PathBuilder, w: f32) -> Stroke {
        Stroke {
            b,
            w,
            on: armed(),
            last: None,
        }
    }

    pub fn move_to(&mut self, p: Point<Pixels>) {
        self.b.move_to(p);
        self.last = Some(p);
    }

    pub fn line_to(&mut self, p: Point<Pixels>) {
        self.b.line_to(p);
        if self.on
            && let Some(l) = self.last
        {
            REC.with_borrow_mut(|r| {
                if r.pending.len() < r.pending.capacity() {
                    r.pending.push((xy(l), xy(p)));
                }
            });
        }
        self.last = Some(p);
    }

    /// Paint it in `color`; its segments become marks named `what`.
    pub fn paint(self, window: &mut Window, color: Hsla, what: &'static str) {
        if let Ok(path) = self.b.build() {
            window.paint_path(path, color);
        }
        if self.on {
            let rgb = crate::theme::rgb_of(color);
            let w = self.w;
            REC.with_borrow_mut(|r| {
                let Rec { marks, pending, .. } = r;
                for (a, b) in pending.drain(..) {
                    if marks.len() < marks.capacity() {
                        marks.push(Mark {
                            shape: Shape::Seg { a, b, w },
                            rgb,
                            what,
                        });
                    }
                }
            });
        }
    }
}

/// How far `at` is from a mark's shape (0 inside or on it).
fn distance(s: &Shape, at: (f32, f32)) -> f32 {
    match *s {
        Shape::Rect { min, max } => {
            let dx = (min.0 - at.0).max(at.0 - max.0).max(0.0);
            let dy = (min.1 - at.1).max(at.1 - max.1).max(0.0);
            (dx * dx + dy * dy).sqrt()
        }
        Shape::Seg { a, b, w } => {
            let (vx, vy) = (b.0 - a.0, b.1 - a.1);
            let len2 = vx * vx + vy * vy;
            let t = if len2 == 0.0 {
                0.0
            } else {
                (((at.0 - a.0) * vx + (at.1 - a.1) * vy) / len2).clamp(0.0, 1.0)
            };
            let (px, py) = (a.0 + t * vx - at.0, a.1 + t * vy - at.1);
            ((px * px + py * py).sqrt() - w / 2.0).max(0.0)
        }
    }
}

/// The topmost of `marks` (painted last) within [`NEAR`] of `at`.
pub fn nearest(marks: &[Mark], at: (f32, f32)) -> Option<Mark> {
    marks
        .iter()
        .rev()
        .find(|m| distance(&m.shape, at) <= NEAR)
        .copied()
}

/// What the dropper would take at `at` (window px) from the last frame's paint.
pub fn pick(at: Point<Pixels>) -> Option<(Rgb, &'static str)> {
    REC.with_borrow(|r| nearest(&r.marks, xy(at)).map(|m| (m.rgb, m.what)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(a: (f32, f32), b: (f32, f32), hex: u32, what: &'static str) -> Mark {
        Mark {
            shape: Shape::Seg { a, b, w: 2.0 },
            rgb: Rgb::hex(hex),
            what,
        }
    }

    /// The dropper takes the topmost thing drawn near the pointer: a line over a candle wins,
    /// a candle alone is taken, nothing near is nothing.
    #[test]
    fn the_topmost_mark_near_the_pointer_is_taken() {
        let candle = Mark {
            shape: Shape::Rect {
                min: (10.0, 10.0),
                max: (16.0, 40.0),
            },
            rgb: Rgb::hex(0x3fcf9a),
            what: "Up candle",
        };
        let vwap = seg((0.0, 25.0), (100.0, 25.0), 0x8c7bff, "VWAP");
        let marks = [candle, vwap];
        assert_eq!(nearest(&marks, (13.0, 26.0)).unwrap().what, "VWAP");
        assert_eq!(nearest(&marks, (13.0, 15.0)).unwrap().what, "Up candle");
        assert_eq!(
            nearest(&marks, (50.0, 28.0)).unwrap().what,
            "VWAP",
            "within reach of a line"
        );
        assert!(nearest(&marks, (50.0, 60.0)).is_none());
        // A diagonal segment: the distance is to the line, not its box.
        let diag = [seg((0.0, 0.0), (100.0, 100.0), 0xff0000, "Line")];
        assert!(nearest(&diag, (50.0, 52.0)).is_some());
        assert!(nearest(&diag, (50.0, 70.0)).is_none());
    }

    #[test]
    fn nothing_is_noted_unless_armed_and_marks_never_grow_past_their_reserve() {
        arm(false);
        rect(
            Bounds::new(
                gpui::point(gpui::px(0.0), gpui::px(0.0)),
                gpui::size(gpui::px(4.0), gpui::px(4.0)),
            ),
            gpui::red(),
            "x",
        );
        assert!(pick(gpui::point(gpui::px(1.0), gpui::px(1.0))).is_none());
        arm(true);
        begin(true);
        let cap = REC.with_borrow(|r| r.marks.capacity());
        for _ in 0..cap + 10 {
            rect(
                Bounds::new(
                    gpui::point(gpui::px(0.0), gpui::px(0.0)),
                    gpui::size(gpui::px(4.0), gpui::px(4.0)),
                ),
                gpui::red(),
                "x",
            );
        }
        REC.with_borrow(|r| {
            assert_eq!(r.marks.len(), cap);
            assert_eq!(r.marks.capacity(), cap, "never grown while painting");
        });
        end();
        assert_eq!(
            pick(gpui::point(gpui::px(1.0), gpui::px(1.0))).unwrap().1,
            "x"
        );
        // Another chart painting (not picking) notes nothing and keeps the marks.
        begin(false);
        assert!(!armed());
        end();
        assert!(pick(gpui::point(gpui::px(1.0), gpui::px(1.0))).is_some());
        arm(false);
    }
}
