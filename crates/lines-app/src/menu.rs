//! Context menus (pane, tab, detached pane): rows of commands with their hotkey hints
//! (decision 0018: a menu entry is just another way to issue a command). Keyboard: ↑↓ skip
//! separators, Enter runs, Esc closes. Shared by the main and pane windows.

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement, Pixels, Point, Size, Styled, Window, div, prelude::*, px,
};

use crate::commands::{self, Command};
use crate::theme::Theme;

const MENU_W: f32 = 320.0;
const ROW_H: f32 = 22.0;
const SEP_H: f32 = 9.0;

pub struct MenuItem {
    pub label: String,
    pub hint: String,
    /// `None`: a separator.
    pub cmd: Option<Command>,
    pub checked: bool,
}

impl MenuItem {
    /// A row for `cmd` with its usual label and hint.
    pub fn of(cmd: Command, checked: bool) -> MenuItem {
        MenuItem::named(&commands::label(cmd), cmd, checked)
    }

    pub fn named(label: &str, cmd: Command, checked: bool) -> MenuItem {
        MenuItem {
            label: label.to_owned(),
            hint: commands::hint(cmd),
            cmd: Some(cmd),
            checked,
        }
    }

    pub fn sep() -> MenuItem {
        MenuItem {
            label: String::new(),
            hint: String::new(),
            cmd: None,
            checked: false,
        }
    }
}

pub struct Menu {
    /// Top-left, window coordinates.
    pub at: Point<Pixels>,
    pub items: Vec<MenuItem>,
    pub selected: usize,
}

/// What a key did in an open menu.
#[derive(Debug, PartialEq)]
pub enum MenuKey {
    Close,
    Run(Command),
    Moved,
    Ignored,
}

impl Menu {
    pub fn new(at: Point<Pixels>, items: Vec<MenuItem>) -> Menu {
        let selected = items.iter().position(|i| i.cmd.is_some()).unwrap_or(0);
        Menu {
            at,
            items,
            selected,
        }
    }

    fn step(&self, d: isize) -> usize {
        let n = self.items.len() as isize;
        let mut i = self.selected as isize;
        for _ in 0..n {
            i = (i + d).rem_euclid(n);
            if self.items[i as usize].cmd.is_some() {
                break;
            }
        }
        i as usize
    }

    pub fn key(&mut self, key: &str) -> MenuKey {
        match key {
            "escape" => MenuKey::Close,
            "down" => {
                self.selected = self.step(1);
                MenuKey::Moved
            }
            "up" => {
                self.selected = self.step(-1);
                MenuKey::Moved
            }
            "enter" => self
                .items
                .get(self.selected)
                .and_then(|i| i.cmd)
                .map_or(MenuKey::Close, MenuKey::Run),
            _ => MenuKey::Ignored,
        }
    }

    /// The menu over the whole window (a click outside closes it). `slot` reaches the view's
    /// menu; `run` issues a command.
    pub fn render<V: 'static>(
        &self,
        theme: &Theme,
        viewport: Size<Pixels>,
        slot: fn(&mut V) -> &mut Option<Menu>,
        run: fn(&mut V, Command, &mut Window, &mut Context<V>),
        cx: &mut Context<V>,
    ) -> AnyElement {
        let t = *theme;
        let (vw, vh) = (f32::from(viewport.width), f32::from(viewport.height));
        let h = self
            .items
            .iter()
            .map(|i| if i.cmd.is_some() { ROW_H } else { SEP_H })
            .sum::<f32>()
            + 8.0;
        let h = h.min(vh - 8.0).max(ROW_H);
        let x = f32::from(self.at.x).min(vw - MENU_W - 4.0).max(0.0);
        let y = f32::from(self.at.y).min(vh - h - 4.0).max(0.0);
        let mut list = div()
            .id("menu")
            .occlude()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(MENU_W))
            .max_h(px(h))
            .overflow_y_scroll()
            .py_1()
            .flex()
            .flex_col()
            .bg(t.panel)
            .border_1()
            .border_color(t.axis_line)
            .rounded_md()
            .text_size(px(12.0));
        for (i, item) in self.items.iter().enumerate() {
            let Some(cmd) = item.cmd else {
                list = list.child(div().my_1().h(px(1.0)).flex_none().bg(t.axis_line));
                continue;
            };
            let selected = i == self.selected;
            list = list.child(
                div()
                    .id(("menu-item", i))
                    .h(px(ROW_H))
                    .flex_none()
                    .px_2()
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
                            .w(px(12.0))
                            .text_color(t.text_main)
                            .child(if item.checked { "✓" } else { "" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_color(t.text_main)
                            .child(item.label.clone()),
                    )
                    .child(div().text_color(t.text_dim).child(item.hint.clone()))
                    .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                        if let Some(m) = slot(this).as_mut()
                            && m.selected != i
                        {
                            m.selected = i;
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            *slot(this) = None;
                            run(this, cmd, window, cx);
                            cx.notify();
                        }),
                    ),
            );
        }
        // The menu deferred above any open header dropdown (`dropdown::OVERLAY_PRIORITY`); its
        // backdrop at every popover's (under Flatten and Cancel all, `crate::popover`).
        let rect = crate::layout::Rect::new(x, y, MENU_W, h);
        div()
            .absolute()
            .size_full()
            .child(
                gpui::deferred(
                    // A press anywhere else closes the menu and does nothing else
                    // (`crate::popover`): it occludes, and stops the press.
                    div()
                        .id("menu-backdrop")
                        .absolute()
                        .size_full()
                        .occlude()
                        .on_any_mouse_down(cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            let p = (f32::from(ev.position.x), f32::from(ev.position.y));
                            if let Some(d) = crate::popover::outside_click(true, p, &[rect]) {
                                if d.consume {
                                    cx.stop_propagation();
                                }
                                crate::popover::mark_dismissing();
                                *slot(this) = None;
                                cx.notify();
                            }
                        })),
                )
                .with_priority(crate::dropdown::BACKDROP_PRIORITY),
            )
            .child(gpui::deferred(list).with_priority(crate::dropdown::OVERLAY_PRIORITY))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;

    #[test]
    fn keys_skip_separators_and_run_the_selection() {
        let mut m = Menu::new(
            point(px(0.0), px(0.0)),
            vec![
                MenuItem::sep(),
                MenuItem::of(Command::ClosePane, false),
                MenuItem::sep(),
                MenuItem::of(Command::NewTab, false),
            ],
        );
        assert_eq!(m.selected, 1, "starts on the first command");
        assert_eq!(m.key("down"), MenuKey::Moved);
        assert_eq!(m.selected, 3);
        assert_eq!(m.key("down"), MenuKey::Moved);
        assert_eq!(m.selected, 1, "wraps, skipping separators");
        assert_eq!(m.key("up"), MenuKey::Moved);
        assert_eq!(m.key("enter"), MenuKey::Run(Command::NewTab));
        assert_eq!(m.key("escape"), MenuKey::Close);
        assert_eq!(m.key("x"), MenuKey::Ignored);
        assert_eq!(
            MenuItem::of(Command::ReopenTab, false).label,
            "Reopen closed tab"
        );
    }
}
