//! Settings › Plugins: one card per installed plugin, saying what it is ("Level source") and
//! drawing its settings from its own description (`crate::plugin_settings`; the kinds are a
//! closed set, so a plugin never draws its own UI and nothing here names one). The same model
//! as the Studies menu's "Source settings", in Settings' row style: switches as Off | On,
//! choices as a dropdown whose list floats, numbers and seconds with − / + (a click on the
//! value types it), text and addresses as fields, each setting's help under it.
//!
//! A refused value keeps its text with the reason under it (nothing applied); a switch that
//! asks shows its question under it until answered; a value taken from the plugin's
//! environment says so. Every control is reachable from the keyboard: ↑/↓ move between them,
//! Enter or Space press, typing goes to a field being typed (Enter applies, Esc goes back).

use std::cell::{Cell, RefCell};

use gpui::{
    AnyElement, App, Context, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, SharedString, Styled, div, prelude::FluentBuilder,
};
use lines_feed::levels::{SettingKind, setting};

use crate::drop_list::{DropList, Entry, Outcome};
use crate::plugin_settings::{self as ps, Row};
use crate::settings_view::SettingsWindow;
use crate::theme::Theme;

/// A setting being typed.
pub(crate) struct Edit {
    pub source: String,
    pub key: &'static str,
    pub buf: String,
}

/// A choice's list, open.
pub(crate) struct Open {
    pub source: String,
    pub key: &'static str,
    pub values: Vec<String>,
    pub list: DropList,
    pub scroll: gpui::ScrollHandle,
}

/// What the page holds between frames.
#[derive(Default)]
pub(crate) struct PluginPage {
    pub(crate) edit: Option<Edit>,
    pub(crate) open: Option<Open>,
    /// A switch's question, asked before it turns on: (plugin, key).
    asking: Option<(String, &'static str)>,
    /// The keyboard's control, an index into `controls`.
    focus: Cell<usize>,
    /// The page's controls in drawing order, as last drawn (what ↑/↓ and Enter act on).
    controls: RefCell<Vec<Action>>,
}

/// What a press on the page asks for (`SettingsWindow::plugin_action` does it).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Action {
    /// Set a value (a switch that asks first asks).
    Set(String, &'static str, String),
    /// Start typing a value, from this text.
    Edit(String, &'static str, String),
    /// Open a choice's list.
    Open(String, &'static str),
    /// The question answered: turn on (`true`), or leave it.
    Answer(bool),
    /// A control that can't act now (a step at its end): keeps its place in the keyboard's
    /// order, so the focus never moves to another setting under it.
    Nothing,
}

impl PluginPage {
    /// Leaving the page: a value being typed, a list and a question are dropped (no key lands
    /// in them unseen).
    pub(crate) fn leave(&mut self) {
        self.edit = None;
        self.open = None;
        self.asking = None;
        self.focus.set(0);
    }

    /// A press (`plugin_action`): the page's own part; `Some` for the window to apply.
    pub(crate) fn press(&mut self, a: Action, cx: &mut App) {
        match a {
            Action::Nothing => {}
            Action::Set(src, key, value) => {
                // Another press ends typing and closes a list.
                self.edit = None;
                self.open = None;
                let asks = row_of(&src, key, cx).is_some_and(|r| r.asks(&value));
                if asks {
                    self.asking = Some((src, key));
                } else {
                    self.asking = None;
                    // Refused: kept with its reason (shown here and in the Studies menu).
                    let _ = ps::set(cx, &src, key, &value);
                }
            }
            Action::Edit(source, key, buf) => {
                self.asking = None;
                self.open = None;
                self.edit = Some(Edit { source, key, buf });
            }
            Action::Open(source, key) => {
                self.edit = None;
                let Some(r) = row_of(&source, key, cx) else {
                    return;
                };
                let values: Vec<String> = r.options.iter().map(|(v, _)| v.clone()).collect();
                let current = values.iter().position(|v| v.eq_ignore_ascii_case(&r.value));
                let entries = r
                    .options
                    .iter()
                    .map(|(_, l)| Entry::new(l.clone()))
                    .collect();
                let list = DropList::new(entries, false, false, current);
                let scroll = gpui::ScrollHandle::new();
                if let Some(k) = current {
                    let _ = k;
                    crate::drop_view::reveal(&scroll, &list, true);
                }
                self.open = Some(Open {
                    source,
                    key,
                    values,
                    list,
                    scroll,
                });
            }
            Action::Answer(yes) => {
                if let Some((src, key)) = self.asking.take()
                    && yes
                {
                    let _ = ps::set(cx, &src, key, "true");
                }
            }
        }
    }

    /// A key on the page. `true`: handled (redraw).
    pub(crate) fn key(&mut self, ev: &KeyDownEvent, cx: &mut App) -> bool {
        let k = &ev.keystroke;
        let m = k.modifiers;
        if let Some(o) = self.open.as_mut() {
            match o
                .list
                .key(&k.key, k.key_char.as_deref(), std::time::Instant::now())
            {
                Outcome::Pick(i) => {
                    let o = self.open.take().expect("open");
                    if let Some(v) = o.values.get(i) {
                        self.press(Action::Set(o.source, o.key, v.clone()), cx);
                    }
                }
                Outcome::Close => self.open = None,
                _ => {}
            }
            return true;
        }
        if let Some(e) = self.edit.as_mut() {
            match k.key.as_str() {
                "enter" => {
                    let e = self.edit.take().expect("edit");
                    let _ = ps::set(cx, &e.source, e.key, &e.buf);
                }
                "escape" => {
                    let e = self.edit.take().expect("edit");
                    ps::discard(&e.source, e.key);
                }
                "backspace" => {
                    e.buf.pop();
                }
                "v" if (m.control || m.platform) && !m.alt => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        e.buf
                            .extend(text.trim().chars().filter(|c| !c.is_control()));
                    }
                }
                _ => {
                    if let Some(c) = k.key_char.as_deref().filter(|_| !m.control && !m.alt) {
                        e.buf.extend(c.chars().filter(|c| !c.is_control()));
                    }
                }
            }
            return true;
        }
        if self.asking.is_some() && k.key == "escape" {
            self.asking = None;
            return true;
        }
        if m.control || m.alt {
            return false;
        }
        let n = self.controls.borrow().len();
        if n == 0 {
            return false;
        }
        match k.key.as_str() {
            "up" => self
                .focus
                .set(self.focus.get().min(n - 1).saturating_sub(1)),
            "down" => self.focus.set((self.focus.get() + 1).min(n - 1)),
            "enter" | "space" => {
                let a = self
                    .controls
                    .borrow()
                    .get(self.focus.get().min(n - 1))
                    .cloned();
                if let Some(a) = a {
                    self.press(a, cx);
                }
            }
            _ => return false,
        }
        true
    }
}

/// Plugin `source`'s setting `key` as shown now.
fn row_of(source: &str, key: &str, cx: &App) -> Option<Row> {
    let s = lines_feed::levels::registered()
        .into_iter()
        .find(|s| s.id() == source)?;
    ps::rows(&*s, crate::picker_view::settings(cx).plugins.get(source))
        .into_iter()
        .find(|r| r.spec.key == key)
}

/// Registers the page's controls in drawing order (for the keyboard) and says which one has
/// the keyboard.
struct Ctl<'a> {
    page: &'a PluginPage,
    next: usize,
}

