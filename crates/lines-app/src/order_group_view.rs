//! The list a chart's group tag opens: one row per order (its tag's words, what it makes or loses
//! there, and an × for that order), and Cancel all, which takes a second press. A group never
//! hides a working order: every one is here, and zooming in gives each its own tag back.
//!
//! Rows never move while the list is open (design.md: nothing the trader aims at shifts under
//! the cursor): an order that finishes stays in its row, marked done and inert. Presses act on
//! the click, on the element pressed.

use gpui::{
    AnyElement, App, ClickEvent, Context, FontWeight, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, StatefulInteractiveElement, Styled, Window, div, px,
};
use lines_core::FixedStr;

use crate::chart_trade::{money_per_unit, write_exit_pnl, write_order_tag};
use crate::chart_view::ChartView;
use crate::popover::{self, Kind};
use crate::theme::Theme;

const WIDTH: f32 = 330.0;
const ROW_H: f32 = 24.0;
/// Rows shown before the list scrolls.
const ROWS_SHOWN: usize = 12;
/// The list's header, its gaps and padding, and Cancel all.
const CHROME_H: f32 = 86.0;
/// Past the plot's right edge: the price axis stays clear.
const MARGIN: f32 = 4.0;

/// Cancel all's words: how many, and that a second press is needed once armed.
pub fn cancel_all_label(n: usize, armed: bool) -> String {
    let orders = if n == 1 { "order" } else { "orders" };
    if armed {
        format!("Press again to cancel {n} {orders}")
    } else {
        format!("Cancel {n} {orders}")
    }
}

/// The list's height for `rows` rows with `avail` px from the plot's top to the window's bottom:
/// at most [`ROWS_SHOWN`] rows, fewer if the window is short, always one; and its top, under the
/// tag if there is room, moved up to keep Cancel all in the window.
pub fn list_place(rows: usize, tag_top: f32, avail: f32) -> (f32, f32) {
    let wanted = ROW_H * rows.min(ROWS_SHOWN) as f32;
    let list_h = wanted.min(avail - CHROME_H - MARGIN).max(ROW_H);
    let under = tag_top + lines_core::geometry::order_tags::TAG_H + 6.0;
    let top = under.min(avail - list_h - CHROME_H).max(MARGIN);
    (list_h, top)
}

