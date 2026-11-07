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
//! never over it when there is room; always inside the chart's plot area (above the time axis),
//! its body scrolling with a visible scrollbar when the area is short, "Save as default" pinned
//! in the footer. The box is never taller than the height it was placed with
//! ([`content_height`], an upper bound of what it draws at that width), so it can't spill.

use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, ScrollHandle, StatefulInteractiveElement, Styled, div,
    prelude::*, px,
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

/// Where the editor goes, chart-local: its box is at most `h` tall.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Place {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// The editor's width in an `area` this wide.
pub fn width(area_w: f32) -> f32 {
    W.min((area_w - 2.0 * MARGIN).max(0.0))
}

/// The editor's spot in an `area` (w, h: the chart's width and the height above its time
/// axis) for content `h` tall, beside `target` (the drawing's extent, x0, y0, x1, y1) when
/// there is room: right of it, left, below, above; else the top right (clear of the price
/// axis). Never past any edge of the area: narrower, shorter and scrolling when it must be.
pub fn place(area: (f32, f32), target: Option<(f32, f32, f32, f32)>, h: f32) -> Place {
    let (pw, ph) = area;
    let w = width(pw);
    let h = h.min((ph - 2.0 * MARGIN).max(0.0));
    let clamp_x = |x: f32| x.min(pw - w - MARGIN).max(MARGIN);
    let clamp_y = |y: f32| y.min(ph - h - MARGIN).max(MARGIN);
    let at = |x: f32, y: f32| Place { x, y, w, h };
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

/// GPUI's default line height for text `size` px (1.618 em).
fn line_h(size: f32) -> f32 {
    (size * 1.618).ceil()
}

/// Lines a text of `chars` characters at `size` px wraps to in `w` px (an upper bound: wide
/// average glyphs).
fn text_lines(chars: usize, size: f32, w: f32) -> f32 {
    let per_line = (w / (size * 0.62)).floor().max(1.0);
    (chars as f32 / per_line).ceil().max(1.0)
}

/// Lines a flex-wrapped row of items `widths` (px, gap 4) takes in `w` px.
fn wrapped_lines(widths: impl Iterator<Item = f32>, w: f32) -> f32 {
    let mut lines = 0.0;
    let mut used = f32::INFINITY;
    for iw in widths {
        if used + 4.0 + iw > w {
            lines += 1.0;
            used = iw;
        } else {
            used += 4.0 + iw;
        }
    }
    f32::max(lines, 1.0)
}

/// A control's drawn width (its focus ring included).
fn control_w(c: Control, on: bool, names: &[&'static str]) -> f32 {
    let label = |s: String| s.chars().count() as f32 * 12.0 * 0.62;
    4.0 + match c {
        Control::Color(_) => HIT,
        c if is_toggle(c) => 8.0 + 14.0 + 8.0 + label(control_label(c, names)),
        c => (16.0 + label(control_label(c, names)) + if on { 16.0 } else { 0.0 }).max(HIT),
    }
}

/// The header and footer (title, "Save as default", the key hint) and the borders.
fn chrome_height(e: &StyleEditor, hint: bool) -> f32 {
    let title = 16.0 + line_h(13.0) + 1.0;
    let save = if e.default_label.is_some() {
        4.0 + 28.0
    } else {
        0.0
    };
    let hint = if hint { 8.0 + line_h(11.0) } else { 4.0 };
    2.0 + title + 1.0 + save + hint
}

/// How tall the editor draws at width `w` (logical px), as an upper bound: rows that wrap,
/// warnings that wrap, the hex entry, header and footer. [`place`] places this height and the
/// box never grows past it; when the area is shorter the body scrolls.
pub fn content_height(
    e: &StyleEditor,
    warnings: &[Warning],
    w: f32,
    names: &[&'static str],
) -> f32 {
    let inner = (w - 2.0 - 24.0).max(1.0);
    let controls_w = (inner - LABEL_W).max(1.0);
    let mut h = chrome_height(e, true) + 8.0; // the body's padding
    if e.parts.len() > 1 {
        h += 24.0 + 4.0 + 1.0;
    }
    for r in e.rows() {
        if let [(Control::SaveDefault, _)] = r.controls.as_slice() {
            continue; // in the footer
        }
        let widths = r.controls.iter().map(|&(c, on)| control_w(c, on, names));
        if r.label == "Color" {
            let lines = wrapped_lines(widths, controls_w);
            h += 4.0 + line_h(12.0) + lines * 24.0 + (lines - 1.0) * 4.0 + 2.0;
            if e.hex.is_some() {
                let hint = "Type 6 hex digits · Enter applies · Esc cancels";
                let lines = wrapped_lines(
                    [80.0, hint.chars().count() as f32 * 12.0 * 0.62].into_iter(),
                    controls_w,
                );
                h += 8.0 + lines * line_h(12.0) + (lines - 1.0) * 8.0;
            }
            if !warnings.is_empty() {
                // Inside the box: margins, padding, the bar and the marker.
                let text_w = (inner - 16.0 - 3.0 - 8.0 - 16.0 - 8.0).max(1.0);
                h += 8.0 + 8.0 + 4.0 * (warnings.len() as f32 - 1.0);
                for wn in warnings {
                    h += text_lines(wn.text.chars().count(), 12.0, text_w) * line_h(12.0);
                }
            }
        } else {
            let lines = wrapped_lines(widths, controls_w);
            h += 4.0 + lines * 24.0 + (lines - 1.0) * 4.0;
        }
    }
    h.ceil()
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
                .border_color(t.control_edge)
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
                .border_color(if on { t.accent } else { t.control_edge })
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
        .border_color(if on { t.accent } else { t.control_edge })
        .text_size(px(12.0))
        .text_color(t.text_main)
        .when(on, |d| {
            d.bg(t.accent.opacity(0.28))
                .font_weight(FontWeight::SEMIBOLD)
                .child("✓")
        })
        .child(label)
}

/// A warning's marker on a neutral chip (the chart's background, an edge that reads): the
/// color with a meaning it looks like, else (low contrast) half text, half background.
fn marker(t: &Theme, swatch: Option<Hsla>) -> gpui::Div {
    let chip = div()
        .mt(px(3.0))
        .size(px(16.0))
        .flex_none()
        .p(px(2.0))
        .rounded_sm()
        .bg(t.bg)
        .border_1()
        .border_color(t.control_edge);
    match swatch {
        Some(c) => chip.child(div().size_full().rounded_sm().bg(c)),
        None => chip.child(
            div()
                .size_full()
                .flex()
                .flex_row()
                .rounded_sm()
                .border_1()
                .border_color(t.text_main)
                .child(div().w_1_2().h_full().bg(t.text_main)),
        ),
    }
}

/// The readability warnings: a neutral surface with a warn-colored bar on the left (never
/// text on the warn color, so the markers keep their contrast).
fn warning_box(t: &Theme, warnings: &[Warning]) -> gpui::Div {
    let mut list = div()
        .flex_1()
        .min_w(px(0.0))
        .px(px(8.0))
        .py(px(4.0))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .text_size(px(12.0))
        .text_color(t.text_main);
    for w in warnings {
        list = list.child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(8.0))
                .child(marker(t, w.swatch.map(|rgb| user_color(rgb, 100))))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .whitespace_normal()
                        .child(w.text.clone()),
                ),
        );
    }
    div()
        .mx(px(12.0))
        .my(px(4.0))
        .flex()
        .flex_row()
        .rounded_sm()
        .bg(t.bg)
        .border_1()
        .border_color(t.control_edge)
        .child(div().w(px(3.0)).flex_none().bg(t.warn))
        .child(list)
}

