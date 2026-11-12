//! Settings › Keys: every command and its keys, editable (`crate::keymap` is the model).
//!
//! - **Rows are a fixed 30 px:** a command's name, its keys as caps (a click replaces that
//!   key), `+` (add a key), then ● (changed) and ↺ (back to its default). Nothing in a row
//!   grows: capture, its refusal and a conflict's question replace the keys in place or float
//!   over the rows below.
//! - **Capture:** the next keystroke is the key. Esc cancels; ⌫ removes the key being replaced.
//!   A key the rules refuse stays in the row with its reason; a key another command has asks
//!   Replace (moves it) or Cancel, in a popover under the row. While capturing, every key goes
//!   here: none runs a command (no order, no Flatten), whatever it is.
//! - **Finding:** typing filters by name; a key with Ctrl or Alt (or an F-key) finds the
//!   command it is on. Chips: All, Changed (n) and one per group. The Fixed group lists the
//!   patterns and the panes' own keys, read-only.
//! - **Saved** at once (`settings.json` `key_bindings`, only the changes) and installed, so
//!   menus, tooltips, the palette, F1 and the DOM read it at once.

use gpui::{
    AnyElement, App, Context, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, SharedString, Styled, div, prelude::FluentBuilder,
};

use crate::commands::{self, Command};
use crate::keymap::{self, Check, Combo, Keymap};
use crate::settings_view::SettingsWindow;
use crate::theme::Theme;

/// A row's height (logical px at 100 %): fixed, whatever it shows.
pub const ROW_H: f32 = 30.0;

/// Which commands the list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Chip {
    #[default]
    All,
    Changed,
    Group(&'static str),
    /// The patterns and the panes' own keys (read-only).
    Fixed,
}

/// A key being captured for a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    pub cmd: Command,
    /// The key it replaces (`None`: added).
    pub old: Option<Combo>,
    /// The last key tried and why it was refused (shown in the row; capture stays open).
    pub refused: Option<(String, String)>,
}

/// A key another command has: Replace moves it, Cancel leaves both as they were.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub cmd: Command,
    pub old: Option<Combo>,
    pub key: Combo,
    pub other: Command,
}

/// What the page holds between frames.
#[derive(Default)]
pub(crate) struct KeysPage {
    pub filter: String,
    pub chip: Chip,
    pub capture: Option<Capture>,
    pub conflict: Option<Conflict>,
    /// The keyboard's row (an index into the rows shown).
    pub focus: usize,
    /// Scroll the focused (or capturing) row into view on the next frame.
    pub reveal: bool,
    /// That row's place among the elements [`list`] returned (set as it draws).
    pub at: std::cell::Cell<Option<usize>>,
}

impl Drop for KeysPage {
    fn drop(&mut self) {
        keymap::set_capturing(false);
    }
}

/// What a press on the page asks for.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Action {
    Capture(Command, Option<Combo>),
    Reset(Command),
    ResetAll,
    Chip(Chip),
    Replace,
    CancelConflict,
}

