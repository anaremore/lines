//! Context menus (pane, tab, detached pane, an object under the pointer): rows of commands with
//! their hotkey hints (decision 0018: a menu entry is just another way to issue a command).
//! The rules (`docs/design.md`, "Menus"): about 12 top-level rows; a set is a submenu one level
//! deep (never deeper) whose row shows its current value; toggles are checkmarks; an object's
//! menu starts with its name as a header; destructive rows go last in `Theme::destructive`.
//!
//! Keyboard: ↑↓ skip separators and headers, → or Enter opens a submenu, ← or Esc closes it,
//! Enter runs, Esc closes. The pointer opens a submenu by hovering its row. A submenu sits
//! beside its row and flips to the left at the window's right edge. Shared by the main and
//! pane windows.

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement, Pixels, Point, Size, Styled, Window, div, prelude::*, px,
};

use crate::commands::{self, Command};
use crate::layout::Rect;
use crate::theme::Theme;

/// Widest a menu or submenu gets (narrower in a narrow window).
pub const MENU_W_100: f32 = 280.0;
/// [`MENU_W_100`] at the UI zoom (`crate::zoom`).
#[inline]
pub fn menu_w() -> f32 {
    crate::zoom::f(MENU_W_100)
}
pub const SUB_W_100: f32 = 260.0;
/// [`SUB_W_100`] at the UI zoom (`crate::zoom`).
#[inline]
pub fn sub_w() -> f32 {
    crate::zoom::f(SUB_W_100)
}
const ROW_H_100: f32 = 22.0;
/// [`ROW_H_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn row_h() -> f32 {
    crate::zoom::f(ROW_H_100)
}
const SEP_H_100: f32 = 9.0;
/// [`SEP_H_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn sep_h() -> f32 {
    // `my_1` (rem, zooms) above and below a 1 px line (doesn't).
    crate::zoom::f(SEP_H_100 - 1.0) + 1.0
}
/// The list's padding above its first row (`py_1`).
const PAD_100: f32 = 4.0;
/// [`PAD_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn pad() -> f32 {
    crate::zoom::f(PAD_100)
}
/// Space kept between a menu and the window's edge.
const EDGE_100: f32 = 4.0;
/// [`EDGE_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn edge() -> f32 {
    crate::zoom::f(EDGE_100)
}

pub struct MenuItem {
    pub label: String,
    /// Right side: the command's shortcut, or a submenu's current value.
    pub hint: String,
    /// `None` with no submenu: a separator, or a header.
    pub cmd: Option<Command>,
    pub checked: bool,
    /// Non-empty: this row opens a submenu (one level: its rows have none).
    pub sub: Vec<MenuItem>,
    /// A non-clickable label: an object's name and value, or a section's name.
    pub header: bool,
    /// Drawn in `Theme::destructive` (delete, close, cancel).
    pub destructive: bool,
    /// An object menu's name and value: a header (dim, never highlighted) in normal case above a
    /// divider (a section's name is small capitals).
    pub title: bool,
    /// An on/off switch: drawn as a box (checked when on), told apart from one of a set (a
    /// check on the one in force) and from an action.
    pub toggle: bool,
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
            ..MenuItem::sep()
        }
        .checked(checked)
    }

    pub fn sep() -> MenuItem {
        MenuItem {
            label: String::new(),
            hint: String::new(),
            cmd: None,
            checked: false,
            sub: Vec::new(),
            header: false,
            destructive: false,
            title: false,
            toggle: false,
        }
    }

    /// A label that can't be clicked: an object's "Stop 2 @ 7562.00", or a section's name.
    pub fn header(label: &str) -> MenuItem {
        MenuItem {
            label: label.to_owned(),
            header: true,
            ..MenuItem::sep()
        }
    }

    /// An object menu's first row: what the menu acts on ("Stop 2 @ 7562.00 · -8.0 t").
    pub fn title(label: &str) -> MenuItem {
        MenuItem {
            title: true,
            ..MenuItem::header(label)
        }
    }

    /// A row that opens `items`, showing `value` (what is set now; may be empty) on its right.
    /// Submenus are one level deep: rows of `items` that are submenus themselves are dropped.
    pub fn submenu(label: &str, value: &str, items: Vec<MenuItem>) -> MenuItem {
        MenuItem {
            label: label.to_owned(),
            hint: value.to_owned(),
            sub: items.into_iter().filter(|i| i.sub.is_empty()).collect(),
            ..MenuItem::sep()
        }
    }

    pub fn checked(mut self, on: bool) -> MenuItem {
        self.checked = on;
        self
    }

    pub fn switch(mut self) -> MenuItem {
        self.toggle = true;
        self
    }

    pub fn destructive(mut self) -> MenuItem {
        self.destructive = true;
        self
    }

    /// A row the pointer and the arrow keys stop on.
    pub fn selectable(&self) -> bool {
        self.cmd.is_some() || !self.sub.is_empty()
    }

    pub fn is_sep(&self) -> bool {
        !self.header && !self.selectable()
    }

    fn height(&self) -> f32 {
        if self.is_sep() { sep_h() } else { row_h() }
    }
}