impl Ctl<'_> {
    fn take(&mut self, a: &Action) -> bool {
        self.page.controls.borrow_mut().push(a.clone());
        let focused = self.next == self.page.focus.get();
        self.next += 1;
        focused
    }
}

fn press(
    a: Action,
    cx: &mut Context<SettingsWindow>,
) -> impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut App) + 'static {
    cx.listener(
        move |v: &mut SettingsWindow, _: &gpui::MouseDownEvent, _, cx| {
            cx.stop_propagation();
            v.plugin_action(a.clone(), cx)
        },
    )
}

/// A small bordered control (a step, a button).
fn button(
    id: String,
    label: &str,
    action: Option<Action>,
    ctl: &mut Ctl<'_>,
    t: &Theme,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let focused = ctl.take(action.as_ref().unwrap_or(&Action::Nothing));
    let can = action.is_some();
    div()
        .id(SharedString::from(id))
        .flex_none()
        .whitespace_nowrap()
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(if focused {
            t.accent
        } else if can {
            t.control_edge
        } else {
            t.axis_line
        })
        .text_color(if can { t.text_main } else { t.text_dim })
        .when_some(action, |d, a| {
            d.cursor_pointer()
                .on_mouse_down(MouseButton::Left, press(a, cx))
        })
        .child(label.to_owned())
        .into_any_element()
}

