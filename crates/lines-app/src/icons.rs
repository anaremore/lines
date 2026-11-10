//! Small line icons for pane headers and tabs, drawn in code (no vendored artwork, so no
//! third-party licence): polylines on a 16-unit grid, stroked 1.5 px, every point snapped to
//! the device-pixel grid when painted so lines stay crisp at any DPI. Original shapes, not
//! copied from any trading platform. Colors come from the caller (theme tokens).

use gpui::{
    Bounds, Div, ElementId, Hsla, InteractiveElement, ParentElement, PathBuilder, Pixels, Stateful,
    Styled, Window, canvas, div, point, px,
};

use crate::commands::Command;
use crate::theme::Theme;

/// The icon grid (units per side) and the stroke width (logical px).
pub const GRID: f32 = 16.0;
pub const STROKE: f32 = 1.5;
/// A clickable icon's box (logical px): never under 20 × 20.
pub const HIT: f32 = 20.0;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Icon {
    /// A square with an arrow leaving up-right: pop the pane out into its own window.
    PopOut,
    /// An arrow coming into a square: re-attach a popped-out pane where it was.
    Reattach,
    /// Two panes side by side, an arrow into the right one: dock beside the chart.
    Dock,
    /// An eye with a stroke through it: hide the pane.
    Hide,
    /// Four corners pointing out: chart only (the chart takes the space).
    ChartOnly,
    /// Three dots: the pane menu.
    Menu,
    /// ×: close.
    Close,
    /// A pushpin: a pinned tab (in the tab's close slot).
    Pin,
    /// A triangle pointing right: play (the replay bar).
    Play,
    /// Two bars: pause.
    Pause,
    /// A bar and a triangle pointing left: the previous session open.
    SkipBack,
    /// A triangle pointing right and a bar: the next session open.
    SkipForward,
}

impl Icon {
    #[cfg(test)]
    pub const ALL: [Icon; 12] = [
        Icon::Play,
        Icon::Pause,
        Icon::SkipBack,
        Icon::SkipForward,
        Icon::PopOut,
        Icon::Reattach,
        Icon::Dock,
        Icon::Hide,
        Icon::ChartOnly,
        Icon::Menu,
        Icon::Close,
        Icon::Pin,
    ];