pub struct Menu {
    /// Top-left, window coordinates.
    pub at: Point<Pixels>,
    pub items: Vec<MenuItem>,
    pub selected: usize,
    /// The submenu shown: its row in `items`.
    pub open: Option<usize>,
    /// The submenu's row the keys act on.
    pub sub_selected: usize,
    /// The keys move in the open submenu (else in the menu).
    pub in_sub: bool,
}

/// What a key did in an open menu.
#[derive(Debug, PartialEq)]
pub enum MenuKey {
    Close,
    Run(Command),
    Moved,
    Ignored,
}

/// The next selectable row from `from`, `d` rows at a time, wrapping (`from` if none).
fn step(items: &[MenuItem], from: usize, d: isize) -> usize {
    let n = items.len() as isize;
    if n == 0 {
        return 0;
    }
    let mut i = from as isize;
    for _ in 0..n {
        i = (i + d).rem_euclid(n);
        if items[i as usize].selectable() {
            return i as usize;
        }
    }
    from
}

fn first(items: &[MenuItem]) -> usize {
    items.iter().position(MenuItem::selectable).unwrap_or(0)
}

impl Menu {
    pub fn new(at: Point<Pixels>, items: Vec<MenuItem>) -> Menu {
        let selected = first(&items);
        Menu {
            at,
            items,
            selected,
            open: None,
            sub_selected: 0,
            in_sub: false,
        }
    }

    /// Nothing highlighted until a key or the pointer picks a row: an object's menu, where the
    /// first row may move or cancel an order, never runs on a stray Enter.
    pub fn unselected(mut self) -> Menu {
        self.selected = usize::MAX;
        self
    }

    /// The open submenu's rows.
    pub fn sub_items(&self) -> &[MenuItem] {
        self.open
            .and_then(|i| self.items.get(i))
            .map_or(&[], |i| &i.sub)
    }

    /// Open row `i`'s submenu (if it has one); `keys`: the keys move into it.
    pub fn open_sub(&mut self, i: usize, keys: bool) -> bool {
        let Some(item) = self.items.get(i).filter(|i| !i.sub.is_empty()) else {
            return false;
        };
        // On what is set now (a checked row), else the first row.
        self.sub_selected = item
            .sub
            .iter()
            .position(|i| i.checked && i.selectable())
            .unwrap_or_else(|| first(&item.sub));
        self.selected = i;
        self.open = Some(i);
        self.in_sub = keys;
        true
    }

    fn close_sub(&mut self) {
        self.open = None;
        self.in_sub = false;
    }