/// Whether `c` matches the filter: every word in its name, or (a key typed) one of its keys.
fn matches(m: &Keymap, c: Command, filter: &str) -> bool {
    let f = filter.trim();
    if f.is_empty() {
        return true;
    }
    // A key (pressed, or typed with Ctrl or Alt, or an F-key): that exact key only.
    if let Some(k) = Combo::parse(f)
        && (k.ctrl || k.alt || k.key.starts_with('f') && k.key[1..].parse::<u8>().is_ok())
    {
        return m.keys(c).any(|x| *x == k);
    }
    let label = commands::label(c).to_lowercase();
    let mac = cfg!(target_os = "macos");
    let keys: String = m
        .keys(c)
        .map(|k| k.text(mac).to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    f.to_lowercase()
        .split_whitespace()
        .all(|w| label.contains(w) || keys.contains(w))
}

/// The rows shown: (group heading or command), in the groups' order.
pub(crate) fn rows(m: &Keymap, chip: Chip, filter: &str) -> Vec<Command> {
    let all = keymap::bindable();
    let mut out = Vec::new();
    if chip == Chip::Fixed {
        return out;
    }
    for g in keymap::GROUPS {
        if let Chip::Group(want) = chip
            && want != g
        {
            continue;
        }
        out.extend(all.iter().copied().filter(|c| {
            keymap::group(*c) == g
                && (chip != Chip::Changed || m.changed(*c))
                && matches(m, *c, filter)
        }));
    }
    out
}

impl KeysPage {
    /// Leaving the page: capture and a conflict's question are dropped.
    pub(crate) fn leave(&mut self) {
        self.capture = None;
        self.conflict = None;
        keymap::set_capturing(false);
    }

    /// The focused row inside the rows shown (a change can drop rows: Changed).
    fn clamp(&mut self) {
        let n = rows(&keymap::snapshot(), self.chip, &self.filter).len();
        self.focus = self.focus.min(n.saturating_sub(1));
    }

    /// Whether a key is being captured (every key goes to the page).
    pub(crate) fn capturing(&self) -> bool {
        self.capture.is_some() || self.conflict.is_some()
    }

    /// Apply an edit to the installed table: saved and installed at once.
    fn commit(m: Keymap, cx: &mut App) {
        let saved = m.saved();
        keymap::install(m);
        crate::picker_view::update_settings(cx, |s| s.key_bindings = saved);
    }

    pub(crate) fn press(&mut self, a: Action, cx: &mut App) {
        self.press_inner(a, cx);
        self.clamp();
        keymap::set_capturing(self.capturing());
    }

    fn press_inner(&mut self, a: Action, cx: &mut App) {
        match a {
            Action::Capture(cmd, old) => {
                self.conflict = None;
                self.reveal = true;
                self.capture = Some(Capture {
                    cmd,
                    old,
                    refused: None,
                });
            }
            Action::Reset(cmd) => {
                self.leave();
                let mut m = keymap::snapshot();
                m.reset(cmd);
                Self::commit(m, cx);
                self.reveal = false;
            }
            Action::ResetAll => {
                self.leave();
                Self::commit(Keymap::defaults(), cx);
                self.reveal = false;
            }
            Action::Chip(c) => {
                self.leave();
                self.chip = c;
                self.focus = 0;
            }
            Action::Replace => {
                if let Some(c) = self.conflict.take() {
                    let mut m = keymap::snapshot();
                    match m.set(c.cmd, c.old.as_ref(), c.key.clone(), true) {
                        Ok(()) => {
                            Self::commit(m, cx);
                            self.capture = None;
                        }
                        Err(why) => {
                            self.capture = Some(Capture {
                                cmd: c.cmd,
                                old: c.old,
                                refused: Some((c.key.text(cfg!(target_os = "macos")), why)),
                            });
                        }
                    }
                }
            }
            Action::CancelConflict => {
                self.conflict = None;
                self.capture = None;
            }
        }
    }

    /// A key on the page. `true`: handled (never passed on: while capturing, every key).
    pub(crate) fn key(&mut self, ev: &KeyDownEvent, cx: &mut App) -> bool {
        let handled = self.key_inner(ev, cx);
        self.clamp();
        keymap::set_capturing(self.capturing());
        handled
    }

    fn key_inner(&mut self, ev: &KeyDownEvent, cx: &mut App) -> bool {
        // A held key's repeats: only ↑ ↓ and ⌫ repeat (moving, erasing); a key held through
        // a capture never lands as a key or in the search.
        if ev.is_held
            && (self.capturing()
                || !matches!(ev.keystroke.key.as_str(), "up" | "down" | "backspace"))
        {
            return true;
        }
        let k = &ev.keystroke;
        let m = k.modifiers;
        let key = k.key.as_str();
        let plain = !m.control && !m.alt && !m.platform && !m.shift;
        if self.conflict.is_some() {
            match key {
                "enter" if plain => self.press(Action::Replace, cx),
                "escape" => self.press(Action::CancelConflict, cx),
                _ => {}
            }
            return true;
        }
        if let Some(cap) = self.capture.clone() {
            // A modifier alone: wait for the key.
            if matches!(
                key,
                "shift" | "control" | "alt" | "platform" | "function" | "fn" | "cmd" | "ctrl"
            ) {
                return true;
            }
            if plain && key == "escape" {
                self.capture = None;
                return true;
            }
            if plain && key == "backspace" {
                match &cap.old {
                    Some(old) => {
                        let mut t = keymap::snapshot();
                        match t.remove(cap.cmd, old) {
                            Ok(()) => {
                                Self::commit(t, cx);
                                self.capture = None;
                            }
                            Err(why) => {
                                self.capture = Some(Capture {
                                    refused: Some((old.text(cfg!(target_os = "macos")), why)),
                                    ..cap
                                })
                            }
                        }
                    }
                    None => self.capture = None,
                }
                return true;
            }
            match captured(k) {
                Err((text, why)) => {
                    self.capture = Some(Capture {
                        refused: Some((text, why)),
                        ..cap
                    });
                }
                Ok(combo) => {
                    let mut t = keymap::snapshot();
                    let mac = cfg!(target_os = "macos");
                    match t.check(cap.cmd, &combo) {
                        Check::Same => self.capture = None,
                        Check::Free => match t.set(cap.cmd, cap.old.as_ref(), combo.clone(), false)
                        {
                            Ok(()) => {
                                Self::commit(t, cx);
                                self.capture = None;
                            }
                            Err(why) => {
                                self.capture = Some(Capture {
                                    refused: Some((combo.text(mac), why)),
                                    ..cap
                                })
                            }
                        },
                        Check::Conflict(other) => {
                            self.conflict = Some(Conflict {
                                cmd: cap.cmd,
                                old: cap.old.clone(),
                                key: combo,
                                other,
                            });
                        }
                        Check::Refused(why) => {
                            self.capture = Some(Capture {
                                refused: Some((combo.text(mac), why)),
                                ..cap
                            });
                        }
                    }
                }
            }
            return true;
        }
        // Not capturing: find, move, start.
        let table = keymap::snapshot();
        let n = rows(&table, self.chip, &self.filter).len();
        match key {
            "up" if plain => {
                self.focus = self.focus.saturating_sub(1);
                self.reveal = true;
            }
            "down" if plain => {
                self.focus = (self.focus + 1).min(n.saturating_sub(1));
                self.reveal = true;
            }
            "enter" if plain => {
                if let Some(c) = rows(&table, self.chip, &self.filter).get(self.focus) {
                    let old = table.keys(*c).find(|k| !keymap::fixed(*c, k)).cloned();
                    self.press(Action::Capture(*c, old), cx);
                }
            }
            "escape" if !self.filter.is_empty() => {
                self.filter.clear();
                self.focus = 0;
            }
            "backspace" if !self.filter.is_empty() => {
                self.filter.pop();
                self.focus = 0;
            }
            "escape" | "tab" | "backspace" => return false,
            _ => {
                // A key with Ctrl or Alt (or an F-key): find the command it is on.
                let fkey = key.len() > 1 && key.starts_with('f') && key[1..].parse::<u8>().is_ok();
                if m.control || m.alt || m.platform || fkey {
                    // The window's zoom keys stay the window's.
                    if matches!(
                        commands::command_for(k),
                        Some(Command::ZoomIn | Command::ZoomOut | Command::ZoomReset)
                    ) {
                        return false;
                    }
                    if let Ok(c) = captured(k) {
                        self.filter = c.text(cfg!(target_os = "macos"));
                        self.focus = 0;
                        return true;
                    }
                    return false;
                }
                match k.key_char.as_deref() {
                    Some(c) if !c.chars().any(char::is_control) => {
                        self.filter.push_str(c);
                        self.focus = 0;
                    }
                    _ => return false,
                }
            }
        }
        true
    }
}

/// A keystroke as a key, or why it can't be one whatever the command: the OS key (Win, or
/// Ctrl on macOS) and AltGr characters are never keys (the AltGr guard of `command_for`).
pub(crate) fn captured(k: &gpui::Keystroke) -> Result<Combo, (String, String)> {
    let m = k.modifiers;
    let mac = cfg!(target_os = "macos");
    let key = k.key.as_str();
    let ctrl = if mac { m.platform } else { m.control };
    let c = Combo::new(ctrl, m.shift, m.alt, key);
    if (mac && m.control) || (!mac && m.platform) {
        let os = if mac { "Ctrl" } else { "The Windows key" };
        return Err((c.text(mac), format!("{os} belongs to the system")));
    }
    if !mac
        && m.control
        && m.alt
        && k.key_char
            .as_deref()
            .is_some_and(|ch| !ch.is_empty() && !ch.eq_ignore_ascii_case(key))
    {
        return Err((c.text(mac), "AltGr types a character here".to_owned()));
    }
    Ok(c)
}

fn press(
    a: Action,
    cx: &mut Context<SettingsWindow>,
) -> impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut App) + 'static {
    cx.listener(
        move |v: &mut SettingsWindow, _: &gpui::MouseDownEvent, _, cx| {
            cx.stop_propagation();
            v.keys_action(a.clone(), cx)
        },
    )
}