/// The popover, if a group tag opened one.
pub fn layer(
    v: &ChartView,
    window: &Window,
    t: &Theme,
    cx: &mut Context<ChartView>,
) -> Option<AnyElement> {
    let menu = v.ct.group_menu.as_ref()?;
    let fmt = v.instrument().price;
    let mpu = money_per_unit(v.instrument());
    let mode = cx
        .try_global::<crate::picker_view::AppSettings>()
        .map(|a| a.settings.pnl_display)
        .unwrap_or_default();
    let position = v.ct.position;
    let rows = menu
        .rows
        .iter()
        .map(|r| {
            // An open order as it stands now; a finished one as it was, marked done.
            let m = v.ct.mark(r.mark.id.0).copied().unwrap_or(r.mark);
            let mut text = FixedStr::<96>::new();
            let _ = write_order_tag(&mut text, &m, &fmt);
            let mut pnl = FixedStr::<48>::new();
            let money = write_exit_pnl(&mut pnl, &m, position, fmt.tick, mpu, mode)
                .ok()
                .flatten()
                .filter(|_| r.open);
            let id = m.id.0;
            let tone = money.map(|money| match crate::pnl::tone(money) {
                crate::pnl::Tone::Up => t.up,
                crate::pnl::Tone::Down => t.down,
                crate::pnl::Tone::Flat => t.text_main,
            });
            let row = div()
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
                        .text_color(if r.open { t.text_main } else { t.text_dim })
                        .child(text.as_str().to_owned()),
                )
                .children(tone.map(|c| {
                    div()
                        .flex_shrink_0()
                        .text_color(c)
                        .child(pnl.as_str().to_owned())
                }));
            if r.open {
                row.child(
                    div()
                        .id(("group-cancel", id))
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_shrink_0()
                        .w(px(22.0))
                        .h(px(22.0))
                        .border_1()
                        .border_color(t.axis_line)
                        .text_color(t.text_main)
                        .cursor_pointer()
                        .child("×")
                        .on_click(cx.listener(move |v, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            v.cancel_order(id, cx);
                            cx.notify();
                        })),
                )
            } else {
                row.child(div().flex_shrink_0().text_color(t.text_dim).child("done"))
            }
        })
        .collect::<Vec<_>>();
    let n = menu.open_count();
    let armed = menu.confirm == Some(n);
    let plot = v.plot_bounds();
    let avail = f32::from(window.viewport_size().height) - f32::from(plot.origin.y);
    let (list_h, top) = list_place(menu.rows.len(), menu.top, avail);
    let width = WIDTH
        .min(f32::from(plot.size.width) - 2.0 * MARGIN)
        .max(120.0);
    // The list's left edge from the pane's, keeping it inside the plot (never over the axis).
    let left = (f32::from(plot.size.width) - width - MARGIN).max(MARGIN);
    let scroll = if menu.rows.len() as f32 * ROW_H > list_h {
        " · scroll for all"
    } else {
        ""
    };
    let cancel = div()
        .id("group-cancel-all")
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .h(px(ROW_H + 2.0))
        .border_1()
        .border_color(t.warn)
        .font_weight(FontWeight::BOLD)
        .cursor_pointer()
        .child(cancel_all_label(n, armed))
        // A double-click's second press is not a confirmation.
        .on_click(cx.listener(|v, ev: &ClickEvent, _, cx| {
            cx.stop_propagation();
            if ev.click_count() > 1 {
                return;
            }
            let ids = v.ct.group_menu.as_mut().and_then(|m| m.cancel_all());
            for id in ids.into_iter().flatten() {
                v.cancel_order(id.0, cx);
            }
            cx.notify();
        }));
    let cancel = if armed {
        cancel.bg(t.warn).text_color(t.on_warn)
    } else {
        cancel.text_color(t.text_main)
    };
    let list = div()
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(width))
        .flex()
        .flex_col()
        .gap_2()
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
                .flex_shrink_0()
                .whitespace_nowrap()
                .overflow_hidden()
                .font_weight(FontWeight::BOLD)
                .text_color(t.text_main)
                .child(format!("{}{scroll}", menu.title)),
        )
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
        assert_eq!(cancel_all_label(20, false), "Cancel 20 orders");
        assert_eq!(
            cancel_all_label(20, true),
            "Press again to cancel 20 orders"
        );
        assert_eq!(cancel_all_label(1, true), "Press again to cancel 1 order");
    }

    /// Cancel all and at least one row stay in the window, on any pane.
    #[test]
    fn the_list_stays_in_the_window() {
        // Plenty of room: under the tag, twelve rows.
        let (h, top) = list_place(70, 100.0, 800.0);
        assert_eq!(h, ROW_H * ROWS_SHOWN as f32);
        assert_eq!(top, 100.0 + lines_core::geometry::order_tags::TAG_H + 6.0);
        // A tag near the window's bottom: moved up, all of it inside.
        let (h, top) = list_place(70, 700.0, 800.0);
        assert!(top + h + CHROME_H <= 800.0, "{top} {h}");
        // A short chart at the window's bottom: fewer rows, Cancel all still inside.
        let (h, top) = list_place(70, 20.0, 150.0);
        assert!(h < ROW_H * ROWS_SHOWN as f32);
        assert!(top + h + CHROME_H <= 150.0, "{top} {h}");
        // Fewer rows than the limit: just those.
        assert_eq!(list_place(3, 0.0, 800.0).0, ROW_H * 3.0);
        // Hopelessly short: one row, from the top.
        assert_eq!(list_place(70, 20.0, 60.0), (ROW_H, MARGIN));
    }
}