    /// The pointer over row `i`: it is selected, and opens its submenu (another one closes;
    /// one closed by ← opens again as the pointer moves on its row).
    pub fn hover(&mut self, i: usize) -> bool {
        let has_sub = self.items.get(i).is_some_and(|r| !r.sub.is_empty());
        let open_as_it_should = if has_sub {
            self.open == Some(i)
        } else {
            self.open.is_none()
        };
        if self.selected == i && !self.in_sub && open_as_it_should {
            return false;
        }
        if !self.items.get(i).is_some_and(MenuItem::selectable) {
            return false;
        }
        self.selected = i;
        if !self.open_sub(i, false) {
            self.close_sub();
        }
        true
    }

    /// The pointer over the open submenu's row `j`.
    pub fn hover_sub(&mut self, j: usize) -> bool {
        if (self.in_sub && self.sub_selected == j)
            || !self.sub_items().get(j).is_some_and(MenuItem::selectable)
        {
            return false;
        }
        self.in_sub = true;
        self.sub_selected = j;
        true
    }

    pub fn key(&mut self, key: &str) -> MenuKey {
        if self.in_sub {
            let sub = self.sub_items();
            return match key {
                "escape" | "left" => {
                    self.close_sub();
                    MenuKey::Moved
                }
                "down" => {
                    self.sub_selected = step(sub, self.sub_selected, 1);
                    MenuKey::Moved
                }
                "up" => {
                    self.sub_selected = step(sub, self.sub_selected, -1);
                    MenuKey::Moved
                }
                "enter" => sub
                    .get(self.sub_selected)
                    .and_then(|i| i.cmd)
                    .map_or(MenuKey::Ignored, MenuKey::Run),
                _ => MenuKey::Ignored,
            };
        }
        match key {
            "escape" => MenuKey::Close,
            "down" | "up" if self.selected >= self.items.len() => {
                // Nothing selected yet: the first row, or the last.
                let n = self.items.len();
                self.selected = if key == "down" {
                    step(&self.items, n.saturating_sub(1), 1)
                } else {
                    step(&self.items, 0, -1)
                };
                MenuKey::Moved
            }
            "down" | "up" => {
                self.selected = step(
                    &self.items,
                    self.selected,
                    if key == "down" { 1 } else { -1 },
                );
                self.close_sub();
                MenuKey::Moved
            }
            "enter" if self.selected >= self.items.len() => MenuKey::Ignored,
            "right" | "enter" if self.open_sub(self.selected, true) => MenuKey::Moved,
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
        let vp = (f32::from(viewport.width), f32::from(viewport.height));
        let main = place(
            (f32::from(self.at.x), f32::from(self.at.y)),
            menu_w(),
            list_h(&self.items),
            vp,
        );
        let selected = (!self.in_sub).then_some(self.selected);
        let list = rows(
            &t,
            main,
            &self.items,
            selected,
            self.open,
            "menu",
            slot,
            run,
            false,
            cx,
        );
        let mut rects = vec![main];
        let mut sub_list = None;
        if let Some(o) = self.open {
            let sub = self.sub_items();
            let rect = sub_place(main, row_top(&self.items, o), sub_w(), list_h(sub), vp);
            rects.push(rect);
            let shown = self.in_sub.then_some(self.sub_selected);
            sub_list = Some(rows(
                &t, rect, sub, shown, None, "submenu", slot, run, true, cx,
            ));
        }
        // The menu deferred above any open header dropdown (`dropdown::OVERLAY_PRIORITY`); its
        // backdrop at every popover's (under Flatten and Cancel all, `crate::popover`). Deferred
        // draws of one priority paint (and take the pointer) in the order they are added: the
        // submenu goes last, so where it overlaps the menu (flipped left in a narrow window) it
        // is on top.
        let root = div()
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
                            if let Some(d) = crate::popover::outside_click(true, p, &rects) {
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
            .child(gpui::deferred(list).with_priority(crate::dropdown::OVERLAY_PRIORITY));
        match sub_list {
            Some(s) => {
                root.child(gpui::deferred(s).with_priority(crate::dropdown::OVERLAY_PRIORITY))
            }
            None => root,
        }
        .into_any_element()
    }
}

/// A list's height: its rows and padding.
pub fn list_h(items: &[MenuItem]) -> f32 {
    items.iter().map(MenuItem::height).sum::<f32>() + 2.0 * pad()
}

/// Row `i`'s top, from the list's top.
fn row_top(items: &[MenuItem], i: usize) -> f32 {
    pad() + items.iter().take(i).map(MenuItem::height).sum::<f32>()
}

/// Where a menu of width `w` (narrower in a narrow window) and height `h` goes, its corner at
/// `at`: inside the window, `edge()` from its edges where it fits.
pub fn place(at: (f32, f32), w: f32, h: f32, (vw, vh): (f32, f32)) -> Rect {
    let w = w.min(vw - 2.0 * edge()).max(0.0);
    let h = h.min(vh - 2.0 * edge()).max(row_h());
    let x = at.0.min(vw - w - edge()).max(0.0);
    let y = at.1.min(vh - h - edge()).max(0.0);
    Rect::new(x, y, w, h)
}

/// A submenu beside its row (`row_top` from the menu's top): to the right of the menu, or to
/// its left where the window's right edge would cut it; level with the row, moved up to fit.
pub fn sub_place(main: Rect, row_top: f32, w: f32, h: f32, (vw, vh): (f32, f32)) -> Rect {
    let w = w.min(vw - 2.0 * edge()).max(0.0);
    let right = main.x + main.w - 2.0;
    let x = if right + w <= vw - edge() {
        right
    } else {
        (main.x - w + 2.0).max(0.0)
    };
    let top = main.y + row_top - pad();
    place((x, top), w, h, (vw, vh))
}

/// One list (the menu, or its submenu) at `rect`.
#[allow(clippy::too_many_arguments)]
fn rows<V: 'static>(
    t: &Theme,
    rect: Rect,
    items: &[MenuItem],
    selected: Option<usize>,
    open: Option<usize>,
    id: &'static str,
    slot: fn(&mut V) -> &mut Option<Menu>,
    run: fn(&mut V, Command, &mut Window, &mut Context<V>),
    is_sub: bool,
    cx: &mut Context<V>,
) -> gpui::Stateful<gpui::Div> {
    let t = *t;
    let mut list = div()
        .id(id)
        .occlude()
        .absolute()
        .left(px(rect.x))
        .top(px(rect.y))
        .w(px(rect.w))
        .max_h(px(rect.h))
        .overflow_y_scroll()
        .py_1()
        .flex()
        .flex_col()
        .bg(t.panel)
        .border_1()
        .border_color(t.axis_line)
        .rounded_md()
        .text_size(crate::zoom::ui(12.0));
    for (i, item) in items.iter().enumerate() {
        if item.is_sep() {
            list = list.child(div().my_1().h(px(1.0)).flex_none().bg(t.axis_line));
            continue;
        }
        if item.header {
            // Level with the rows' labels: padding, the 2 px edge, the check column and the
            // gap. An object's title in the main text, a section's name in small capitals.
            let (color, size, text) = if item.title {
                (t.text_dim, 12.0, item.label.clone())
            } else {
                (t.text_dim, 10.5, item.label.to_uppercase())
            };
            list = list.child(
                div()
                    .h(px(row_h()))
                    .flex_none()
                    .px_2()
                    .pl(crate::zoom::ui(30.0))
                    .flex()
                    .items_center()
                    .text_size(crate::zoom::ui(size))
                    .text_color(color)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(text),
            );
            continue;
        }
        let lit = selected == Some(i) || open == Some(i);
        let label_color = if item.destructive {
            t.destructive
        } else {
            t.text_main
        };
        // Shortcut or value, then the submenu's chevron in a column every row keeps, so all
        // of them end at one edge.
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().text_color(t.text_dim).child(item.hint.clone()))
            .child(
                div()
                    .w(crate::zoom::ui(8.0))
                    .text_color(t.text_main)
                    .child(if item.sub.is_empty() { "" } else { "›" }),
            );
        let mark = if item.toggle {
            // A switch: a box, checked when on.
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
                .child(if item.checked { "✓" } else { "" })
        } else {
            div()
                .text_color(t.text_main)
                .child(if item.checked { "✓" } else { "" })
        };
        let cmd = item.cmd;
        let has_sub = !item.sub.is_empty();
        list = list.child(
            div()
                .id((id, i))
                .h(px(row_h()))
                .flex_none()
                .px_2()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .border_l_2()
                .border_color(if lit {
                    t.accent
                } else {
                    gpui::transparent_black()
                })
                .when(lit, |d| d.bg(t.accent.opacity(0.16)))
                .child(div().w(crate::zoom::ui(12.0)).flex_none().child(mark))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(label_color)
                        .child(item.label.clone()),
                )
                .child(right.flex_none())
                .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                    if let Some(m) = slot(this).as_mut() {
                        let moved = if is_sub { m.hover_sub(i) } else { m.hover(i) };
                        if moved {
                            cx.notify();
                        }
                    }
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        if has_sub {
                            // A submenu's row opens it; it runs nothing.
                            if let Some(m) = slot(this).as_mut() {
                                m.open_sub(i, false);
                            }
                            cx.notify();
                            return;
                        }
                        let Some(cmd) = cmd else { return };
                        *slot(this) = None;
                        crate::popover::mark_moved_under_pointer();
                        run(this, cmd, window, cx);
                        cx.notify();
                    }),
                ),
        );
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;

    fn menu(items: Vec<MenuItem>) -> Menu {
        Menu::new(point(px(0.0), px(0.0)), items)
    }

    #[test]
    fn keys_skip_separators_and_headers_and_run_the_selection() {
        let mut m = menu(vec![
            MenuItem::header("Stop 2 @ 7562.00"),
            MenuItem::of(Command::ClosePane, false),
            MenuItem::sep(),
            MenuItem::of(Command::NewTab, false),
        ]);
        assert_eq!(
            m.selected, 1,
            "starts on the first command, past the header"
        );
        assert_eq!(m.key("down"), MenuKey::Moved);
        assert_eq!(m.selected, 3);
        assert_eq!(m.key("down"), MenuKey::Moved);
        assert_eq!(m.selected, 1, "wraps, skipping separators and the header");
        assert_eq!(m.key("up"), MenuKey::Moved);
        assert_eq!(m.key("enter"), MenuKey::Run(Command::NewTab));
        assert_eq!(m.key("escape"), MenuKey::Close);
        assert_eq!(m.key("x"), MenuKey::Ignored);
        assert_eq!(
            MenuItem::of(Command::ReopenTab, false).label,
            "Reopen closed tab"
        );
    }

    fn with_sub() -> Menu {
        menu(vec![
            MenuItem::of(Command::Studies, false),
            MenuItem::submenu(
                "Link",
                "A",
                vec![
                    MenuItem::header("Group"),
                    MenuItem::of(Command::SetLink(None), false),
                    MenuItem::of(Command::SetLink(Some(0)), true),
                ],
            ),
            MenuItem::of(Command::Palette, false),
        ])
    }

    #[test]
    fn right_or_enter_opens_a_submenu_and_left_or_escape_closes_it() {
        let mut m = with_sub();
        assert_eq!(m.key("right"), MenuKey::Ignored, "not a submenu row");
        m.key("down");
        assert_eq!(m.key("right"), MenuKey::Moved);
        assert_eq!(
            (m.open, m.in_sub, m.sub_selected),
            (Some(1), true, 2),
            "on the checked row"
        );
        assert_eq!(m.key("down"), MenuKey::Moved);
        assert_eq!(m.sub_selected, 1, "wraps past the header");
        assert_eq!(m.key("enter"), MenuKey::Run(Command::SetLink(None)));
        assert_eq!(m.key("left"), MenuKey::Moved);
        assert_eq!((m.open, m.in_sub, m.selected), (None, false, 1));
        assert_eq!(m.key("enter"), MenuKey::Moved, "Enter opens it too");
        assert!(m.in_sub);
        assert_eq!(m.key("escape"), MenuKey::Moved, "Esc steps back out first");
        assert_eq!(m.key("escape"), MenuKey::Close);
    }

    #[test]
    fn an_object_menu_opens_with_nothing_selected() {
        let mut m = menu(vec![
            MenuItem::title("Stop 2 @ 7562.00"),
            MenuItem::of(Command::NewTab, false),
            MenuItem::sep(),
            MenuItem::of(Command::ClosePane, false),
        ])
        .unselected();
        assert_eq!(
            m.key("enter"),
            MenuKey::Ignored,
            "a stray Enter runs nothing"
        );
        assert_eq!(m.key("down"), MenuKey::Moved);
        assert_eq!(m.selected, 1, "↓: the first row");
        let mut m = menu(vec![
            MenuItem::title("x"),
            MenuItem::of(Command::NewTab, false),
            MenuItem::of(Command::ClosePane, false),
        ])
        .unselected();
        m.key("up");
        assert_eq!(m.selected, 2, "↑: the last row");
    }

    #[test]
    fn a_submenu_closed_by_left_opens_again_on_the_pointer() {
        let mut m = with_sub();
        m.hover(1);
        m.key("right");
        m.key("left");
        assert_eq!(m.open, None);
        assert!(m.hover(1), "the pointer moving on its row opens it again");
        assert_eq!(m.open, Some(1));
    }

    #[test]
    fn hover_opens_and_closes_submenus() {
        let mut m = with_sub();
        assert!(m.hover(1));
        assert_eq!((m.open, m.in_sub), (Some(1), false));
        assert!(m.hover_sub(2));
        assert!(m.in_sub);
        assert!(!m.hover_sub(0), "a header is never selected");
        assert_eq!(m.key("enter"), MenuKey::Run(Command::SetLink(Some(0))));
        assert!(m.hover(2));
        assert_eq!(m.open, None, "another row closes it");
        assert!(!m.hover(2), "nothing changed");
    }

    #[test]
    fn submenus_are_one_level_deep() {
        let inner = MenuItem::submenu("Inner", "", vec![MenuItem::of(Command::NewTab, false)]);
        let outer = MenuItem::submenu("Outer", "", vec![inner, MenuItem::of(Command::Open, false)]);
        assert_eq!(outer.sub.len(), 1);
        assert!(outer.sub.iter().all(|i| i.sub.is_empty()));
    }

    #[test]
    fn a_submenu_flips_left_at_the_right_edge_and_stays_in_the_window() {
        let vp = (1000.0, 600.0);
        let main = place((100.0, 100.0), MENU_W_100, 200.0, vp);
        let s = sub_place(main, 30.0, SUB_W_100, 120.0, vp);
        assert!(s.x >= main.x + main.w - 2.0, "right of the menu");
        assert_eq!(s.y, main.y + 30.0 - PAD_100, "level with its row");
        let main = place((900.0, 100.0), MENU_W_100, 200.0, vp);
        assert!(main.x + main.w <= vp.0, "the menu itself stays in");
        let s = sub_place(main, 30.0, SUB_W_100, 120.0, vp);
        assert!(s.x + s.w <= main.x + 2.0, "flipped to the left");
        // Near the bottom: moved up to fit.
        let s = sub_place(main, 180.0, SUB_W_100, 300.0, (1000.0, 400.0));
        assert!(s.y + s.h <= 400.0 - EDGE_100);
        // A narrow window: nothing outside it.
        let vp = (300.0, 400.0);
        let main = place((250.0, 10.0), MENU_W_100, 200.0, vp);
        let s = sub_place(main, 10.0, SUB_W_100, 100.0, vp);
        for r in [main, s] {
            assert!(r.x >= 0.0 && r.x + r.w <= vp.0, "{r:?}");
        }
    }
}