/// A key's caps (one per key), clickable when `action` is given.
fn key_caps(
    id: String,
    k: &Combo,
    action: Option<Action>,
    t: &Theme,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let mac = cfg!(target_os = "macos");
    let caps = crate::keycap::caps_of(&k.caps(mac), t).unwrap_or_else(div);
    div()
        .id(SharedString::from(id))
        .flex_none()
        .rounded_sm()
        .p(gpui::px(1.0))
        .when_some(action, |d, a| {
            d.cursor_pointer()
                .hover(|s| s.bg(t.ghost_hover))
                .on_mouse_down(MouseButton::Left, press(a, cx))
        })
        .child(caps)
        .into_any_element()
}

fn push(out: &mut Vec<AnyElement>, e: impl IntoElement) {
    out.push(e.into_any_element());
}

/// A small text button.
fn button(
    id: String,
    label: &str,
    action: Option<Action>,
    on: bool,
    t: &Theme,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let can = action.is_some();
    div()
        .id(SharedString::from(id))
        .flex_none()
        .whitespace_nowrap()
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(if on {
            t.accent
        } else if can {
            t.control_edge
        } else {
            t.axis_line
        })
        .when(on, |d| d.bg(t.element_selected))
        .text_color(if can { t.text_main } else { t.text_dim })
        .when_some(action, |d, a| {
            d.cursor_pointer()
                .hover(|s| s.bg(t.ghost_hover))
                .on_mouse_down(MouseButton::Left, press(a, cx))
        })
        .child(label.to_owned())
        .into_any_element()
}