/// Off | On, the current one filled: a state, never a lone button.
fn switch(
    id: String,
    src: &str,
    r: &Row,
    ctl: &mut Ctl<'_>,
    t: &Theme,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let on = setting::is_on(&r.value);
    let mut seg = div()
        .flex()
        .flex_row()
        .flex_none()
        .border_1()
        .border_color(t.axis_line)
        .rounded_sm();
    for (j, (name, val)) in [("Off", false), ("On", true)].into_iter().enumerate() {
        let a = Action::Set(src.to_owned(), r.spec.key, val.to_string());
        let focused = ctl.take(&a);
        seg = seg.child(
            div()
                .id(SharedString::from(format!("{id}-{j}")))
                .px_2()
                .whitespace_nowrap()
                .cursor_pointer()
                .border_1()
                .border_color(if focused {
                    t.accent
                } else {
                    gpui::transparent_black()
                })
                .when(on == val, |d| d.bg(t.accent).text_color(t.bg))
                .child(name)
                .on_mouse_down(MouseButton::Left, press(a, cx)),
        );
    }
    seg.into_any_element()
}

#[allow(clippy::too_many_arguments)]
/// A field's box: its text, the placeholder dimmed while empty, the accent while typed or
/// focused, the destructive edge while refused.
fn value_box(
    id: String,
    text: String,
    placeholder: &str,
    wide: bool,
    state: (bool, bool),
    action: Action,
    ctl: &mut Ctl<'_>,
    t: &Theme,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let (typing, refused) = state;
    let focused = ctl.take(&action);
    let empty = text.is_empty();
    div()
        .id(SharedString::from(id))
        .flex_none()
        .when(wide, |d| d.min_w(crate::zoom::ui(220.0)))
        .when(!wide, |d| {
            d.min_w(crate::zoom::ui(64.0)).flex().justify_end()
        })
        .px_2()
        .rounded_sm()
        .border_1()
        // Refused shows even with the keyboard on it (only typing outlines it in the accent).
        .border_color(if typing {
            t.accent
        } else if refused {
            t.destructive
        } else if focused {
            t.accent
        } else {
            t.axis_line
        })
        .whitespace_nowrap()
        .text_color(if empty { t.text_dim } else { t.text_main })
        .cursor_text()
        .on_mouse_down(MouseButton::Left, press(action, cx))
        .child(if empty { placeholder.to_owned() } else { text })
        .into_any_element()
}

