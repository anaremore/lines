//! A dropdown drawn (`crate::drop_list` is its model): the trigger, a field showing the value
//! in force and a ▾, and the open list in the menus' look (`crate::menu`): small-capital group
//! headers, a check for the value in force (a box per row for a multi-select), the lit row with
//! the accent's left edge, a last row "Custom…" that becomes a field. Placed by
//! `crate::dropdown::dropdown_list`: floating under its trigger (above it when there's more
//! room there), never moving or resizing anything, at most `drop_list::MAX_H` tall with the lit
//! row scrolled into view.

use gpui::prelude::*;
use gpui::{Context, MouseButton, ScrollHandle, Window, div, px};

use crate::drop_list::{DropList, MAX_H};
use crate::theme::Theme;

/// The trigger: `text` (the value in force, or a multi-select's summary) and ▾, outlined like a
/// number field; `focused`: the keys' row (a main-text edge), `open`: its list is open (accent).
pub fn trigger(
    id: (&'static str, usize),
    text: String,
    focused: bool,
    open: bool,
    locked: bool,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .whitespace_nowrap()
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(if open {
            t.accent
        } else if focused {
            t.text_main
        } else {
            t.control_edge
        })
        .when(locked, |d| d.border_dashed())
        .when(!locked, |d| d.cursor_pointer())
        .text_color(t.text_main)
        .relative()
        .children(
            crate::layout_rec::on()
                .then(|| {
                    crate::layout_rec::bordered(
                        lines_core::layout_lint::Kind::Chip,
                        text.clone(),
                        0,
                        "",
                        1.0,
                    )
                })
                .flatten(),
        )
        .child(text)
        .child(div().text_color(t.text_dim).child("▾"))
}

/// The open list. `checked(k)`: row `k` is the value in force (one pick) or on (several);
/// `on_row(v, k, ..)`: row `k` clicked (a row index of `l`, "Custom…" included).
pub fn list<V: 'static>(
    id: &'static str,
    l: &DropList,
    checked: &dyn Fn(usize) -> bool,
    t: &Theme,
    cx: &mut Context<V>,
    on_row: impl Fn(&mut V, usize, &mut Window, &mut Context<V>) + Clone + 'static,
) -> gpui::Stateful<gpui::Div> {
    let mut el = div()
        .id(id)
        // The wheel scrolls the list itself (`dropdown_list`); every other press stops here.
        .block_mouse_except_scroll()
        .min_w(crate::zoom::ui(120.0))
        .flex()
        .flex_col()
        .py_1()
        .bg(t.panel)
        .border_1()
        .border_color(t.axis_line)
        .rounded_md()
        .whitespace_nowrap()
        .text_size(crate::zoom::ui(12.0));
    let (accent, lit_bg) = (t.accent, t.ghost_selected);
    let mut prev: Option<&'static str> = None;
    let row_h = crate::zoom::ui(22.0);
    for k in 0..l.len() {
        let custom = l.custom && k == l.entries.len();
        if let Some(e) = l.entries.get(k)
            && e.group.is_some()
            && e.group != prev
        {
            if prev.is_some() {
                el = el.child(sep(t));
            }
            prev = e.group;
            el = el.child(
                div()
                    .flex_none()
                    .px_3()
                    .pt_1()
                    .text_size(crate::zoom::ui(10.5))
                    .text_color(t.text_dim)
                    .child(e.group.unwrap_or_default().to_uppercase()),
            );
        }
        if custom {
            el = el.child(sep(t));
        }
        let lit = k == l.lit;
        let label = match (custom, &l.typing) {
            (true, Some(text)) => format!("Custom: {text}▏"),
            (true, None) => "Custom…".to_owned(),
            (false, _) => l.entries[k].label.clone(),
        };
        let mark = if l.multi && !custom {
            let on = checked(k);
            div()
                .size(crate::zoom::ui(11.0))
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .border_color(t.control_edge)
                .rounded_sm()
                .text_size(crate::zoom::ui(10.0))
                .text_color(t.text_main)
                .child(if on { "✓" } else { "" })
        } else if !custom && checked(k) {
            div().text_color(t.text_main).child("✓")
        } else {
            div()
        };
        let on_row = on_row.clone();
        el = el.child(
            div()
                .id(("drop-row", k))
                .h(row_h)
                .flex_none()
                .pl_2()
                .pr_3()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .border_l_2()
                .border_color(if lit {
                    accent
                } else {
                    gpui::transparent_black()
                })
                .when(lit, |d| d.bg(lit_bg))
                .cursor_pointer()
                .hover(move |s| s.bg(lit_bg).border_color(accent))
                .text_color(t.text_main)
                .relative()
                .children(row_marker(&label))
                .child(div().w(crate::zoom::ui(12.0)).flex_none().child(mark))
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v: &mut V, _: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        on_row(v, k, window, cx);
                        cx.notify();
                    }),
                ),
        );
    }
    el
}

/// A row's record for the layout lint, on the popups' layer (it covers the rows under the list).
/// A row the list's own scroll cuts is how a scrolling list looks: not recorded.
fn row_marker(label: &str) -> Option<gpui::AnyElement> {
    if !crate::layout_rec::on() {
        return None;
    }
    let label = label.to_owned();
    Some(
        gpui::canvas(
            move |b, window, _| {
                let shown = b.intersect(&window.content_mask().bounds);
                if shown.size != b.size && shown.size.height > px(0.0) {
                    return;
                }
                crate::layout_rec::record(
                    window,
                    b,
                    crate::layout_rec::Spec::text(&label).layer(crate::layout_rec::POPUP),
                );
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .into_any_element(),
    )
}

fn sep(t: &Theme) -> gpui::Div {
    div().my_1().h(px(1.0)).flex_none().bg(t.axis_line)
}

/// The cap `dropdown_list` takes, at the UI zoom.
pub fn cap() -> f32 {
    f32::from(crate::zoom::ui(MAX_H))
}

/// Scroll row `k` of `l` into view (on open: a few rows above it show too, so it sits near the
/// middle of a long list).
pub fn reveal(scroll: &ScrollHandle, l: &DropList, opening: bool) {
    let child = l.child_index(l.lit);
    if opening {
        scroll.scroll_to_top_of_item(child.saturating_sub(4));
    } else {
        scroll.scroll_to_item(child);
    }
}