    /// The polylines, in grid units (0..=16, y down).
    pub fn lines(self) -> &'static [&'static [(f32, f32)]] {
        match self {
            Icon::PopOut => &[
                // The box, open at the top right.
                &[
                    (8.0, 3.0),
                    (3.0, 3.0),
                    (3.0, 13.0),
                    (13.0, 13.0),
                    (13.0, 8.0),
                ],
                // The arrow up-right.
                &[(7.0, 9.0), (13.0, 3.0)],
                &[(9.0, 3.0), (13.0, 3.0), (13.0, 7.0)],
            ],
            Icon::Reattach => &[
                &[
                    (8.0, 3.0),
                    (3.0, 3.0),
                    (3.0, 13.0),
                    (13.0, 13.0),
                    (13.0, 8.0),
                ],
                // The arrow down-left, into the box.
                &[(13.0, 3.0), (7.0, 9.0)],
                &[(7.0, 5.0), (7.0, 9.0), (11.0, 9.0)],
            ],
            Icon::Dock => &[
                &[
                    (2.0, 3.0),
                    (14.0, 3.0),
                    (14.0, 13.0),
                    (2.0, 13.0),
                    (2.0, 3.0),
                ],
                &[(8.0, 3.0), (8.0, 13.0)],
                // The arrow into the right pane.
                &[(4.0, 8.0), (12.0, 8.0)],
                &[(10.0, 6.0), (12.0, 8.0), (10.0, 10.0)],
            ],
            Icon::Hide => &[
                // The eye's lids.
                &[(2.0, 8.0), (5.0, 5.0), (8.0, 4.0), (11.0, 5.0), (14.0, 8.0)],
                &[
                    (2.0, 8.0),
                    (5.0, 11.0),
                    (8.0, 12.0),
                    (11.0, 11.0),
                    (14.0, 8.0),
                ],
                // The stroke through it (no pupil: at 16 px it merged with the stroke).
                &[(3.0, 13.0), (13.0, 3.0)],
            ],
            Icon::ChartOnly => &[
                &[(2.0, 6.0), (2.0, 2.0), (6.0, 2.0)],
                &[(10.0, 2.0), (14.0, 2.0), (14.0, 6.0)],
                &[(14.0, 10.0), (14.0, 14.0), (10.0, 14.0)],
                &[(6.0, 14.0), (2.0, 14.0), (2.0, 10.0)],
            ],
            Icon::Menu => &[
                // Three dots: 2-unit squares on whole grid units, closed (solid when stroked).
                &[(2.0, 7.0), (4.0, 7.0), (4.0, 9.0), (2.0, 9.0), (2.0, 7.0)],
                &[(7.0, 7.0), (9.0, 7.0), (9.0, 9.0), (7.0, 9.0), (7.0, 7.0)],
                &[
                    (12.0, 7.0),
                    (14.0, 7.0),
                    (14.0, 9.0),
                    (12.0, 9.0),
                    (12.0, 7.0),
                ],
            ],
            Icon::Close => &[&[(4.0, 4.0), (12.0, 12.0)], &[(12.0, 4.0), (4.0, 12.0)]],
            Icon::Pin => &[
                // The head, the body and the base, then the needle.
                &[(5.0, 2.0), (11.0, 2.0)],
                &[(6.0, 2.0), (6.0, 8.0)],
                &[(10.0, 2.0), (10.0, 8.0)],
                &[(4.0, 9.0), (12.0, 9.0)],
                &[(8.0, 9.0), (8.0, 14.0)],
            ],
            Icon::Play => &[&[(5.0, 3.0), (13.0, 8.0), (5.0, 13.0), (5.0, 3.0)]],
            Icon::Pause => &[&[(5.0, 3.0), (5.0, 13.0)], &[(11.0, 3.0), (11.0, 13.0)]],
            Icon::SkipBack => &[
                &[(4.0, 3.0), (4.0, 13.0)],
                &[(12.0, 3.0), (6.0, 8.0), (12.0, 13.0), (12.0, 3.0)],
            ],
            Icon::SkipForward => &[
                &[(12.0, 3.0), (12.0, 13.0)],
                &[(4.0, 3.0), (10.0, 8.0), (4.0, 13.0), (4.0, 3.0)],
            ],
        }
    }
}

/// The stroke as drawn at `scale`: `STROKE` rounded to whole device pixels (at least one), in
/// logical px. [`snap`] places points for exactly this width, so every straight edge covers
/// whole pixels (1.0: 2 device px on pixel edges; 2.0: 3 device px on pixel centers).
pub fn stroke_width(scale: f32) -> f32 {
    let s = scale.max(1.0);
    (STROKE * s).round().max(1.0) / s
}

/// Snap a logical coordinate for a stroke of [`stroke_width`]: odd device widths on pixel
/// centers, even ones on pixel edges.
pub fn snap(v: f32, scale: f32) -> f32 {
    let s = scale.max(1.0);
    let w = (stroke_width(s) * s).round();
    // Odd device widths sit on pixel centers, even ones on pixel edges.
    let offset = if w as i32 % 2 == 1 { 0.5 } else { 0.0 };
    ((v * s - offset).round() + offset) / s
}

/// Paint `icon` centered in `bounds` (logical px) in `color`. Builds a few short GPUI paths
/// (GPUI tessellates them per frame, like the chart's study lines): for the hovered pane's
/// header controls and the tab bar's close buttons, never for ladder or chart rows.
pub fn paint(icon: Icon, bounds: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let scale = window.scale_factor();
    let side = f32::from(bounds.size.width).min(f32::from(bounds.size.height));
    let unit = side.min(GRID) / GRID;
    let x0 = f32::from(bounds.origin.x) + (f32::from(bounds.size.width) - GRID * unit) * 0.5;
    let y0 = f32::from(bounds.origin.y) + (f32::from(bounds.size.height) - GRID * unit) * 0.5;
    for line in icon.lines() {
        let mut b = PathBuilder::stroke(px(stroke_width(scale)));
        // A line ending where it began is a closed shape: closed with a join, so its first
        // corner isn't cut by the stroke's flat end.
        let closed = line.len() > 2 && line.first() == line.last();
        let n = if closed { line.len() - 1 } else { line.len() };
        for (i, &(x, y)) in line[..n].iter().enumerate() {
            let p = point(
                px(snap(x0 + x * unit, scale)),
                px(snap(y0 + y * unit, scale)),
            );
            if i == 0 {
                b.move_to(p);
            } else {
                b.line_to(p);
            }
        }
        if closed {
            b.close();
        }
        if let Ok(path) = b.build() {
            window.paint_path(path, color);
        }
    }
}

