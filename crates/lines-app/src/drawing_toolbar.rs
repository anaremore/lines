//! The chart's drawing toolbar (owner-approved mock, 2026-11-10): the essential drawing tools,
//! each named as everywhere else (`Tool::label`; owner, 2026-11-11: one name per tool)
//! in one segmented group, then Delete apart. Off by default; per chart (`ChartState::
//! drawing_tools`), on new charts as Settings › Charts says, docked left (a column) or top (a
//! row, `Settings::drawing_tools_dock`). It runs the drawing commands the keys run, never its
//! own path: a tool's button is `DrawCmd::Tool` (again: back to pointing), Delete is
//! `DrawCmd::DeleteSelected`. Icons are Lucide's (`lines_icons`, drawn with `icons::lucide`); the
//! measure tool's is the ruler (owner).

use gpui::prelude::FluentBuilder;
use gpui::{App, Context, Div, InteractiveElement, MouseButton, ParentElement, Styled, div};
use lines_core::drawings::Tool;
use serde::{Deserialize, Serialize};

use crate::chart_view::ChartView;
use crate::commands::{Command, DrawCmd};
use crate::icons::lucide;
use crate::theme::Theme;

/// Where the toolbar sits: a column at the plot's left (the default) or a row above it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dock {
    #[default]
    Left,
    Top,
}