/// The page: one card per installed plugin with settings.
pub(crate) fn list(page: &PluginPage, t: &Theme, cx: &mut Context<SettingsWindow>) -> AnyElement {
    page.controls.borrow_mut().clear();
    let mut ctl = Ctl { page, next: 0 };
    let saved = crate::picker_view::settings(cx).plugins.clone();
    let mut out = div().flex().flex_col().gap_3().mt_1();
    for (n, source) in ps::plugins().into_iter().enumerate() {
        let id = source.id().to_owned();
        let rows = ps::rows(&*source, saved.get(&id));
        let mut card = div()
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(t.axis_line)
            .px_3()
            .py_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(t.text_main)
                            .child(source.label()),
                    )
                    .child(
                        div()
                            .text_size(crate::zoom::ui(11.0))
                            .text_color(t.text_dim)
                            .child("Level source"),
                    ),
            )
            .child(
                div()
                    .text_size(crate::zoom::ui(11.0))
                    .text_color(t.text_dim)
                    .child("Its settings apply to every chart it serves."),
            );
        let mut group = "";
        for (j, r) in rows.iter().enumerate() {
            if r.spec.group != group {
                group = r.spec.group;
                if !group.is_empty() {
                    card = card.child(
                        div()
                            .pt_2()
                            .text_size(crate::zoom::ui(11.0))
                            .text_color(t.text_dim)
                            .child(group.to_uppercase()),
                    );
                }
            }
            let key = r.spec.key;
            let cid = format!("plugin-{n}-{j}");
            let typing = page
                .edit
                .as_ref()
                .filter(|e| e.source == id && e.key == key);
            let control = match r.spec.kind {
                SettingKind::Bool => switch(cid.clone(), &id, r, &mut ctl, t, cx),
                SettingKind::Choice(_) => {
                    let a = Action::Open(id.clone(), key);
                    let focused = ctl.take(&a);
                    let open = page
                        .open
                        .as_ref()
                        .filter(|o| o.source == id && o.key == key);
                    let label = r.choice_label();
                    let trigger = crate::drop_view::trigger(
                        ("plugin-drop", n * 100 + j),
                        label.clone(),
                        focused,
                        open.is_some(),
                        false,
                        0,
                        t,
                    )
                    .on_mouse_down(MouseButton::Left, press(a, cx));
                    let list = open.map(|o| {
                        let current = r
                            .options
                            .iter()
                            .position(|(v, _)| v.eq_ignore_ascii_case(&r.value));
                        let el = crate::drop_view::list(
                            "plugin-drop-list",
                            &o.list,
                            &|k| Some(k) == current,
                            crate::layout_rec::POPUP,
                            t,
                            cx,
                            |v: &mut SettingsWindow, k, _, cx| v.plugin_pick(k, cx),
                        );
                        (el, o.scroll.clone(), crate::drop_view::cap(), t.panel)
                    });
                    crate::dropdown::dropdown_list(trigger, list)
                        .lint_trigger(&label)
                        .on_dismiss(Box::new(cx.listener(
                            |v: &mut SettingsWindow, _: &gpui::MouseDownEvent, _, cx| {
                                v.plugins.open = None;
                                cx.notify();
                            },
                        )))
                        .into_any_element()
                }
                SettingKind::Seconds { .. } | SettingKind::Number { .. } => {
                    let start = match (&r.refused, r.spec.kind) {
                        (Some(f), _) => f.text.clone(),
                        (None, SettingKind::Seconds { .. }) => r.shown.clone(),
                        (None, _) => r.value.clone(),
                    };
                    let text = typing
                        .map_or_else(|| r.text().to_owned(), |e| format!("{}\u{258f}", e.buf));
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(button(
                            format!("{cid}-less"),
                            "\u{2212}",
                            r.step(false).map(|v| Action::Set(id.clone(), key, v)),
                            &mut ctl,
                            t,
                            cx,
                        ))
                        .child(value_box(
                            format!("{cid}-value"),
                            text,
                            r.spec.placeholder,
                            false,
                            (typing.is_some(), r.refused.is_some()),
                            Action::Edit(id.clone(), key, start),
                            &mut ctl,
                            t,
                            cx,
                        ))
                        .child(button(
                            format!("{cid}-more"),
                            "+",
                            r.step(true).map(|v| Action::Set(id.clone(), key, v)),
                            &mut ctl,
                            t,
                            cx,
                        ))
                        .into_any_element()
                }
                SettingKind::Text | SettingKind::Url => {
                    let text = typing
                        .map_or_else(|| r.text().to_owned(), |e| format!("{}\u{258f}", e.buf));
                    value_box(
                        cid.clone(),
                        text,
                        r.spec.placeholder,
                        true,
                        (typing.is_some(), r.refused.is_some()),
                        Action::Edit(id.clone(), key, r.text().to_owned()),
                        &mut ctl,
                        t,
                        cx,
                    )
                }
                SettingKind::Secret => continue,
            };
            let from_env = r.origin == setting::Origin::Outside && r.refused.is_none();
            card = card.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .mt_1()
                    .child(
                        div()
                            .w(crate::zoom::ui(140.0))
                            .flex_none()
                            .text_color(t.text_main)
                            .child(r.spec.label),
                    )
                    .child(control)
                    .when(from_env, |d| {
                        d.child(
                            div()
                                .text_size(crate::zoom::ui(11.0))
                                .text_color(t.text_dim)
                                .child("from the environment"),
                        )
                    }),
            );
            // Under the control: why it was refused, else its help.
            let under = match r.refused_line() {
                Some(line) => div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(div().flex_none().text_color(t.destructive).child("!"))
                    .child(div().min_w_0().text_color(t.text_main).child(line)),
                None => div().text_color(t.text_dim).child(r.spec.help),
            };
            card = card.child(
                div()
                    .pl(crate::zoom::ui(140.0 + 8.0))
                    .text_size(crate::zoom::ui(11.0))
                    .child(under),
            );
            if page
                .asking
                .as_ref()
                .is_some_and(|(s, k)| *s == id && *k == key)
            {
                card = card.child(
                    div()
                        .ml(crate::zoom::ui(140.0 + 8.0))
                        .mt_1()
                        .p_2()
                        .rounded_sm()
                        .border_1()
                        .border_color(t.accent)
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_color(t.text_main).child(r.spec.confirm))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_2()
                                .child(button(
                                    format!("{cid}-cancel"),
                                    "Cancel",
                                    Some(Action::Answer(false)),
                                    &mut ctl,
                                    t,
                                    cx,
                                ))
                                .child(button(
                                    format!("{cid}-yes"),
                                    &format!("Turn on {}", r.spec.label.to_lowercase()),
                                    Some(Action::Answer(true)),
                                    &mut ctl,
                                    t,
                                    cx,
                                )),
                        ),
                );
            }
        }
        out = out.child(card);
    }
    out.into_any_element()
}
