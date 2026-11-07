//! Rendering of the style editor (`crate::style_editor` is the model): a popover over the
//! chart (`crate::popover::layer`: a press outside closes it and does nothing else), render
//! time only. Every control is clickable and maps to the same model calls as the keys.
//!
//! State never relies on color alone (WCAG 1.4.1, 1.4.11): a selected choice has a check mark,
//! a 1 px accent border and a stronger fill; a selected swatch a 2 px ring and a dot; on/off
//! options are checkboxes; a color row names its focused (else selected) swatch beside its
//! label. Keyboard focus is a separate 2 px accent ring around the control.
//!
//! Placement ([`place`]): beside the edited drawing (right, left, below, above, in that order),
//! never over it when there is room; always inside the pane, its body scrolling when the pane
//! is short, "Save as default" pinned in the footer.

use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, StatefulInteractiveElement, Styled, div, prelude::*, px,
};

use crate::chart_view::ChartView;
use crate::style_editor::{Control, StyleEditor, Warning, color_name, control_label, is_toggle};
use crate::theme::{Theme, user_color};

/// Widest the editor gets (logical px).
pub const W: f32 = 440.0;
/// Kept clear of the pane's edges and of the drawing (logical px).
pub const MARGIN: f32 = 8.0;
const GAP: f32 = 12.0;
/// The label column.
const LABEL_W: f32 = 64.0;
/// Every control is at least this tall and wide (hit target).
const HIT: f32 = 20.0;
/// The price axis, which the default spot keeps clear of.
const AXIS_W: f32 = 72.0;

/// Where the editor goes, chart-local.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Place {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub max_h: f32,
}

/// The editor's spot in a `pane` (w, h) about `h` tall, beside `target` (the drawing's extent,
/// x0, y0, x1, y1) when there is room: right of it, left, below, above; else the top right
/// (clear of the price axis). Never past any pane edge: narrower and scrolling when it must be.
pub fn place(pane: (f32, f32), target: Option<(f32, f32, f32, f32)>, h: f32) -> Place {
    let (pw, ph) = pane;
    let w = W.min((pw - 2.0 * MARGIN).max(0.0));
    let max_h = (ph - 2.0 * MARGIN).max(0.0);
    let h = h.min(max_h);
    let clamp_x = |x: f32| x.min(pw - w - MARGIN).max(MARGIN);
    let clamp_y = |y: f32| y.min(ph - h - MARGIN).max(MARGIN);
    let at = |x: f32, y: f32| Place { x, y, w, max_h };
    if let Some((x0, y0, x1, y1)) = target {
        if x1 + GAP + w <= pw - MARGIN {
            return at(x1 + GAP, clamp_y(y0));
        }
        if x0 - GAP - w >= MARGIN {
            return at(x0 - GAP - w, clamp_y(y0));
        }
        if y1 + GAP + h <= ph - MARGIN {
            return at(clamp_x(x0), y1 + GAP);
        }
        if y0 - GAP - h >= MARGIN {
            return at(clamp_x(x0), y0 - GAP - h);
        }
    }
    at(clamp_x(pw - AXIS_W - w - MARGIN), clamp_y(MARGIN + 16.0))
}

/// About how tall the editor draws (for [`place`]; the body scrolls if this is short).
pub fn estimated_height(e: &StyleEditor, warnings: usize) -> f32 {
    let rows = e.rows();
    let mut h = 36.0 + 24.0; // title, footer hint
    if e.parts.len() > 1 {
        h += 32.0;
    }
    for r in &rows {
        h += match r.label {
            "" => 36.0,      // save as default, in the footer
            "Color" => 52.0, // the name line and the swatches
            _ => 28.0,
        };
    }
    if e.hex.is_some() {
        h += 28.0;
    }
    h + 24.0 * warnings as f32 + if warnings > 0 { 16.0 } else { 0.0 }
}

