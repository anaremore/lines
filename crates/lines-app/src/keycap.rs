//! A command's shortcut drawn as keycaps: one small cap per key (`Ctrl` `Shift` `F`, or
//! `⇧` `⌘` `F` on macOS) beside an action's name in tooltips, menus, the trade panel's lists, the
//! palette and Settings › Keys. The keys come from `commands::keys` (`commands::BINDINGS`), the
//! same source as the text form `commands::hint`, which stays for sentences and accessible
//! names. Caps are sized in UI-zoom units and colored from theme tokens: `text_dim` on `bg`,
//! edged in `control_edge` (AA in every theme; `theme::tests`).

use gpui::{Div, IntoElement, ParentElement, Styled, div};

use crate::commands::{self, Command};
use crate::theme::Theme;

/// The caps for `cmd`'s key, or `None` when it has none.
pub fn caps(cmd: Command, t: &Theme) -> Option<Div> {
    caps_of(&commands::keys(cmd), t)
}

/// Caps for `keys` (one per key), or `None` when there are none.
pub fn caps_of(keys: &[String], t: &Theme) -> Option<Div> {
    if keys.is_empty() {
        return None;
    }
    let mut row = div()
        .flex()
        .flex_row()
        .flex_none()
        .items_center()
        .gap(crate::zoom::ui(2.0));
    for k in keys {
        row = row.child(cap(k, t));
    }
    Some(row)
}

/// One cap: the key's text in a small bordered box, at least as wide as it is tall.
fn cap(key: &str, t: &Theme) -> impl IntoElement {
    div()
        .flex_none()
        .h(crate::zoom::ui(CAP_H_100))
        .min_w(crate::zoom::ui(CAP_H_100))
        .px(crate::zoom::ui(3.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .border_1()
        .border_color(t.control_edge)
        .bg(t.bg)
        .text_size(crate::zoom::ui(CAP_TEXT_100))
        .line_height(crate::zoom::ui(CAP_TEXT_100 + 2.0))
        .text_color(t.text_dim)
        .whitespace_nowrap()
        .child(key.to_owned())
}

/// A cap's height at 100 %.
pub const CAP_H_100: f32 = 16.0;
/// A cap's text size at 100 % (the type scale's small size, docs/design.md §5).
pub const CAP_TEXT_100: f32 = 11.0;