/// The page.
pub(crate) fn list(
    page: &KeysPage,
    t: &Theme,
    cx: &mut Context<SettingsWindow>,
) -> Vec<AnyElement> {
    let table = keymap::snapshot();
    let mac = cfg!(target_os = "macos");
    let changed = table.changed_count();
    let shown = rows(&table, page.chip, &page.filter);
    let row_h = crate::zoom::ui(ROW_H);
    // The filter and Reset all.
    let filter = div()
        .flex_1()
        .min_w_0()
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(if page.filter.is_empty() {
            t.axis_line
        } else {
            t.accent
        })
        .text_color(if page.filter.is_empty() {
            t.text_dim
        } else {
            t.text_main
        })
        .truncate()
        .child(if page.filter.is_empty() {
            "Search commands or press a key…".to_owned()
        } else {
            format!("{}\u{258f}", page.filter)
        });
    let head = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .mt_1()
        .child(filter)
        .child(button(
            "keys-reset-all".into(),
            "Reset all",
            (changed > 0).then_some(Action::ResetAll),
            false,
            t,
            cx,
        ));
    // The chips.
    let mut chips = div().flex().flex_row().flex_wrap().gap_1().mt_2();
    chips = chips.child(button(
        "keys-chip-all".into(),
        "All",
        Some(Action::Chip(Chip::All)),
        page.chip == Chip::All,
        t,
        cx,
    ));
    chips = chips.child(button(
        "keys-chip-changed".into(),
        &format!("Changed ({changed})"),
        Some(Action::Chip(Chip::Changed)),
        page.chip == Chip::Changed,
        t,
        cx,
    ));
    for (j, g) in keymap::GROUPS.iter().enumerate() {
        chips = chips.child(button(
            format!("keys-chip-{j}"),
            g,
            Some(Action::Chip(Chip::Group(g))),
            page.chip == Chip::Group(g),
            t,
            cx,
        ));
    }
    chips = chips.child(button(
        "keys-chip-fixed".into(),
        "Fixed",
        Some(Action::Chip(Chip::Fixed)),
        page.chip == Chip::Fixed,
        t,
        cx,
    ));
    let mut out: Vec<AnyElement> = vec![
        div()
            .flex()
            .flex_col()
            .child(head)
            .child(chips)
            .into_any_element(),
    ];
    page.at.set(None);
    let mut group = "";
    if shown.is_empty() && page.chip != Chip::Fixed {
        push(
            &mut out,
            div()
                .mt_2()
                .text_color(t.text_dim)
                .child("No command matches"),
        );
    }
    for (i, c) in shown.iter().enumerate() {
        let c = *c;
        let g = keymap::group(c);
        if g != group {
            group = g;
            push(
                &mut out,
                div()
                    .pt_3()
                    .pb_1()
                    .text_size(crate::zoom::ui(11.0))
                    .text_color(t.text_dim)
                    .child(g.to_uppercase()),
            );
        }
        let focused = i == page.focus && !page.capturing();
        let capture = page.capture.as_ref().filter(|x| x.cmd == c);
        let conflict = page.conflict.as_ref().filter(|x| x.cmd == c);
        let keys: Vec<Combo> = table.keys(c).cloned().collect();
        // The keys area: caps (a click replaces one), +; or the capture.
        let area: AnyElement = match (capture, conflict) {
            (_, Some(cf)) => {
                // The key asked about, and the question floating under the row.
                let trigger = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(t.accent)
                    .child(crate::keycap::caps_of(&cf.key.caps(mac), t).unwrap_or_else(div));
                let popup = div()
                    .id("keys-conflict")
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(t.accent)
                    .bg(t.panel)
                    .flex()
                    .flex_col()
                    .gap_1()
                    .max_w(crate::zoom::ui(340.0))
                    .child(div().text_color(t.text_main).child(format!(
                        "{} is already {}.",
                        cf.key.text(mac),
                        commands::label(cf.other)
                    )))
                    .child(div().text_color(t.text_dim).child(format!(
                        "Use it here and remove it from {}?",
                        commands::label(cf.other)
                    )))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(button(
                                "keys-conflict-cancel".into(),
                                "Cancel",
                                Some(Action::CancelConflict),
                                false,
                                t,
                                cx,
                            ))
                            .child(button(
                                "keys-conflict-replace".into(),
                                "Replace",
                                Some(Action::Replace),
                                true,
                                t,
                                cx,
                            )),
                    );
                crate::dropdown::dropdown(trigger, Some(popup.into_any_element()))
                    .on_dismiss(Box::new(cx.listener(
                        |v: &mut SettingsWindow, _: &gpui::MouseDownEvent, _, cx| {
                            v.keys.leave();
                            cx.notify();
                        },
                    )))
                    .into_any_element()
            }
            (Some(cap), None) => {
                let field = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .px_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(if cap.refused.is_some() {
                        t.destructive
                    } else {
                        t.accent
                    });
                match &cap.refused {
                    Some((text, why)) => field
                        .child(
                            div()
                                .flex_none()
                                .text_color(t.text_main)
                                .child(text.clone()),
                        )
                        .child(div().flex_none().text_color(t.destructive).child("!"))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(t.text_main)
                                // The key is already shown: its reason only.
                                .child(
                                    why.strip_prefix(text.as_str())
                                        .and_then(|w| w.strip_prefix(": "))
                                        .unwrap_or(why)
                                        .to_owned(),
                                ),
                        )
                        .into_any_element(),
                    None => field
                        .text_color(t.text_dim)
                        .child("Press keys… (Esc cancels, ⌫ removes)")
                        .into_any_element(),
                }
            }
            (None, None) => {
                let mut d = div().flex().flex_row().items_center().gap_1();
                for (j, k) in keys.iter().enumerate() {
                    let action =
                        (!keymap::fixed(c, k)).then(|| Action::Capture(c, Some(k.clone())));
                    d = d.child(key_caps(format!("keys-{i}-{j}"), k, action, t, cx));
                }
                d.child(button(
                    format!("keys-{i}-add"),
                    "+",
                    Some(Action::Capture(c, None)),
                    false,
                    t,
                    cx,
                ))
                .into_any_element()
            }
        };
        let changed_here = table.changed(c);
        if focused || capture.is_some() || conflict.is_some() {
            page.at.set(Some(out.len()));
        }
        push(
            &mut out,
            div()
                .id(("keys-row", i))
                .h(row_h)
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .pl(crate::zoom::ui(10.0))
                .border_b_1()
                .border_color(t.axis_line)
                .border_l_2()
                .border_color(if focused {
                    t.accent
                } else {
                    gpui::transparent_black()
                })
                .when(focused, |d| d.bg(t.ghost_selected))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(t.text_main)
                        .child(commands::label(c)),
                )
                .child(
                    div()
                        .flex_none()
                        .min_w_0()
                        .max_w(gpui::relative(0.62))
                        .overflow_hidden()
                        .child(area),
                )
                // ● and ↺: always their slots, so a change moves nothing.
                .child(
                    div()
                        .flex_none()
                        .w(crate::zoom::ui(12.0))
                        .text_color(t.accent)
                        .when(changed_here, |d| d.child("●")),
                )
                .child(
                    div()
                        .id(("keys-reset", i))
                        .flex_none()
                        .w(crate::zoom::ui(24.0))
                        .flex()
                        .justify_center()
                        .when(changed_here, |d| {
                            d.cursor_pointer()
                                .text_color(t.text_main)
                                .hover(|s| s.bg(t.ghost_hover))
                                .on_mouse_down(MouseButton::Left, press(Action::Reset(c), cx))
                                .child("↺")
                        }),
                ),
        );
    }
    // The fixed keys: patterns and the panes' own, read-only.
    if page.chip == Chip::Fixed || page.chip == Chip::All && page.filter.trim().is_empty() {
        push(
            &mut out,
            div()
                .pt_3()
                .pb_1()
                .text_size(crate::zoom::ui(11.0))
                .text_color(t.text_dim)
                .child("FIXED"),
        );
        let ctrl = if mac { "⌘" } else { "Ctrl" };
        let alt = if mac { "⌥" } else { "Alt" };
        for (name, keys) in [
            ("Focus pane", vec![alt.to_owned(), "←↑→↓".to_owned()]),
            ("Go to tab 1–9", vec![ctrl.to_owned(), "1–9".to_owned()]),
            (
                "Symbol and timeframe typing in a pane",
                vec!["A–Z".to_owned(), "0–9".to_owned()],
            ),
            (
                "DOM: center, grouping",
                vec!["C".to_owned(), "G".to_owned()],
            ),
            ("Chart: reset the view", vec!["Home".to_owned()]),
            ("Symbol picker", vec![ctrl.to_owned(), "K".to_owned()]),
            (
                "Picker: favorite, size",
                vec![
                    ctrl.to_owned(),
                    "D".to_owned(),
                    alt.to_owned(),
                    "M".to_owned(),
                ],
            ),
            (
                "Chart: volume profile",
                vec![alt.to_owned(), "V R B G".to_owned()],
            ),
            (
                "Lists and fields",
                vec!["Esc".to_owned(), "Enter".to_owned()],
            ),
        ] {
            push(
                &mut out,
                div()
                    .h(row_h)
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .pl(crate::zoom::ui(10.0))
                    .pr(crate::zoom::ui(44.0))
                    .border_b_1()
                    .border_color(t.axis_line)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(t.text_dim)
                            .child(name),
                    )
                    .children(crate::keycap::caps_of(&keys, t)),
            );
        }
    }
    out
}