/// The scrollbar's thumb (top, height) in a body `view` px tall showing `content` px, scrolled
/// `scrolled` px down; `None` when everything fits.
pub fn thumb(view: f32, content: f32, scrolled: f32) -> Option<(f32, f32)> {
    if content <= view + 0.5 || view <= 0.0 {
        return None;
    }
    let h = (view * view / content).max(16.0_f32.min(view));
    let max = content - view;
    let top = (scrolled.clamp(0.0, max) / max) * (view - h);
    Some((top, h))
}

/// The editor at `at`. `content_h`: [`content_height`] at this width; `scroll`: the body's
/// scroll state (its scrollbar shows whenever the body doesn't fit).
pub fn render(
    e: &StyleEditor,
    t: &Theme,
    warnings: &[Warning],
    at: Place,
    content_h: f32,
    scroll: &ScrollHandle,
    cx: &mut Context<ChartView>,
) -> AnyElement {
    // Short of room: the key hint goes first (the keys still work), then the body scrolls.
    let hint = content_h <= at.h;
    let rows = e.rows();
    let focus = (e.row, e.col);
    let names = &t.drawing_names;
    // The rows keep their own heights (flex_none); the scroll container around them shrinks.
    let mut body = div().flex_none().flex().flex_col().py(px(4.0));
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
                        .border_color(t.control_edge)
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
            // row, with the selected one after it when they differ; else the selected one.
            let name = |c: &Control| match c {
                Control::Color(c) => color_name(*c, names),
                c => control_label(*c, names),
            };
            let selected = row.controls.iter().find(|(_, on)| *on);
            let named = match row.controls.get(e.col).filter(|_| e.row == r) {
                Some((c, false)) => Some(match selected {
                    Some((s, _)) => format!("{} · selected: {}", name(c), name(s)),
                    None => name(c),
                }),
                Some((c, true)) => Some(name(c)),
                None => selected.map(|(c, _)| name(c)),
            };
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
        .when(hint, |d| {
            d.child(
                div()
                    .px(px(12.0))
                    .py(px(4.0))
                    .text_size(px(11.0))
                    .text_color(t.text_dim)
                    .whitespace_nowrap()
                    .truncate()
                    .child("↑↓←→ move · Enter pick · Tab next part · Esc close"),
            )
        })
        .when(!hint, |d| d.pb(px(4.0)));
    // The scrollbar: from the body's last layout, else (first frame) from the estimate.
    let view_h = f32::from(scroll.bounds().size.height);
    let (view_h, body_h) = if view_h > 0.0 {
        (view_h, view_h + f32::from(scroll.max_offset().y))
    } else {
        let view = (at.h - chrome_height(e, hint)).max(0.0);
        (view, (content_h - chrome_height(e, true)).max(0.0))
    };
    let bar = thumb(view_h, body_h, -f32::from(scroll.offset().y)).map(|(top, h)| {
        div()
            .absolute()
            .top(px(top))
            .right(px(2.0))
            .w(px(4.0))
            .h(px(h))
            .rounded_full()
            .bg(t.text_dim)
    });
    let body = div()
        .relative()
        .flex_1()
        .min_h(px(0.0))
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(
            div()
                .id("style-editor-body")
                .flex_1()
                .min_h(px(0.0))
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .track_scroll(scroll)
                .child(body),
        )
        .when_some(bar, |d, b| d.child(b));
    div()
        .absolute()
        .left(px(at.x))
        .top(px(at.y))
        .child(
            div()
                .id("style-editor")
                .occlude()
                .w(px(at.w))
                .max_h(px(at.h))
                .overflow_hidden()
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
        assert!(p.h <= pane.1 - 2.0 * MARGIN + 0.01, "{p:?}");
        assert!(p.y + p.h <= pane.1 - MARGIN + 0.01, "{p:?} in {pane:?}");
    }

    fn overlaps(p: Place, h: f32, (x0, y0, x1, y1): (f32, f32, f32, f32)) -> bool {
        let h = h.min(p.h);
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

    /// The tallest editor: a zone's line tab with the hex entry open and every warning the
    /// guard can give (each meaning, named in the longest way, and low contrast).
    fn tallest() -> (StyleEditor, Vec<Warning>) {
        use lines_core::drawings::{Style, Tool};
        let mut e = StyleEditor::for_drawing(Tool::Rect, &Style::DEFAULT);
        let rows = e.rows();
        let custom = rows[0].controls.len() - 1;
        e.click(0, custom); // the hex entry
        assert!(e.hex.is_some());
        let long = " in the dark theme with the colorblind palette, the light theme and the light theme with the colorblind palette";
        let mut w: Vec<Warning> = [
            "up",
            "down",
            "last-price",
            "warning",
            "order target",
            "order stop",
        ]
        .iter()
        .map(|n| Warning {
            text: format!("Looks like the {n} color{long}; may be misread as {n}."),
            swatch: Some(lines_core::style::Rgb::hex(0x3fcf9a)),
        })
        .collect();
        w.push(Warning {
            text: "Low contrast in the dark theme (1.2:1) and the light theme (1.4:1); lines need 3:1."
                .into(),
            swatch: None,
        });
        (e, w)
    }

    /// The placed box (at most the height it was placed with) stays inside the chart's plot
    /// area for the tallest content, in wide, narrow, short and tiled panes, beside any
    /// drawing; when the content fits, it isn't cut.
    #[test]
    fn the_tallest_editor_stays_inside_the_plot_area() {
        let names = crate::theme::Theme::dark(crate::theme::Palette::Standard).drawing_names;
        let (e, warnings) = tallest();
        for area in [
            (1600.0, 876.0),
            (1400.0, 360.0),
            (420.0, 276.0),
            (300.0, 900.0),
            (900.0, 200.0),
            (220.0, 160.0),
            (800.0, 400.0),
        ] {
            let w = width(area.0);
            let h = content_height(&e, &warnings, w, &names);
            for target in [
                None,
                Some((10.0, 10.0, 200.0, 40.0)),
                Some((0.0, area.1 - 12.0, area.0 - 72.0, area.1 - 12.0)),
                Some((area.0 * 0.4, area.1 - 30.0, area.0 * 0.5, area.1 - 10.0)),
            ] {
                let p = place(area, target, h);
                inside(p, area);
                if h <= area.1 - 2.0 * MARGIN {
                    assert_eq!(p.h, h, "fits: not cut ({area:?})");
                }
            }
        }
        // Narrower: the rows and warnings wrap, so it's taller.
        let wide = content_height(&e, &warnings, W, &names);
        let narrow = content_height(&e, &warnings, 260.0, &names);
        assert!(narrow > wide, "{narrow} > {wide}");
        assert!(wide > content_height(&e, &[], W, &names) + 7.0 * 19.0);
    }

    /// The scrollbar shows exactly when the body doesn't fit, its thumb inside the track and
    /// at the bottom when scrolled to the end.
    #[test]
    fn the_scrollbar_shows_when_the_body_scrolls() {
        assert_eq!(thumb(200.0, 180.0, 0.0), None);
        let (top, h) = thumb(100.0, 400.0, 0.0).unwrap();
        assert_eq!((top, h), (0.0, 25.0));
        let (top, h) = thumb(100.0, 400.0, 300.0).unwrap();
        assert!((top + h - 100.0).abs() < 0.01);
        let (top, _) = thumb(100.0, 400.0, 9999.0).unwrap();
        assert!(top <= 75.0 + 0.01);
    }
}