impl Dock {
    /// Settings › Charts' choice names, in order.
    pub const LABELS: [&'static str; 2] = ["Left", "Top"];
    pub const ALL: [Dock; 2] = [Dock::Left, Dock::Top];

    pub fn is_default(&self) -> bool {
        *self == Dock::Left
    }
}

/// One tool button: its tool, name (the tooltip and accessible name), icon (its asset path,
/// `icons/<lucide name>.svg`), and quarter turns.
#[derive(Copy, Clone, Debug)]
pub struct ToolButton {
    pub tool: Tool,
    pub name: &'static str,
    pub icon: &'static str,
    pub turns: u8,
}

/// Delete's icon.
pub const TRASH: &str = "icons/trash.svg";

/// The tools, in order: line, horizontal, vertical, rectangle, text, measure.
pub const TOOLS: [ToolButton; 6] = [
    ToolButton {
        tool: Tool::Trend,
        name: "Trend line",
        icon: "icons/slash.svg",
        turns: 0,
    },
    ToolButton {
        tool: Tool::HLine,
        name: "Horizontal line",
        icon: "icons/minus.svg",
        turns: 0,
    },
    ToolButton {
        tool: Tool::VLine,
        name: "Vertical line",
        icon: "icons/minus.svg",
        turns: 1,
    },
    ToolButton {
        tool: Tool::Rect,
        name: "Rectangle",
        icon: "icons/square.svg",
        turns: 0,
    },
    ToolButton {
        tool: Tool::Note,
        name: "Text",
        icon: "icons/type.svg",
        turns: 0,
    },
    ToolButton {
        tool: Tool::Measure,
        name: "Measure",
        icon: "icons/ruler.svg",
        turns: 0,
    },
];

/// A button (logical px at 100 %): above the 20 px least target.
pub const BUTTON: f32 = 24.0;
/// The icon in it.
pub const ICON: f32 = 16.0;
/// The strip's depth (its width docked left, its height docked top).
pub const STRIP: f32 = 32.0;
/// The gap before Delete.
pub const GAP: f32 = 8.0;
/// The strip's padding along its length.
pub const PAD: f32 = 6.0;

/// The row's length docked top (logical px at 100 %): the group with its edges, the gap and
/// Delete, and the padding at both ends.
pub fn row_len() -> f32 {
    let n = TOOLS.len() as f32;
    // Buttons, the 1 px separators between them and the group's 1 px edge each side.
    let group = n * BUTTON + (n - 1.0) + 2.0;
    PAD + group + GAP + BUTTON + PAD
}

/// Where the toolbar goes on a chart `width` logical px wide (at 100 %): as chosen, except
/// that a row that doesn't fit becomes the column (never wrapped or cut).
pub fn dock_for(chosen: Dock, width: f32) -> Dock {
    match chosen {
        Dock::Top if width < row_len() => Dock::Left,
        d => d,
    }
}

/// The narrowest pane (logical px at 100 %) the toolbar shows on: its column, the price axis
/// (`chart_view::AXIS_W_100`, 78) and about 70 px of plot. Narrower, it steps aside rather
/// than squeeze the axis and cut the last price (the keys and the menus still reach every tool).
pub const MIN_PANE: f32 = 180.0;

/// Where the toolbar goes from where it is now (`was`; `None`: not shown), on a pane `width`
/// logical px wide (at 100 %): [`dock_for`], or `None` on a pane under [`MIN_PANE`]. Each
/// change needs a pixel to spare (the column back to the row, the toolbar back at all), so a
/// width that pixel rounding moves by a fraction never flips it back and forth (each flip
/// renders again).
pub fn dock_from(chosen: Dock, width: f32, was: Option<Dock>) -> Option<Dock> {
    let back = if was.is_none() { 1.0 } else { 0.0 };
    if width < MIN_PANE + back {
        return None;
    }
    let slack = if was == Some(Dock::Left) { 1.0 } else { 0.0 };
    Some(dock_for(chosen, width - slack))
}

/// A chart pane's width (window logical px) from its chart's (the body beside the toolbar)
/// with the toolbar placed at `dock` (`None`: not shown) at UI zoom `zoom`: the column docked
/// left takes its width from the pane, a row on top none. The header and the workspace's pane
/// controls size by the pane's width, so they agree.
pub fn pane_width(chart: f32, dock: Option<Dock>, zoom: f32) -> f32 {
    if chart > 0.0 && dock == Some(Dock::Left) {
        chart + STRIP * zoom
    } else {
        chart
    }
}

/// The toolbar for a chart: `active` the tool in use (`None`: pointing), `selected` whether a
/// drawing is selected (Delete is live), `dock` where it sits (after [`dock_for`]).
pub fn render(
    dock: Dock,
    active: Option<Tool>,
    selected: bool,
    t: &Theme,
    cx: &mut Context<ChartView>,
) -> Div {
    let z = crate::zoom::ui;
    let left = dock == Dock::Left;
    let mut group = div()
        .flex()
        .flex_none()
        .border_1()
        .border_color(t.axis_line)
        .rounded(z(4.0))
        .overflow_hidden()
        .bg(t.panel);
    group = if left {
        group.flex_col()
    } else {
        group.flex_row()
    };
    for (i, b) in TOOLS.iter().enumerate() {
        let on = active == Some(b.tool);
        let (fg, bg) = if on {
            (t.bg, Some(t.accent))
        } else {
            (t.text_dim, None)
        };
        let cmd = Command::Draw(DrawCmd::Tool(b.tool));
        let tool = b.tool;
        let mut el = div()
            .id(("draw-tool", i))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .w(z(BUTTON))
            .h(z(BUTTON))
            .cursor_pointer()
            .group("draw-tool")
            .when_some(bg, |d, c| d.bg(c))
            .child({
                let icon = lucide(b.icon, z(ICON), fg, b.turns);
                // Under the pointer the icon turns `text_main` (its own color, not inherited).
                let main = t.text_main;
                if on {
                    icon
                } else {
                    icon.group_hover("draw-tool", move |s| s.text_color(main))
                }
            });
        if !on {
            let hover_bg = t.ghost_hover;
            el = el.hover(move |s| s.bg(hover_bg));
        }
        if i > 0 {
            el = if left {
                el.border_t_1()
            } else {
                el.border_l_1()
            }
            .border_color(t.axis_line);
        }
        let el = el.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |v: &mut ChartView, _, window, cx| {
                cx.stop_propagation();
                v.toolbar_command(DrawCmd::Tool(tool), window, cx);
            }),
        );
        let el = crate::tooltip::with_tip(el, b.name, Some(cmd), t);
        group = group.child(el);
    }
    // Delete: last, apart, destructive; dimmed (a dashed edge, no fill) with nothing selected.
    let (fg, edge) = if selected {
        (t.destructive, t.destructive)
    } else {
        (t.text_dim, t.axis_line)
    };
    let del = div()
        .id("draw-delete")
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(z(BUTTON))
        .h(z(BUTTON))
        .rounded(z(4.0))
        .border_color(edge)
        .map(|d| {
            if selected {
                d.border_1().cursor_pointer()
            } else {
                d.border_2().border_dashed()
            }
        })
        .child(lucide(TRASH, z(ICON), fg, 0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |v: &mut ChartView, _, window, cx| {
                cx.stop_propagation();
                if selected {
                    v.toolbar_command(DrawCmd::DeleteSelected, window, cx);
                }
            }),
        );
    let del = crate::tooltip::tip_keys(del, "Delete", vec!["Del".to_owned()], t);
    let strip = div()
        .flex()
        .flex_none()
        .items_center()
        .bg(t.panel)
        .border_color(t.axis_line);
    let strip = if left {
        strip.flex_col().w(z(STRIP)).py(z(PAD)).border_r_1()
    } else {
        strip.flex_row().h(z(STRIP)).px(z(PAD)).border_b_1()
    };
    let gap = if left {
        div().flex_none().h(z(GAP))
    } else {
        div().flex_none().w(z(GAP))
    };
    strip.child(group).child(gap).child(del)
}