/// An icon that is only shown (a pinned tab's pin): [`HIT`] px square, in `color`.
pub fn glyph(icon: Icon, color: Hsla) -> Div {
    div().flex_none().w(px(HIT)).h(px(HIT)).child(
        canvas(
            |_, _, _| (),
            move |b, (), window, _| paint(icon, b, color, window),
        )
        .size_full(),
    )
}

/// A clickable icon: [`HIT`] px square, `text_dim` at rest, `text_main` under the pointer,
/// `accent` while `on` (e.g. chart only active). Carries its tooltip ("Pop out" with its key as caps,
/// after `tooltip::DELAY`), the button role and `name` as its accessible label. The caller
/// adds the click.
pub fn button(
    id: impl Into<ElementId>,
    icon: Icon,
    name: &str,
    cmd: Option<Command>,
    on: bool,
    t: &Theme,
) -> Stateful<Div> {
    let (dim, main, accent, hover_bg) = (t.text_dim, t.text_main, t.accent, t.axis_line);
    let el = div()
        .id(id)
        .flex_none()
        .w(px(HIT))
        .h(px(HIT))
        .rounded_sm()
        .cursor_pointer()
        // The hover style also makes the window repaint when the pointer enters or leaves.
        .hover(move |s| s.bg(hover_bg))
        .child(
            canvas(
                |_, _, _| (),
                move |b, (), window, _| {
                    // The pointer's last position stays after it leaves the window (no exit
                    // event on Windows): only a hovered window lights an icon.
                    let hot = window.is_window_hovered() && b.contains(&window.mouse_position());
                    let c = if on {
                        accent
                    } else if hot {
                        main
                    } else {
                        dim
                    };
                    paint(icon, b, c, window);
                },
            )
            .size_full(),
        );
    crate::tooltip::with_tip(el, name, cmd, t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every icon stays inside its grid with a margin (the stroke never clips), has at least
    /// one line of two points, and no two icons are drawn the same.
    #[test]
    fn icons_fit_their_grid_and_differ() {
        for icon in Icon::ALL {
            let lines = icon.lines();
            assert!(!lines.is_empty(), "{icon:?}");
            for l in lines {
                assert!(l.len() >= 2, "{icon:?}");
                for &(x, y) in *l {
                    let m = STROKE / 2.0;
                    assert!(
                        (m..=GRID - m).contains(&x) && (m..=GRID - m).contains(&y),
                        "{icon:?} ({x}, {y})"
                    );
                }
            }
        }
        for (i, a) in Icon::ALL.iter().enumerate() {
            for b in &Icon::ALL[i + 1..] {
                assert_ne!(a.lines(), b.lines(), "{a:?} vs {b:?}");
            }
        }
        const { assert!(HIT >= 20.0) };
    }

    /// Points land on device-pixel centers (odd stroke widths) at DPI 1.0 and 2.0.
    #[test]
    fn strokes_snap_to_device_pixels() {
        // 1.5 px at 1.0 is drawn 2 device px wide (even): on pixel edges.
        assert_eq!(stroke_width(1.0), 2.0);
        assert_eq!(snap(10.3, 1.0), 10.0);
        // 1.5 px at 2.0 is 3 device px (odd): on pixel centers (half device pixels).
        assert_eq!(stroke_width(2.0), 1.5);
        assert_eq!(snap(10.3, 2.0), 10.25);
        assert_eq!(snap(10.0, 2.0), 10.25);
        // At every common scale a snapped straight edge covers whole device pixels: both
        // sides of the drawn stroke land on pixel boundaries.
        for scale in [1.0_f32, 1.25, 1.5, 1.75, 2.0, 3.0] {
            let w = stroke_width(scale) * scale;
            assert!((w - w.round()).abs() < 1e-4 && w >= 1.0, "{scale}");
            for v in [3.0_f32, 7.6, 10.3, 13.9] {
                let c = snap(v, scale) * scale;
                for edge in [c - w / 2.0, c + w / 2.0] {
                    assert!((edge - edge.round()).abs() < 1e-3, "{scale} {v}: {edge}");
                }
            }
        }
    }
}
