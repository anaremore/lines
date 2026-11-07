//! Rendering of the studies menu (`crate::study_menu` is the model). Render time only: the
//! chart keeps painting live data underneath; the menu occludes only its own panel.
//!
//! Every row and control is clickable and maps to the same commands as the keys: a study
//! row's label opens its editor (the gear), "hide"/"show" is its eye, "remove" removes it.

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, StatefulInteractiveElement, Styled, div, prelude::*, px,
};

use crate::chart_view::{ChartView, MenuPart};
use crate::study_menu::{Item, StudyMenu, Swatch};
use crate::theme::Theme;

const W: f32 = 420.0;

pub fn render(
    menu: &StudyMenu,
    items: &[Item],
    t: &Theme,
    cx: &mut Context<ChartView>,
) -> AnyElement {
    let click = |i: usize, part: MenuPart| {
        cx.listener(move |view: &mut ChartView, _: &MouseDownEvent, _, cx| {
            cx.stop_propagation();
            view.menu_click(i, part, cx);
        })
    };
    let mut body = div().flex().flex_col().py_1();
    // Controls of one setting share a line: "Reset  [ETH] [RTH] …".
    let mut i = 0;
    while i < items.len() {
        let selected = i == menu.cursor;
        match &items[i] {
            Item::Header(h) => {
                body = body.child(
                    div()
                        .px_3()
                        .pt_2()
                        .pb_1()
                        .text_size(px(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text_dim)
                        .child(*h),
                );
                i += 1;
            }
            Item::Note(n) => {
                body = body.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_size(px(12.0))
                        .text_color(t.text_dim)
                        .truncate()
                        .child(n.clone()),
                );
                i += 1;
            }
            Item::Study {
                label,
                swatch,
                hidden,
                editing,
                ..
            } => {
                let sw = match swatch {
                    Swatch::Study(c) => Some(t.study[*c as usize % t.study.len()]),
                    Swatch::None => None,
                };
                let link = |id: (&'static str, usize), text: &'static str, part| {
                    div()
                        .id(id)
                        .px_1()
                        .text_size(px(11.0))
                        .text_color(t.text_dim)
                        .hover(|s| s.text_color(t.text_main))
                        .child(text)
                        .on_mouse_down(MouseButton::Left, click(i, part))
                };
                body = body.child(
                    div()
                        .id(("study", i))
                        .px_3()
                        .py_1()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .border_l_2()
                        .border_color(if selected {
                            t.accent
                        } else {
                            gpui::transparent_black()
                        })
                        .when(selected, |d| d.bg(t.accent.opacity(0.16)))
                        .child(div().w(px(12.0)).h(px(3.0)).when_some(sw, |d, c| d.bg(c)))
                        .child(
                            div()
                                .id(("study-label", i))
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(if *hidden { t.text_dim } else { t.text_main })
                                .child(label.clone())
                                .on_mouse_down(MouseButton::Left, click(i, MenuPart::Primary)),
                        )
                        .child(link(
                            ("study-eye", i),
                            if *hidden { "show" } else { "hide" },
                            MenuPart::Eye,
                        ))
                        .child(link(
                            ("study-gear", i),
                            if *editing { "done" } else { "edit" },
                            MenuPart::Primary,
                        ))
                        .child(link(("study-del", i), "remove", MenuPart::Delete)),
                );
                i += 1;
            }
            Item::Control { group, .. } => {
                // Every consecutive control of this group on one line.
                let group = *group;
                let mut line = div()
                    .px_3()
                    .py(px(2.0))
                    .pl(px(32.0))
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .w(px(64.0))
                            .text_size(px(11.0))
                            .text_color(t.text_dim)
                            .child(group),
                    );
                while let Some(Item::Control {
                    group: g,
                    label,
                    on,
                    swatch,
                    ..
                }) = items.get(i)
                {
                    if *g != group {
                        break;
                    }
                    let sel = i == menu.cursor;
                    let chip = div()
                        .id(("control", i))
                        .px_2()
                        .py(px(1.0))
                        .rounded_sm()
                        .border_1()
                        .border_color(if sel { t.accent } else { t.axis_line })
                        .text_size(px(12.0))
                        .text_color(t.text_main)
                        .when(*on, |d| d.bg(t.accent.opacity(0.22)))
                        .on_mouse_down(MouseButton::Left, click(i, MenuPart::Primary));
                    let chip = match swatch {
                        Swatch::Study(c) => chip.child(
                            div()
                                .w(px(14.0))
                                .h(px(10.0))
                                .bg(t.study[*c as usize % t.study.len()]),
                        ),
                        Swatch::None => chip.child(label.clone()),
                    };
                    line = line.child(chip);
                    i += 1;
                }
                body = body.child(line);
            }
            Item::Action { label, delete, .. } => {
                body = body.child(
                    div()
                        .id(("action", i))
                        .px_3()
                        .py_1()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .border_l_2()
                        .border_color(if selected {
                            t.accent
                        } else {
                            gpui::transparent_black()
                        })
                        .when(selected, |d| d.bg(t.accent.opacity(0.16)))
                        .child(
                            div()
                                .id(("action-label", i))
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(t.text_main)
                                .child(label.clone())
                                .on_mouse_down(MouseButton::Left, click(i, MenuPart::Primary)),
                        )
                        .when(delete.is_some(), |d| {
                            d.child(
                                div()
                                    .id(("action-del", i))
                                    .px_1()
                                    .text_size(px(11.0))
                                    .text_color(t.text_dim)
                                    .hover(|s| s.text_color(t.text_main))
                                    .child("delete")
                                    .on_mouse_down(MouseButton::Left, click(i, MenuPart::Delete)),
                            )
                        }),
                );
                i += 1;
            }
        }
    }
    let query = if menu.query.is_empty() {
        div()
            .text_color(t.text_dim)
            .child("Type to search studies to add")
    } else {
        div()
            .text_color(t.text_main)
            .child(format!("{}▏", menu.query))
    };
    div()
        .absolute()
        .top(px(24.0))
        .right(px(90.0))
        .child(
            div()
                .id("study-menu")
                .occlude()
                .w(px(W))
                .max_h(px(620.0))
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
                        .flex()
                        .flex_row()
                        .gap_3()
                        .items_center()
                        .border_b_1()
                        .border_color(t.axis_line)
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(t.text_main)
                                .child("Studies"),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(query)),
                )
                .child(body)
                .child(
                    // One short line, never wrapped.
                    div()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(t.axis_line)
                        .text_size(px(11.0))
                        .text_color(t.text_dim)
                        .whitespace_nowrap()
                        .truncate()
                        .child("↑↓ move · Enter edit/add · Space hide · Del remove · Esc back"),
                ),
        )
        .into_any_element()
}

/// The anchoring prompt over the plot (a click on a bar anchors; Esc cancels).
pub fn anchor_banner(t: &Theme) -> AnyElement {
    pick_banner(t, "Click a bar to anchor the VWAP · Esc cancels")
}

/// A banner over the plot while a click is awaited (anchoring a VWAP, a chart limit's price).
pub fn pick_banner(t: &Theme, text: &'static str) -> AnyElement {
    div()
        .absolute()
        .top(px(30.0))
        .left_0()
        .right_0()
        .flex()
        .flex_row()
        .justify_center()
        .child(
            div()
                .px_3()
                .py_1()
                .bg(t.label_bg)
                .border_1()
                .border_color(t.accent)
                .rounded_md()
                .text_size(px(12.0))
                .text_color(t.text_main)
                .child(text),
        )
        .into_any_element()
}
