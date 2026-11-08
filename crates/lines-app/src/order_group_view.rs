//! The list a chart's group tag opens: one row per order (its tag's words, what it makes or loses
//! there, and an × for that order), and Cancel all, which takes a second press. A group never
//! hides a working order: every one is here, and zooming in gives each its own tag back.

use gpui::{
    AnyElement, App, Context, FontWeight, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, StatefulInteractiveElement, Styled, div, px,
};
use lines_core::FixedStr;

use crate::chart_trade::{money_per_unit, write_exit_pnl, write_order_tag};
use crate::chart_view::ChartView;
use crate::popover::{self, Kind};
use crate::theme::Theme;

const WIDTH: f32 = 330.0;
const ROW_H: f32 = 22.0;
/// Rows shown before the list scrolls.
const ROWS_SHOWN: usize = 12;

/// Cancel all's words: how many, and that a second press is needed once armed.
pub fn cancel_all_label(n: usize, armed: bool) -> String {
    if armed {
        format!("Press again to cancel {n} orders")
    } else {
        format!("Cancel all {n}")
    }
}

/// The popover, if a group tag opened one. Orders that finished leave the list (disarming Cancel
/// all); when none are left it closes.
pub fn layer(v: &mut ChartView, t: &Theme, cx: &mut Context<ChartView>) -> Option<AnyElement> {
    let marks = v.ct.marks.clone();
    let menu = v.ct.group_menu.as_mut()?;
    menu.retain_open(|o| marks.iter().any(|m| m.id == o));
    if menu.orders.is_empty() {
        v.close_popover(Kind::OrderGroup, cx);
        return None;
    }
    let menu = v.ct.group_menu.as_ref()?;
    let fmt = v.instrument().price;
    let mpu = money_per_unit(v.instrument());
    let mode = cx
        .try_global::<crate::picker_view::AppSettings>()
        .map(|a| a.settings.pnl_display)
        .unwrap_or_default();
    let position = v.ct.position;
    let rows = menu
        .orders
        .iter()
        .filter_map(|o| marks.iter().find(|m| m.id == *o).copied())
        .map(|m| {
            let mut text = FixedStr::<96>::new();
            let _ = write_order_tag(&mut text, &m, &fmt);
            let mut pnl = FixedStr::<48>::new();
            let shown = write_exit_pnl(&mut pnl, &m, position, fmt.tick, mpu, mode)
                .ok()
                .flatten()
                .is_some();
            let id = m.id.0;
            div()
                .id(("group-order", id))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .h(px(ROW_H))
                .flex_shrink_0()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_nowrap()
                        .text_color(t.text_main)
                        .child(text.as_str().to_owned()),
                )
                .children(shown.then(|| {
                    div()
                        .flex_shrink_0()
                        .text_color(t.text_dim)
                        .child(pnl.as_str().to_owned())
                }))
                .child(
                    div()
                        .id(("group-cancel", id))
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_shrink_0()
                        .w(px(ROW_H))
                        .h(px(ROW_H))
                        .border_1()
                        .border_color(t.axis_line)
                        .text_color(t.text_main)
                        .cursor_pointer()
                        .child("×")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |v, _: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                v.cancel_order(id, cx);
                                cx.notify();
                            }),
                        ),
                )
        })
        .collect::<Vec<_>>();
    let n = menu.orders.len();
    let armed = menu.confirm == Some(n);
    let plot_h = v.plot_height();
    let list_h = ROW_H * n.min(ROWS_SHOWN) as f32;
    // Under the tag (it stays in view), kept inside the plot.
    let top = (menu.top + lines_core::geometry::order_tags::TAG_H + 6.0)
        .clamp(4.0, (plot_h - list_h - 70.0).max(4.0));
    let cancel = div()
        .id("group-cancel-all")
        .flex()
        .items_center()
        .justify_center()
        .h(px(ROW_H + 2.0))
        .border_1()
        .border_color(t.warn)
        .font_weight(FontWeight::BOLD)
        .cursor_pointer()
        .child(cancel_all_label(n, armed))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|v, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                let ids = v.ct.group_menu.as_mut().and_then(|m| m.cancel_all());
                for id in ids.into_iter().flatten() {
                    v.cancel_order(id.0, cx);
                }
                cx.notify();
            }),
        );
    let cancel = if armed {
        cancel.bg(t.warn).text_color(t.on_warn)
    } else {
        cancel.text_color(t.text_main)
    };
    let list = div()
        .absolute()
        .right(px(70.0))
        .top(px(top))
        .w(px(WIDTH))
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .bg(t.panel)
        .border_1()
        .border_color(t.axis_line)
        .rounded_md()
        .text_size(px(12.0))
        .on_mouse_down(MouseButton::Left, |_, _, cx: &mut App| {
            cx.stop_propagation()
        })
        .child(
            div()
                .id("group-orders")
                .flex()
                .flex_col()
                .h(px(list_h))
                .overflow_y_scroll()
                .children(rows),
        )
        .child(cancel);
    let dismiss: popover::OnDismiss =
        Box::new(cx.listener(|v, _: &MouseDownEvent, _, cx| v.close_popover(Kind::OrderGroup, cx)));
    Some(popover::layer(list, dismiss).into_any_element())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_all_says_what_the_next_press_does() {
        assert_eq!(cancel_all_label(20, false), "Cancel all 20");
        assert_eq!(
            cancel_all_label(20, true),
            "Press again to cancel 20 orders"
        );
    }
}
