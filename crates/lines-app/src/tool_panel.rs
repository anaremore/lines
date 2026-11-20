//! A plugin drawing tool's strip and point card on a chart (`lines_feed::levels::DrawTool`).
//!
//! - **Strip** ([`DrawTool::panel`]): while the tool is chosen, or one of its drawings is
//!   selected, its status text and buttons over the plot's top-right corner. A button calls
//!   [`DrawTool::act`]; what it asks back ([`ToolEffect`]) the chart does: draw the tool's
//!   drawings again, or show another stretch of time.
//! - **Point card** ([`DrawTool::describe`]): for a selected drawing of a tool that names its
//!   points ([`DrawTool::point_names`]), beside the point last pressed (the second after a
//!   placement): the tool's lines about it, a button per point name (the point's own is on;
//!   the other swaps the drawing's two points, one undo step) and Delete.
//!
//! The tool supplies only text; Lines draws both (decision 0021: a plugin never draws its own
//! UI). Both are rendered, not painted, and only when a chart shows one.

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement,
    SharedString, Styled, div, prelude::FluentBuilder, px,
};
use lines_feed::levels::{DrawTool, ToolPanel};

use crate::chart_view::ChartView;
use crate::theme::Theme;

/// The point card's width (logical px, before UI zoom).
pub const CARD_W: f32 = 240.0;

/// What a selected plugin drawing's card shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub lines: Vec<String>,
    pub names: [&'static str; 2],
    /// The point described (0: `a`, 1: `b`).
    pub point: usize,
}

/// The card for point `point` of the drawing `a`–`b` of `tool`, if the tool names its points.
pub fn card(
    tool: &dyn DrawTool,
    ctx: &lines_feed::levels::DrawToolContext,
    (a, b): (lines_core::drawings::Anchor, lines_core::drawings::Anchor),
    point: usize,
    prices: &dyn lines_feed::levels::DrawPrices,
) -> Option<Card> {
    let names = tool.point_names()?;
    let point = point.min(1);
    Some(Card {
        lines: tool.describe(ctx, a, b, point, prices),
        names,
        point,
    })
}

/// Where the card goes (body-local, logical px): beside the point at (`x`, `y`), turned left
/// or up where it would leave the body (`w`, `h`: the body's size; `cw`, `ch`: the card's),
/// never above `top_min` (below the strip) nor off the body.
pub fn card_origin(
    x: f32,
    y: f32,
    (w, h): (f32, f32),
    (cw, ch): (f32, f32),
    top_min: f32,
) -> (f32, f32) {
    const GAP: f32 = 12.0;
    let left = if x + GAP + cw <= w {
        x + GAP
    } else {
        x - GAP - cw
    };
    let top = if y + GAP + ch <= h {
        y + GAP
    } else {
        y - GAP - ch
    };
    (
        left.clamp(0.0, (w - cw).max(0.0)),
        top.clamp(top_min, (h - ch).max(top_min)),
    )
}

