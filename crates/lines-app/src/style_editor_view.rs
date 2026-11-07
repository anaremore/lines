//! Rendering of the style editor (`crate::style_editor` is the model): a popover over the
//! chart, render time only (the chart keeps painting live data underneath; the popover
//! occludes only its own panel). Every control is clickable and maps to the same model calls
//! as the keys.
//!
//! State never relies on color alone: the current value of a row has the accent fill (text
//! chips) or a ring plus a dot (color swatches); keyboard focus is the accent border.

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, StatefulInteractiveElement, Styled, div, prelude::*, px,
};

use crate::chart_view::ChartView;
use crate::style_editor::{Control, StyleEditor, control_label};
use crate::theme::{Theme, rgb_of};

const W: f32 = 440.0;

pub fn render(e: &StyleEditor, t: &Theme, cx: &mut Context<ChartView>) -> AnyElement {
    let rows = e.rows();
    let focus = (e.row, e.col);
    let mut body = div().flex().flex_col().py_1();
    if e.parts.len() > 1 {
        let mut tabs = div()
            .px_3()
            .pt_1()
            .pb_1()
            .flex()
            .flex_row()
            .gap_1()
            .border_b_1()
            .border_color(t.axis_line);
        for (i, p) in e.parts.iter().enumerate() {
            let on = i == e.tab;
            tabs = tabs.child(
                div()
                    .id(("style-tab", i))
                    .px_2()
                    .py(px(2.0))
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
    for (r, row) in rows.iter().enumerate() {
        let mut line = div()
            .px_3()
            .py(px(3.0))
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(
                div()
                    .w(px(64.0))
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(t.text_dim)
                    .child(row.label),
            );
        for (k, &(c, on)) in row.controls.iter().enumerate() {
            let focused = focus == (r, k);
            let click = cx.listener(move |view: &mut ChartView, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                view.style_click(r, k, cx);
            });
            let chip = match c {
                Control::Color(color) => {
                    // A swatch: a ring when current (plus a dot inside: never color alone),
                    // the accent border when focused.
                    let fill = t.style_color(color, &t.drawing, 100);
                    div()
                        .id(("style-ctl", r * 64 + k))
                        .w(px(20.0))
                        .h(px(18.0))
                        .p(px(2.0))
                        .rounded_sm()
                        .border_2()
                        .border_color(if focused {
                            t.accent
                        } else if on {
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
                                    d.child(
                                        div()
                                            .w(px(4.0))
                                            .h(px(4.0))
                                            .rounded_full()
                                            .bg(t.on_fill(fill)),
                                    )
                                }),
                        )
                        .on_mouse_down(MouseButton::Left, click)
                }
                Control::SaveDefault => {
                    let label = if on {
                        "Saved as the default".to_owned()
                    } else {
                        e.default_label.clone().unwrap_or_else(|| control_label(c))
                    };
                    div()
                        .id(("style-ctl", r * 64 + k))
                        .mt_1()
                        .px_2()
                        .py(px(2.0))
                        .rounded_sm()
                        .border_1()
                        .border_color(if focused { t.accent } else { t.axis_line })
                        .text_size(px(12.0))
                        .text_color(if on { t.text_dim } else { t.text_main })
                        .hover(|s| s.bg(t.accent.opacity(0.12)))
                        .child(label)
                        .on_mouse_down(MouseButton::Left, click)
                }
                _ => div()
                    .id(("style-ctl", r * 64 + k))
                    .px_2()
                    .py(px(1.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(if focused { t.accent } else { t.axis_line })
                    .text_size(px(12.0))
                    .text_color(t.text_main)
                    .when(on, |d| d.bg(t.accent.opacity(0.22)))
                    .child(control_label(c))
                    .on_mouse_down(MouseButton::Left, click),
            };
            line = line.child(chip);
        }
        body = body.child(line);
        // The custom color entry sits under its color row.
        if let Some(hex) = &e.hex
            && row.label == "Color"
        {
            body = body.child(
                div()
                    .px_3()
                    .py(px(2.0))
                    .pl(px(80.0))
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .text_size(px(12.0))
                    .child(
                        div()
                            .px_2()
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
    }
    // The readability guard: words on the warning color, never a block.
    let warning = e.warning(
        |c| rgb_of(t.style_color(c, &t.drawing, 100)),
        rgb_of(t.bg),
        rgb_of(t.panel),
        &t.meanings(),
    );
    if let Some(w) = warning {
        body = body.child(
            div().px_3().py_1().child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(t.warn)
                    .text_size(px(12.0))
                    .text_color(t.on_warn)
                    .child(format!("Readability: {w}")),
            ),
        );
    }
    div()
        .absolute()
        .top(px(24.0))
        .right(px(90.0))
        .child(
            div()
                .id("style-editor")
                .occlude()
                .w(px(W))
                .max_h(px(560.0))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .bg(t.panel)
                .border_1()
                .border_color(t.axis_line)
                .rounded_md()
                .text_size(px(13.0))
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(t.axis_line)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text_main)
                        .child(e.title.clone()),
                )
                .child(body)
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(t.axis_line)
                        .text_size(px(11.0))
                        .text_color(t.text_dim)
                        .whitespace_nowrap()
                        .truncate()
                        .child("↑↓←→ move · Enter pick · Tab next part · Esc close"),
                ),
        )
        .into_any_element()
}