/// The focus ring around a control: 2 px accent when focused, else the same space clear (so
/// focus never moves anything).
fn ring(t: &Theme, focused: bool) -> gpui::Div {
    div()
        .flex_none()
        .rounded_md()
        .border_2()
        .border_color(if focused {
            t.accent
        } else {
            gpui::transparent_black()
        })
}

fn swatch(t: &Theme, fill: Hsla, on: bool) -> gpui::Div {
    // Selected: a 2 px ring in the text color and a dot inside (never color alone).
    div()
        .size(px(HIT))
        .p(px(2.0))
        .rounded_sm()
        .border_2()
        .border_color(if on {
            t.text_main
        } else {
            gpui::transparent_black()
        })
        .child(
            div()
                .size_full()
                .rounded_sm()
                .bg(fill)
                .border_1()
                .border_color(t.axis_line)
                .flex()
                .items_center()
                .justify_center()
                .when(on, |d| {
                    d.child(div().size(px(4.0)).rounded_full().bg(t.on_fill(fill)))
                }),
        )
}

fn checkbox(t: &Theme, on: bool, label: String) -> gpui::Div {
    div()
        .h(px(HIT))
        .px(px(4.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .text_size(px(12.0))
        .text_color(t.text_main)
        .child(
            div()
                .size(px(14.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .border_1()
                .border_color(if on { t.accent } else { t.text_dim })
                .when(on, |d| {
                    d.bg(t.accent)
                        .text_color(t.on_last)
                        .text_size(px(11.0))
                        .child("✓")
                }),
        )
        .child(label)
}

fn choice(t: &Theme, on: bool, label: String) -> gpui::Div {
    // Selected: a check, a 1 px accent border and a stronger fill (WCAG 1.4.11).
    div()
        .h(px(HIT))
        .min_w(px(HIT))
        .px(px(8.0))
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(4.0))
        .rounded_sm()
        .border_1()
        .border_color(if on { t.accent } else { t.axis_line })
        .text_size(px(12.0))
        .text_color(t.text_main)
        .when(on, |d| {
            d.bg(t.accent.opacity(0.28))
                .font_weight(FontWeight::SEMIBOLD)
                .child("✓")
        })
        .child(label)
}

fn warning_box(t: &Theme, warnings: &[Warning]) -> gpui::Div {
    let mut list = div()
        .mx(px(12.0))
        .my(px(4.0))
        .px(px(8.0))
        .py(px(4.0))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .rounded_sm()
        .bg(t.warn)
        .text_size(px(12.0))
        .text_color(t.on_warn);
    for w in warnings {
        list = list.child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(8.0))
                .when_some(w.swatch, |d, rgb| {
                    // The color it looks like, beside the words.
                    d.child(
                        div()
                            .mt(px(2.0))
                            .size(px(12.0))
                            .flex_none()
                            .rounded_sm()
                            .bg(user_color(rgb, 100))
                            .border_1()
                            .border_color(t.on_warn),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .whitespace_normal()
                        .child(w.text.clone()),
                ),
        );
    }
    list
}

pub fn render(
    e: &StyleEditor,
    t: &Theme,
    warnings: &[Warning],
    at: Place,
    cx: &mut Context<ChartView>,
) -> AnyElement {
    let rows = e.rows();
    let focus = (e.row, e.col);
    let names = &t.drawing_names;
    let mut body = div()
        .id("style-editor-body")
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .py(px(4.0));
    if e.parts.len() > 1 {
        let mut tabs = div()
            .px(px(12.0))
            .pb(px(4.0))
            .flex()
            .flex_row()
            .gap(px(4.0))
            .border_b_1()
            .border_color(t.axis_line);
        for (i, p) in e.parts.iter().enumerate() {
            let on = i == e.tab;
            tabs = tabs.child(
                div()
                    .id(("style-tab", i))
                    .h(px(24.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .text_size(px(12.0))
                    .text_color(if on { t.text_main } else { t.text_dim })
                    .when(on, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .border_b_2()
                    .border_color(if on {
                        t.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .hover(|s| s.text_color(t.text_main))
                    .child(p.title)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view: &mut ChartView, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            view.style_tab(i, cx);
                        }),
                    ),
            );
        }
        body = body.child(tabs);
    }
    let mut footer_save = None;
    for (r, row) in rows.iter().enumerate() {
        let click = |k: usize| {
            cx.listener(move |view: &mut ChartView, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                view.style_click(r, k, cx);
            })
        };
        if let [(Control::SaveDefault, saved)] = row.controls.as_slice() {
            let focused = focus == (r, 0);
            let label = if *saved {
                "Saved as the default".to_owned()
            } else {
                e.default_label
                    .clone()
                    .unwrap_or_else(|| control_label(Control::SaveDefault, names))
            };
            footer_save = Some(
                ring(t, focused).child(
                    div()
                        .id(("style-ctl", r * 64))
                        .h(px(24.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .border_1()
                        .border_color(t.axis_line)
                        .text_size(px(12.0))
                        .text_color(if *saved { t.text_dim } else { t.text_main })
                        .hover(|s| s.bg(t.accent.opacity(0.12)))
                        .child(label)
                        .on_mouse_down(MouseButton::Left, click(0)),
                ),
            );
            continue;
        }
        let is_color = row.label == "Color";
        let mut controls = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(4.0));
        for (k, &(c, on)) in row.controls.iter().enumerate() {
            let focused = focus == (r, k);
            let inner = match c {
                Control::Color(color) => swatch(t, t.style_color(color, &t.drawing, 100), on),
                c if is_toggle(c) => checkbox(t, on, control_label(c, names)),
                c => choice(t, on, control_label(c, names)),
            };
            controls = controls.child(
                ring(t, focused).child(
                    inner
                        .id(("style-ctl", r * 64 + k))
                        .on_mouse_down(MouseButton::Left, click(k)),
                ),
            );
        }
        let label = div()
            .w(px(LABEL_W))
            .flex_none()
            .text_size(px(11.0))
            .text_color(t.text_dim)
            .child(row.label);
        if is_color {
            // The swatch's name beside the label: the focused one while the focus is on this
            // row, else the selected one.
            let named = row
                .controls
                .get(e.col)
                .filter(|_| e.row == r)
                .or_else(|| row.controls.iter().find(|(_, on)| *on))
                .map(|(c, _)| match c {
                    Control::Color(c) => color_name(*c, names),
                    c => control_label(*c, names),
                });
            body = body.child(
                div()
                    .px(px(12.0))
                    .pt(px(4.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(label)
                    .when_some(named, |d, n| {
                        d.child(div().text_size(px(12.0)).text_color(t.text_main).child(n))
                    }),
            );
            body = body.child(
                div()
                    .pl(px(12.0 + LABEL_W))
                    .pr(px(12.0))
                    .pb(px(2.0))
                    .child(controls),
            );
        } else {
            body = body.child(
                div()
                    .px(px(12.0))
                    .py(px(2.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(label)
                    .child(controls),
            );
        }
        if is_color {
            // The custom color entry, then the readability warnings, right under the colors.
            if let Some(hex) = &e.hex {
                body = body.child(
                    div()
                        .pl(px(12.0 + LABEL_W))
                        .pr(px(12.0))
                        .py(px(4.0))
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(8.0))
                        .items_center()
                        .text_size(px(12.0))
                        .child(
                            div()
                                .px(px(8.0))
                                .border_1()
                                .border_color(t.accent)
                                .rounded_sm()
                                .text_color(t.text_main)
                                .child(format!("#{hex}▏")),
                        )
                        .child(
                            div()
                                .text_color(t.text_dim)
                                .child("Type 6 hex digits · Enter applies · Esc cancels"),
                        ),
                );
            }
            if !warnings.is_empty() {
                body = body.child(warning_box(t, warnings));
            }
        }
    }
    let footer = div()
        .flex_none()
        .border_t_1()
        .border_color(t.axis_line)
        .when_some(footer_save, |d, s| {
            d.child(div().px(px(12.0)).pt(px(4.0)).flex().flex_row().child(s))
        })
        .child(
            div()
                .px(px(12.0))
                .py(px(4.0))
                .text_size(px(11.0))
                .text_color(t.text_dim)
                .whitespace_nowrap()
                .truncate()
                .child("↑↓←→ move · Enter pick · Tab next part · Esc close"),
        );
    div()
        .absolute()
        .left(px(at.x))
        .top(px(at.y))
        .child(
            div()
                .id("style-editor")
                .occlude()
                .w(px(at.w))
                .max_h(px(at.max_h))
                .flex()
                .flex_col()
                .bg(t.panel)
                .border_1()
                .border_color(t.axis_line)
                .rounded_md()
                .text_size(px(13.0))
                .child(
                    div()
                        .flex_none()
                        .px(px(12.0))
                        .py(px(8.0))
                        .border_b_1()
                        .border_color(t.axis_line)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text_main)
                        .child(e.title.clone()),
                )
                .child(body)
                .child(footer),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inside(p: Place, pane: (f32, f32)) {
        assert!(p.x >= MARGIN - 0.01 && p.y >= MARGIN - 0.01, "{p:?}");
        assert!(p.x + p.w <= pane.0 - MARGIN + 0.01, "{p:?} in {pane:?}");
        assert!(p.max_h <= pane.1 - 2.0 * MARGIN + 0.01, "{p:?}");
    }

    fn overlaps(p: Place, h: f32, (x0, y0, x1, y1): (f32, f32, f32, f32)) -> bool {
        let h = h.min(p.max_h);
        p.x < x1 && p.x + p.w > x0 && p.y < y1 && p.y + h > y0
    }

    /// Beside the drawing when there's room (right, then left, below, above), never over it.
    #[test]
    fn the_editor_sits_beside_its_drawing() {
        let pane = (1600.0, 900.0);
        let h = 300.0;
        // A note on the left: to its right.
        let note = (100.0, 200.0, 220.0, 224.0);
        let p = place(pane, Some(note), h);
        assert!(p.x >= 220.0 && !overlaps(p, h, note));
        // A note near the right edge: to its left.
        let note = (1300.0, 200.0, 1420.0, 224.0);
        let p = place(pane, Some(note), h);
        assert!(p.x + p.w <= 1300.0 && !overlaps(p, h, note));
        // A horizontal line across the chart: below it, or above when it is low.
        let line = (0.0, 300.0, 1528.0, 300.0);
        let p = place(pane, Some(line), h);
        assert!(p.y > 300.0 && !overlaps(p, h, line));
        let line = (0.0, 800.0, 1528.0, 800.0);
        let p = place(pane, Some(line), h);
        assert!(p.y + h < 800.0 && !overlaps(p, h, line));
        for p in [
            place(pane, Some(note), h),
            place(pane, Some(line), h),
            place(pane, None, h),
        ] {
            inside(p, pane);
        }
    }

    /// A narrow or short pane: never past the left edge, narrower, and its body scrolls.
    #[test]
    fn the_editor_fits_narrow_and_short_panes() {
        for pane in [
            (300.0, 900.0),
            (500.0, 200.0),
            (220.0, 160.0),
            (1600.0, 240.0),
        ] {
            for target in [None, Some((10.0, 10.0, 200.0, 40.0))] {
                let p = place(pane, target, 520.0);
                inside(p, pane);
                assert!(p.w <= W);
            }
        }
        assert_eq!(place((300.0, 900.0), None, 400.0).w, 300.0 - 2.0 * MARGIN);
    }
}