/// The toolbar's place on a pane `width` wide (window logical px; `None` before the first
/// layout: the choice stands) with the app's choice, from where it is now (`was`); `None`:
/// the pane is too narrow for it ([`MIN_PANE`]).
pub fn dock_now(width: Option<f32>, was: Option<Dock>, cx: &App) -> Option<Dock> {
    let chosen = crate::picker_view::settings(cx).drawing_tools_dock;
    width.map_or(Some(chosen), |w| {
        dock_from(chosen, w / crate::zoom::f(1.0), was)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row docked top needs 195 px at 100 % (the mock's 196 counted Delete's edge twice);
    /// a narrower chart gets the column instead (never a wrapped or cut row); left stays left.
    #[test]
    fn a_row_that_does_not_fit_becomes_the_column() {
        assert_eq!(row_len(), 195.0);
        assert_eq!(dock_for(Dock::Top, 195.0), Dock::Top);
        assert_eq!(dock_for(Dock::Top, 194.0), Dock::Left);
        assert_eq!(dock_for(Dock::Left, 1000.0), Dock::Left);
        assert_eq!(dock_for(Dock::Left, 100.0), Dock::Left);
        // Back from the column only with a pixel to spare (no flip-flop on rounding).
        assert_eq!(
            dock_from(Dock::Top, 195.5, Some(Dock::Left)),
            Some(Dock::Left)
        );
        assert_eq!(
            dock_from(Dock::Top, 196.0, Some(Dock::Left)),
            Some(Dock::Top)
        );
        assert_eq!(
            dock_from(Dock::Top, 195.0, Some(Dock::Top)),
            Some(Dock::Top)
        );
        assert_eq!(dock_from(Dock::Top, 195.0, None), Some(Dock::Top));
        // Under 180 px the toolbar steps aside (never squeezing the price axis); back with a
        // pixel to spare.
        assert_eq!(dock_from(Dock::Left, 179.0, Some(Dock::Left)), None);
        assert_eq!(
            dock_from(Dock::Top, 185.0, Some(Dock::Top)),
            Some(Dock::Left)
        );
        assert_eq!(dock_from(Dock::Left, 180.5, None), None);
        assert_eq!(dock_from(Dock::Left, 181.0, None), Some(Dock::Left));
        const { assert!(MIN_PANE >= STRIP + crate::chart_view::AXIS_W_100 + 64.0) };
    }

    /// The header sizes by the pane's width, not the chart's: a 380 px pane with the column
    /// docked left has a 348 px chart, and the header must still see 380 (as the workspace's
    /// pane controls do), at any zoom; no width yet stays none.
    #[test]
    fn the_pane_is_the_chart_plus_the_column() {
        assert_eq!(pane_width(348.0, Some(Dock::Left), 1.0), 380.0);
        assert_eq!(pane_width(332.0, Some(Dock::Left), 1.5), 380.0);
        assert_eq!(pane_width(380.0, Some(Dock::Top), 1.0), 380.0);
        assert_eq!(pane_width(380.0, None, 1.0), 380.0);
        assert_eq!(pane_width(0.0, Some(Dock::Left), 1.0), 0.0);
    }

    /// One name per tool (owner, 2026-11-11): the toolbar's, the style editor's and
    /// `Tool::label` (the banner, the palette, menus) are the same words.
    #[test]
    fn every_tool_has_one_name() {
        let cap = |s: &str| {
            let mut c = s.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        };
        for b in TOOLS {
            assert_eq!(b.name, cap(b.tool.label()), "{:?}", b.tool);
        }
        for t in Tool::ALL {
            assert_eq!(
                crate::style_editor::tool_names(t).0,
                cap(t.label()),
                "{t:?}"
            );
        }
    }

    /// The tools run the keys' commands, in the mock's order, with the owner's ruler for the
    /// measure; every button is above the 20 px least target.
    #[test]
    fn the_tools_are_the_keys_tools() {
        let tools: Vec<_> = TOOLS.iter().map(|b| b.tool).collect();
        assert_eq!(
            tools,
            [
                Tool::Trend,
                Tool::HLine,
                Tool::VLine,
                Tool::Rect,
                Tool::Note,
                Tool::Measure
            ]
        );
        let measure = TOOLS.iter().find(|b| b.tool == Tool::Measure).unwrap();
        assert_eq!(measure.icon, "icons/ruler.svg");
        // Every icon is served (`icons::Assets`): a path is a string, so a typo or an icon
        // dropped from `lines_icons::ICONS` would otherwise draw a blank button.
        use gpui::AssetSource;
        for path in TOOLS.iter().map(|b| b.icon).chain([TRASH]) {
            assert!(crate::icons::Assets.load(path).unwrap().is_some(), "{path}");
        }
        const { assert!(BUTTON >= 20.0) };
        // Each tool's key is the one its tooltip shows (Alt+T, H, L, Z, N, U).
        for (b, key) in TOOLS.iter().zip(["T", "H", "L", "Z", "N", "U"]) {
            let keys = crate::commands::keys(Command::Draw(DrawCmd::Tool(b.tool)));
            assert_eq!(keys.len(), 2, "{}: {keys:?}", b.name);
            assert_eq!(keys[1], key, "{}", b.name);
        }
    }
}