/// Screenshots (`--settings keys --settings-set keys:…`): `keys:bind:ID=K1,K2` sets a
/// command's keys (empty: none), `keys:capture:ID` (or `ID@N`, replacing its Nth key) opens
/// capture, `keys:press:KEY` presses a key on the page ("ctrl-shift-f"), `keys:filter:Q`
/// types a filter, `keys:chip:changed|all|GROUP` picks a chip. `false`: not one of these.
#[cfg(feature = "screenshot")]
pub(crate) fn shot(page: &mut KeysPage, s: &str, cx: &mut App) -> Result<bool, String> {
    let Some(rest) = s.strip_prefix("keys:") else {
        return Ok(false);
    };
    let by_id = |id: &str| {
        keymap::bindable()
            .into_iter()
            .find(|c| keymap::id(*c) == id)
            .ok_or_else(|| format!("{s:?}: no command {id:?}"))
    };
    if let Some(b) = rest.strip_prefix("bind:") {
        let (id, keys) = b.split_once('=').ok_or_else(|| format!("{s:?}: ID=KEYS"))?;
        by_id(id)?;
        let mut saved = keymap::snapshot().saved();
        saved.insert(
            id.to_owned(),
            keys.split(',')
                .filter(|k| !k.is_empty())
                .map(str::to_owned)
                .collect(),
        );
        let (m, skipped) = Keymap::with(&saved);
        if let Some(why) = skipped.first() {
            return Err(format!("{s:?}: {why}"));
        }
        KeysPage::commit(m, cx);
    } else if let Some(c) = rest.strip_prefix("capture:") {
        let (id, n) = match c.split_once('@') {
            Some((id, n)) => (
                id,
                Some(n.parse::<usize>().map_err(|_| format!("{s:?}: @N"))?),
            ),
            None => (c, None),
        };
        let cmd = by_id(id)?;
        let old = n.and_then(|n| keymap::snapshot().keys(cmd).nth(n).cloned());
        page.press(Action::Capture(cmd, old), cx);
    } else if let Some(k) = rest.strip_prefix("press:") {
        let keystroke = gpui::Keystroke::parse(Box::leak(k.to_owned().into_boxed_str()))
            .map_err(|e| format!("{s:?}: {e}"))?;
        page.key(
            &KeyDownEvent {
                keystroke,
                is_held: false,
                prefer_character_input: false,
            },
            cx,
        );
    } else if let Some(q) = rest.strip_prefix("filter:") {
        page.filter = q.to_owned();
    } else if let Some(c) = rest.strip_prefix("chip:") {
        page.chip = match c {
            "all" => Chip::All,
            "changed" => Chip::Changed,
            "fixed" => Chip::Fixed,
            g => Chip::Group(
                keymap::GROUPS
                    .iter()
                    .copied()
                    .find(|x| x.eq_ignore_ascii_case(g))
                    .ok_or_else(|| format!("{s:?}: no group {g:?}"))?,
            ),
        };
    } else {
        return Err(format!("{s:?}: keys:bind|capture|press|filter|chip"));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::TradeCmd;

    #[test]
    fn rows_filter_by_name_key_group_and_changed() {
        let mut m = Keymap::defaults();
        let all = rows(&m, Chip::All, "");
        assert_eq!(all.len(), keymap::bindable().len());
        // By a key: the command on it.
        let f = rows(&m, Chip::All, "ctrl+shift+f");
        assert_eq!(f, vec![Command::Trade(TradeCmd::Flatten)]);
        // Exactly that key: not one that starts with it (Ctrl+Shift+F9).
        m.set(
            Command::NewTab,
            None,
            Combo::parse("ctrl+shift+f9").unwrap(),
            false,
        )
        .unwrap();
        assert_eq!(rows(&m, Chip::All, "Ctrl+Shift+F"), f);
        m.reset(Command::NewTab);
        // By words.
        assert!(rows(&m, Chip::All, "flatten").contains(&Command::Trade(TradeCmd::Flatten)));
        // A group.
        assert!(
            rows(&m, Chip::Group("Trading"), "")
                .iter()
                .all(|c| keymap::group(*c) == "Trading")
        );
        // Changed: none, then the one changed.
        assert!(rows(&m, Chip::Changed, "").is_empty());
        m.set(
            Command::NewTab,
            None,
            Combo::parse("ctrl+alt+f11").unwrap(),
            false,
        )
        .unwrap();
        assert_eq!(rows(&m, Chip::Changed, ""), vec![Command::NewTab]);
    }
}
