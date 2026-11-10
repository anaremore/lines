//! A small hover tooltip (trade panel v2; reused by the pane-header icons): the action's name
//! and its key as caps (`keycap`), if it has one, after [`DELAY`] on hover. Keys live here,
//! never in button labels. Colors are theme tokens (`panel` with `text_main`, AA in
//! every theme; `theme::tests`).
//!
//! Also gives a control its accessible role and name (AccessKit), so it is never anonymous to
//! assistive technology.

use std::time::Duration;

use gpui::{
    AnyView, App, AppContext, Context, IntoElement, ParentElement, Render, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Window, div,
};

use crate::commands::Command;
use crate::theme::Theme;

/// Hover time before a tooltip shows.
pub const DELAY: Duration = Duration::from_millis(400);

pub struct Tooltip {
    name: SharedString,
    cmd: Option<Command>,
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
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(self.name.clone())
            .children(self.cmd.and_then(|c| crate::keycap::caps(c, &t)))
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
    let tip: SharedString = name.to_owned().into();
    let theme = *t;
    el.role(gpui::Role::Button)
        .aria_label(SharedString::from(name.to_owned()))
        .tooltip(move |_, cx: &mut App| -> AnyView {
            cx.new(|_| Tooltip {
                name: tip.clone(),
                cmd,
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
    use crate::commands;
    use crate::commands::TradeCmd;

    #[test]
    fn a_tooltip_shows_its_actions_key_as_caps() {
        // Flatten's key, one cap per key, ending in its letter.
        let k = commands::keys(Command::Trade(TradeCmd::Flatten));
        assert!(k.len() >= 2 && k.last().is_some_and(|l| l == "F"), "{k:?}");
        // An action without a key gets no caps.
        assert!(commands::keys(Command::Strategy(commands::StrategyCmd::Start)).is_empty());
    }
}
