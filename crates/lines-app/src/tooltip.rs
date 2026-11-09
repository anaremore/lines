//! A small hover tooltip (trade panel v2; reused by the pane-header icons): the action's name
//! and its key, if it has one ("Flatten · Ctrl+Shift+F"), after [`DELAY`] on hover. Keys live
//! here, never in button labels. Colors are theme tokens (`panel` with `text_main`, AA in
//! every theme; `theme::tests`).
//!
//! Also gives a control its accessible role and name (AccessKit), so it is never anonymous to
//! assistive technology.

use std::time::Duration;

use gpui::{
    AnyView, App, AppContext, Context, IntoElement, ParentElement, Render, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Window, div,
};

use crate::commands::{self, Command};
use crate::theme::Theme;

/// Hover time before a tooltip shows.
pub const DELAY: Duration = Duration::from_millis(400);

pub struct Tooltip {
    text: SharedString,
    theme: Theme,
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        div()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(t.panel)
            .border_1()
            .border_color(t.axis_line)
            .text_size(crate::zoom::ui(12.0))
            .text_color(t.text_main)
            .child(self.text.clone())
    }
}

/// The tooltip text for an action: its name, plus its key when it has one.
pub fn text(name: &str, cmd: Option<Command>) -> String {
    match cmd.map(commands::hint).filter(|h| !h.is_empty()) {
        Some(h) => format!("{name} · {h}"),
        None => name.to_owned(),
    }
}

/// Give a control a tooltip (`name` plus `cmd`'s key, after [`DELAY`]) and an accessible
/// button role with `name` as its label.
pub fn with_tip<E: StatefulInteractiveElement>(
    el: E,
    name: &str,
    cmd: Option<Command>,
    t: &Theme,
) -> E {
    let tip: SharedString = text(name, cmd).into();
    let theme = *t;
    el.role(gpui::Role::Button)
        .aria_label(SharedString::from(name.to_owned()))
        .tooltip(move |_, cx: &mut App| -> AnyView {
            cx.new(|_| Tooltip {
                text: tip.clone(),
                theme,
            })
            .into()
        })
        .tooltip_show_delay(DELAY)
}

/// [`with_tip`] for the common case of a stateful div.
pub fn tip_div(
    el: Stateful<gpui::Div>,
    name: &str,
    cmd: Option<Command>,
    t: &Theme,
) -> Stateful<gpui::Div> {
    with_tip(el, name, cmd, t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::TradeCmd;

    #[test]
    fn names_carry_their_key() {
        let k = text("Flatten", Some(Command::Trade(TradeCmd::Flatten)));
        assert!(k.starts_with("Flatten · ") && k.ends_with('F'), "{k}");
        assert_eq!(text("Edit presets", None), "Edit presets");
    }
}