/// A strip or card button.
fn button(
    id: (&'static str, usize),
    label: SharedString,
    on: bool,
    enabled: bool,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let z = crate::zoom::ui;
    let (fg, bg, edge) = match (on, enabled) {
        (_, false) => (t.text_dim, t.panel, t.border_disabled),
        (true, true) => (t.text_main, t.element_selected, t.accent),
        (false, true) => (t.text_main, t.panel, t.control_edge),
    };
    let hover = t.ghost_hover;
    div()
        .id(id)
        .flex_none()
        .px(z(8.0))
        .h(z(20.0))
        .flex()
        .items_center()
        .rounded(z(3.0))
        .border_1()
        .border_color(edge)
        .bg(bg)
        .text_color(fg)
        .whitespace_nowrap()
        .child(label)
        .when(enabled, |d| d.cursor_pointer().hover(move |s| s.bg(hover)))
}

/// The strip: `panel`'s status and buttons, right-aligned `right` px from the body's right edge.
pub fn strip(
    tool: &'static str,
    panel: &ToolPanel,
    right: f32,
    t: &Theme,
    cx: &mut Context<ChartView>,
) -> AnyElement {
    let z = crate::zoom::ui;
    let mut row = div()
        .absolute()
        .top(z(6.0))
        .right(px(right + 6.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(z(6.0))
        .px(z(6.0))
        .py(z(4.0))
        .rounded(z(4.0))
        .border_1()
        .border_color(t.axis_line)
        .bg(t.panel)
        .text_size(z(12.0))
        .text_color(t.text_main)
        // A press on the strip is never the plot's.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
    if !panel.status.is_empty() {
        row = row.child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .text_color(t.text_dim)
                .child(SharedString::from(panel.status.clone())),
        );
    }
    for (i, a) in panel.actions.iter().enumerate() {
        let label = match a.on {
            Some(true) => format!("{} \u{2713}", a.label),
            _ => a.label.clone(),
        };
        let action = a.id;
        let b = button(
            ("tool-strip", i),
            label.into(),
            a.on == Some(true),
            a.enabled,
            t,
        );
        let b = if a.enabled {
            b.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v: &mut ChartView, _, window, cx| {
                    cx.stop_propagation();
                    v.tool_action(tool, action, window, cx);
                }),
            )
        } else {
            b
        };
        row = row.child(b);
    }
    row.into_any_element()
}

/// The point card at body-local (`left`, `top`).
pub fn card_view(
    c: &Card,
    (left, top): (f32, f32),
    t: &Theme,
    cx: &mut Context<ChartView>,
) -> AnyElement {
    let z = crate::zoom::ui;
    let mut body = div()
        .id("point-card")
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(z(CARD_W))
        .flex()
        .flex_col()
        .gap(z(3.0))
        .p(z(8.0))
        .rounded(z(6.0))
        .border_1()
        .border_color(t.axis_line)
        .bg(t.panel)
        .text_size(z(11.0))
        .text_color(t.text_dim)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
    for (i, l) in c.lines.iter().enumerate() {
        let line = div().child(SharedString::from(l.clone()));
        body = body.child(if i == 0 {
            line.text_size(z(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.text_main)
        } else {
            line
        });
    }
    let mut buttons = div().flex().flex_row().gap(z(6.0)).pt(z(4.0));
    for (i, name) in c.names.iter().enumerate() {
        let on = i == c.point;
        let b = button(("point-name", i), (*name).into(), on, true, t);
        let b = if on {
            b
        } else {
            b.on_mouse_down(
                MouseButton::Left,
                cx.listener(|v: &mut ChartView, _, window, cx| {
                    cx.stop_propagation();
                    v.swap_points(window, cx);
                }),
            )
        };
        buttons = buttons.child(b);
    }
    buttons = buttons.child(
        button(("point-delete", 0), "Delete".into(), false, true, t).on_mouse_down(
            MouseButton::Left,
            cx.listener(|v: &mut ChartView, _, window, cx| {
                cx.stop_propagation();
                v.toolbar_command(crate::commands::DrawCmd::DeleteSelected, window, cx);
            }),
        ),
    );
    body.child(buttons).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_stays_inside_the_body() {
        let body = (800.0, 400.0);
        let card = (240.0, 90.0);
        assert_eq!(card_origin(100.0, 100.0, body, card, 0.0), (112.0, 112.0));
        // Near the right and bottom edges it turns left and up.
        assert_eq!(card_origin(700.0, 380.0, body, card, 0.0), (448.0, 278.0));
        // Never off the top-left.
        assert_eq!(card_origin(5.0, 5.0, (100.0, 50.0), card, 0.0), (0.0, 0.0));
        // A point above the plot: below the strip, inside the body.
        assert_eq!(card_origin(300.0, -80.0, body, card, 36.0), (312.0, 36.0));
        assert_eq!(card_origin(900.0, 100.0, body, card, 36.0), (560.0, 112.0));
    }
}
