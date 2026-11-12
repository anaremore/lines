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

/// The scroll cue's height: the Add list's last rows fade into the panel while more is below
/// (never a row cut in half with nothing saying there is more).
const FADE_H: f32 = 24.0;

pub fn render(
    menu: &StudyMenu,
    items: &[Item],
    t: &Theme,
    scroll: &gpui::ScrollHandle,
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
                        .text_size(crate::zoom::ui(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text_dim)
                        .child(h.to_string()),
                );
                i += 1;
            }
            Item::Note(n) => {
                body = body.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_size(crate::zoom::ui(12.0))
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
                    Swatch::Custom(rgb) => Some(crate::theme::user_color(*rgb, 100)),
                    Swatch::None => None,
                };
                let link = |id: (&'static str, usize), text: &'static str, part| {
                    div()
                        .id(id)
                        .px_1()
                        .text_size(crate::zoom::ui(11.0))
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
                        .when(selected, |d| d.bg(t.ghost_selected))
                        .child(
                            div()
                                .w(crate::zoom::ui(12.0))
                                .h(crate::zoom::ui(3.0))
                                .when_some(sw, |d, c| d.bg(c)),
                        )
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
                // Every consecutive control of this group on one line: its name, then its
                // chips, which wrap under the first chip (the value column), never under the
                // name.
                let group = *group;
                let mut line = div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_1();
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
                    // The keys' chip: a 2 px violet outline (the accent: focus), seen on a chip
                    // that is on too; a chip that is on: its fill and a 1 px violet edge; off:
                    // the control edge (3:1). The thicker edge takes a pixel of padding, so
                    // nothing moves.
                    let chip = div()
                        .id(("control", i))
                        .rounded_sm()
                        .when(sel, |d| d.border_2().px(crate::zoom::ui(7.0)).py(px(0.0)))
                        .when(!sel, |d| d.border_1().px(crate::zoom::ui(8.0)).py(px(1.0)))
                        .border_color(if sel || *on { t.accent } else { t.control_edge })
                        .text_size(crate::zoom::ui(12.0))
                        .text_color(t.text_main)
                        .when(*on, |d| d.bg(t.element_selected))
                        .on_mouse_down(MouseButton::Left, click(i, MenuPart::Primary));
                    let chip = match swatch {
                        Swatch::Study(c) => chip.child(
                            div()
                                .w(crate::zoom::ui(14.0))
                                .h(crate::zoom::ui(10.0))
                                .bg(t.study[*c as usize % t.study.len()]),
                        ),
                        Swatch::Custom(rgb) => chip.child(
                            div()
                                .w(crate::zoom::ui(14.0))
                                .h(crate::zoom::ui(10.0))
                                .bg(crate::theme::user_color(*rgb, 100)),
                        ),
                        Swatch::None => chip.child(label.clone()),
                    };
                    line = line.child(chip);
                    i += 1;
                }
                body = body.child(
                    div()
                        .px_3()
                        .py(crate::zoom::ui(2.0))
                        .pl(crate::zoom::ui(32.0))
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap_1()
                        .child(
                            div()
                                .flex_none()
                                .w(crate::zoom::ui(64.0))
                                .text_size(crate::zoom::ui(11.0))
                                .text_color(t.text_dim)
                                .child(group),
                        )
                        .child(line),
                );
            }
            Item::Drop {
                group,
                label,
                options,
                ..
            } => {
                // One field: the value in force (or a summary) and ▾; open, its options float
                // over the menu (`dropdown::dropdown_list`, nested), never moving its lines.
                let open = menu.drop.as_ref().filter(|d| d.at == i);
                let trigger = crate::drop_view::trigger(
                    ("study-drop", i),
                    label.clone(),
                    selected,
                    open.is_some(),
                    false,
                    crate::layout_rec::POPUP,
                    t,
                )
                // As tall as a chip: every editor line keeps one pitch.
                .py(px(1.0))
                .text_size(crate::zoom::ui(12.0))
                .on_mouse_down(MouseButton::Left, click(i, MenuPart::Primary));
                let list = open.map(|d| {
                    let checked = |k: usize| options.get(k).is_some_and(|o| o.on);
                    let el = crate::drop_view::list(
                        "study-drop-list",
                        &d.list,
                        &checked,
                        crate::layout_rec::POPUP + 1,
                        t,
                        cx,
                        |v: &mut ChartView, k, _, cx| v.menu_drop_row(k, cx),
                    );
                    (el, d.scroll.clone(), crate::drop_view::cap(), t.panel)
                });
                let field = crate::dropdown::dropdown_list(trigger, list)
                    .nested()
                    .lint_trigger(label)
                    .on_dismiss(Box::new(cx.listener(
                        |v: &mut ChartView, _: &MouseDownEvent, _, cx| v.menu_drop_close(cx),
                    )));
                body = body.child(group_line(group, field.into_any_element(), t));
                i += 1;
            }
            Item::Step {
                group,
                label,
                less,
                more,
            } => {
                // − value +: the keys' line outlines the value (← / → step it).
                let button = |id: (&'static str, usize), text: &'static str, can: bool, part| {
                    div()
                        .id(id)
                        .flex_none()
                        .w(crate::zoom::ui(22.0))
                        .py(px(1.0))
                        .flex()
                        .justify_center()
                        .rounded_sm()
                        .border_1()
                        .border_color(if can { t.control_edge } else { t.axis_line })
                        .text_color(if can { t.text_main } else { t.text_dim })
                        .when(can, |d| {
                            d.cursor_pointer()
                                .hover(|s| s.bg(t.ghost_selected))
                                .on_mouse_down(MouseButton::Left, click(i, part))
                        })
                        .child(text)
                };
                let field = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .text_size(crate::zoom::ui(12.0))
                    .child(button(
                        ("step-less", i),
                        "−",
                        less.is_some(),
                        MenuPart::Less,
                    ))
                    .child(
                        // One width for every number, right-aligned in tabular digits: the
                        // values and the + buttons line up down the editor.
                        div()
                            .w(crate::zoom::ui(64.0))
                            .px_2()
                            .py(px(1.0))
                            .flex()
                            .justify_end()
                            .font_features(gpui::FontFeatures(std::sync::Arc::new(vec![(
                                "tnum".into(),
                                1,
                            )])))
                            .rounded_sm()
                            .border_1()
                            .border_color(if selected { t.accent } else { t.axis_line })
                            .text_color(t.text_main)
                            .child(label.clone()),
                    )
                    .child(button(
                        ("step-more", i),
                        "+",
                        more.is_some(),
                        MenuPart::More,
                    ));
                body = body.child(group_line(group, field.into_any_element(), t));
                i += 1;
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
                        .when(selected, |d| d.bg(t.ghost_selected))
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
                                    .text_size(crate::zoom::ui(11.0))
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
        .top(crate::zoom::ui(24.0))
        .right(crate::zoom::ui(90.0))
        .child(
            div()
                .id("study-menu")
                .occlude()
                .w(crate::zoom::ui(W))
                .max_h(crate::zoom::ui(620.0))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .flex()
                .flex_col()
                .bg(t.panel)
                .border_1()
                .border_color(t.axis_line)
                .rounded_b_md()
                .text_size(crate::zoom::ui(13.0))
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
                        .child(div().flex_1().min_w_0().truncate().child(query)),
                )
                .child(body),
        )
        .when(
            // More below the visible part (the offset is negative, down to -max_offset).
            scroll.max_offset().y > px(0.0) && -scroll.offset().y < scroll.max_offset().y - px(1.0),
            |d| {
                d.child(
                    div()
                        .absolute()
                        .left(px(1.0))
                        .right(px(1.0))
                        .bottom(px(1.0))
                        .h(crate::zoom::ui(FADE_H))
                        .bg(gpui::linear_gradient(
                            180.0,
                            // The panel, fully transparent: a scroll cue, not a UI state.
                            gpui::linear_color_stop(t.panel.opacity(0.0), 0.0),
                            gpui::linear_color_stop(t.panel, 1.0),
                        )),
                )
            },
        )
        .into_any_element()
}

/// An editor line: the setting's name in the label column, then its field.
fn group_line(group: &str, field: AnyElement, t: &Theme) -> gpui::Div {
    div()
        .px_3()
        .py(crate::zoom::ui(2.0))
        .pl(crate::zoom::ui(32.0))
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(
            div()
                .flex_none()
                .w(crate::zoom::ui(64.0))
                .text_size(crate::zoom::ui(11.0))
                .text_color(t.text_dim)
                .child(group.to_owned()),
        )
        .child(field)
}

/// The anchoring prompt over the plot (a click on a bar anchors; Esc cancels).
pub fn anchor_banner(t: &Theme) -> AnyElement {
    pick_banner(t, "Click a bar to anchor the VWAP · Esc cancels")
}

/// A banner over the plot while a click is awaited (anchoring a VWAP, a chart limit's price).
pub fn pick_banner(t: &Theme, text: &'static str) -> AnyElement {
    div()
        .absolute()
        .top(crate::zoom::ui(30.0))
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
                .text_size(crate::zoom::ui(12.0))
                .text_color(t.text_main)
                .child(text),
        )
        .into_any_element()
}
