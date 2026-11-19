//! Settings › Groups: account groups (decision 0006) and new paper accounts.
//!
//! The page lists the groups (name and member count) with "New group", and edits the one
//! chosen: its name, its members (a box per paper account, with its kind), Rename and Delete
//! group. "New paper account…" opens a short form under the members: a name, a starting
//! balance and prop rules (a preset; it sets the balance to its account size). Every change is
//! a trading command (`Command::CreateGroup`, `SetGroupMembers`, `AddPaperAccount`, …) sent as
//! a person's; the core answers, and the line under the editor says how it went. Nothing here
//! is saved by Settings: groups and accounts live in the trading file.
//!
//! Groups hold paper accounts only for now (the paper core keeps them; a broker venue's
//! accounts aren't listed here). Every control is reachable from the keyboard: ↑/↓ move between
//! them, Enter or Space press, typing goes to a field being typed (Enter applies, Esc goes
//! back).

use std::cell::{Cell, RefCell};

use gpui::{
    AnyElement, App, Context, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, SharedString, Styled, div, prelude::FluentBuilder,
};
use lines_trading::{AccountId, Command as TradeCommand, CommandId, RulePreset, Source};

use crate::drop_list::{DropList, Entry, Outcome};
use crate::settings_view::SettingsWindow;
use crate::theme::Theme;

/// The longest name typed here (the core's limit).
const MAX_NAME: usize = lines_trading::group::MAX_NAME_CHARS;
/// A new paper account's starting balance until one is typed (whole dollars).
pub const DEFAULT_BALANCE: i64 = 50_000;

/// A field being typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
    /// A new group's name.
    NewGroup,
    /// The chosen group's new name.
    Rename,
    /// The new paper account's name.
    AccountName,
    /// The new paper account's starting balance.
    Balance,
}

/// The new paper account's form, while open.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NewAccount {
    pub name: String,
    /// Whole dollars, as typed (digits; separators are ignored).
    pub balance: String,
    /// Prop rules: an index into `rules::presets()` (`None`: none).
    pub preset: Option<usize>,
}

impl Default for NewAccount {
    fn default() -> NewAccount {
        NewAccount {
            name: String::new(),
            balance: crate::settings_schema::grouped(DEFAULT_BALANCE),
            preset: None,
        }
    }
}

impl NewAccount {
    /// The command it makes, or why it can't be sent yet.
    pub fn command(&self) -> Result<TradeCommand, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Name the account".into());
        }
        let rules = self.preset.and_then(|k| presets().into_iter().nth(k));
        let starting_balance = match &rules {
            Some(r) => r.account_size,
            None => parse_balance(&self.balance)
                .ok_or_else(|| "A starting balance is a whole number of dollars".to_owned())?,
        };
        Ok(TradeCommand::AddPaperAccount {
            name: name.to_owned(),
            starting_balance,
            rules,
        })
    }
}

/// Whole dollars as typed: digits, with `,`, `_`, spaces and a leading `$` ignored.
pub fn parse_balance(s: &str) -> Option<i64> {
    let digits: String = s
        .trim()
        .trim_start_matches('$')
        .chars()
        .filter(|c| !matches!(c, ',' | '_' | ' '))
        .collect();
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn presets() -> Vec<RulePreset> {
    lines_trading::rules::presets()
}

/// What a command sent from here was, to say how it went.
#[derive(Clone, Debug, PartialEq)]
enum Sent {
    Created(String),
    Renamed(String),
    Members(String),
    Deleted(String),
    Account(String),
}

/// What the page holds between frames.
#[derive(Default)]
pub(crate) struct GroupPage {
    /// The group being edited (by name; the first when none or gone).
    pub(crate) selected: Option<String>,
    pub(crate) typing: Option<(Field, String)>,
    pub(crate) form: Option<NewAccount>,
    /// The prop rules' list, open.
    pub(crate) preset_open: Option<(DropList, gpui::ScrollHandle)>,
    /// Commands sent from here, waiting for their answers.
    waiting: Vec<(CommandId, Sent)>,
    /// The last answer: its words, and whether it was a refusal.
    pub(crate) msg: Option<(String, bool)>,
    /// The keyboard's control, an index into `controls`.
    focus: Cell<usize>,
    /// The page's controls in drawing order, as last drawn (what ↑/↓ and Enter act on).
    controls: RefCell<Vec<Action>>,
}

/// What a press on the page asks for (`SettingsWindow::groups_action` does it).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Action {
    Select(String),
    /// Start typing a new group's name.
    NewGroup,
    /// Start typing the chosen group's new name.
    Rename,
    /// Add the account to the chosen group, or take it out.
    Toggle(AccountId),
    Delete,
    /// Open the new paper account's form.
    NewAccount,
    /// Type one of the form's fields.
    Type(Field),
    /// Open the prop rules' list.
    Presets,
    Create,
    /// Close the form (or a field being typed).
    Cancel,
    /// A control that can't act now: keeps its place in the keyboard's order.
    Nothing,
}

/// Where a palette command opens the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    NewGroup,
    NewAccount,
}

impl GroupPage {
    /// Leaving the page: what was being typed and an open list are dropped (no key lands in
    /// them unseen); a form keeps what was typed.
    pub(crate) fn leave(&mut self) {
        self.typing = None;
        self.preset_open = None;
        self.focus.set(0);
    }

    /// Opened from the palette.
    pub(crate) fn start(&mut self, s: Start) {
        self.preset_open = None;
        self.msg = None;
        match s {
            Start::NewGroup => self.typing = Some((Field::NewGroup, String::new())),
            Start::NewAccount => {
                // A form already open keeps what was typed in it.
                let f = self.form.get_or_insert_with(NewAccount::default);
                self.typing = Some((Field::AccountName, f.name.clone()));
            }
        }
    }

    /// Whether a field is being typed or a list is open (they take every key).
    pub(crate) fn busy(&self) -> bool {
        self.typing.is_some() || self.preset_open.is_some()
    }

    /// The group being edited, as the core has it now.
    pub(crate) fn chosen<'a>(
        &self,
        groups: &'a [lines_trading::group::AccountGroup],
    ) -> Option<&'a lines_trading::group::AccountGroup> {
        self.selected
            .as_deref()
            .and_then(|n| {
                groups
                    .iter()
                    .find(|g| lines_trading::group::same_name(&g.name, n))
            })
            .or_else(|| groups.first())
    }

    fn send(&mut self, cmd: TradeCommand, sent: Sent, cx: &App) {
        let Some(t) = crate::trading::Trading::try_global(cx) else {
            self.msg = Some(("Trading is not running".into(), true));
            return;
        };
        let id = t.submit(Source::Palette, std::time::Instant::now(), cmd);
        self.waiting.push((id, sent));
    }

    /// The core's answers to what was sent from here. `true`: something changed (redraw).
    pub(crate) fn check(&mut self, cx: &App) -> bool {
        if self.waiting.is_empty() {
            return false;
        }
        let Some(cache) = crate::trading::Trading::try_global(cx).and_then(|t| t.cache.try_read())
        else {
            return false;
        };
        let mut changed = false;
        let mut left = Vec::new();
        for (id, sent) in std::mem::take(&mut self.waiting) {
            let Some(result) = cache.result(id) else {
                left.push((id, sent));
                continue;
            };
            changed = true;
            self.msg = Some(match result {
                Err(r) => (capitalized(&r.message), true),
                Ok(_) => {
                    let text = match &sent {
                        Sent::Created(n) => {
                            self.selected = Some(n.clone());
                            format!("Created {n}")
                        }
                        Sent::Renamed(n) => {
                            self.selected = Some(n.clone());
                            format!("Renamed to {n}")
                        }
                        Sent::Members(n) => format!("{n}: members saved"),
                        Sent::Deleted(n) => {
                            self.selected = None;
                            format!("Deleted {n}. Its accounts are untouched")
                        }
                        Sent::Account(n) => {
                            self.form = None;
                            format!("Added the paper account {n}")
                        }
                    };
                    (text, false)
                }
            });
        }
        self.waiting = left;
        changed
    }

    /// A press (`groups_action`).
    pub(crate) fn press(&mut self, a: Action, cx: &mut App) {
        if a != Action::Presets {
            self.preset_open = None;
        }
        let groups: Vec<lines_trading::group::AccountGroup> =
            crate::trading::Trading::try_global(cx)
                .map(|t| t.cache.read().groups().to_vec())
                .unwrap_or_default();
        let chosen = self.chosen(&groups).cloned();
        // A click on the field being typed keeps typing (it never starts it over).
        let typed = |f: Field| matches!(&self.typing, Some((x, _)) if *x == f);
        if (a == Action::NewGroup && typed(Field::NewGroup))
            || (a == Action::Rename && typed(Field::Rename))
            || matches!(a, Action::Type(f) if typed(f))
        {
            return;
        }
        // Another press ends typing: what was typed is applied, never dropped unseen.
        if !matches!(a, Action::Cancel | Action::Nothing) {
            self.commit_typing(cx);
        }
        match a {
            Action::Nothing => {}
            Action::Select(name) => {
                self.typing = None;
                self.selected = Some(name);
            }
            Action::NewGroup => {
                self.msg = None;
                self.typing = Some((Field::NewGroup, String::new()));
            }
            Action::Rename => {
                if let Some(g) = chosen {
                    self.msg = None;
                    self.typing = Some((Field::Rename, g.name));
                }
            }
            Action::Toggle(account) => {
                let Some(g) = chosen else { return };
                self.typing = None;
                let members = toggled(&g.members, &account);
                self.send(
                    TradeCommand::SetGroupMembers {
                        group: g.name.clone(),
                        members,
                    },
                    Sent::Members(g.name),
                    cx,
                );
            }
            Action::Delete => {
                let Some(g) = chosen else { return };
                self.typing = None;
                self.send(
                    TradeCommand::DeleteGroup {
                        group: g.name.clone(),
                    },
                    Sent::Deleted(g.name),
                    cx,
                );
            }
            Action::NewAccount => {
                self.msg = None;
                let f = self.form.get_or_insert_with(NewAccount::default);
                self.typing = Some((Field::AccountName, f.name.clone()));
            }
            Action::Type(field) => {
                if let Some(f) = &self.form {
                    let buf = match field {
                        Field::AccountName => f.name.clone(),
                        Field::Balance if f.preset.is_some() => return,
                        Field::Balance => f.balance.clone(),
                        _ => return,
                    };
                    self.typing = Some((field, buf));
                }
            }
            Action::Presets => {
                let Some(f) = &self.form else { return };
                let mut entries = vec![Entry::new("None")];
                entries.extend(presets().into_iter().map(|p| Entry::new(p.name)));
                let current = Some(f.preset.map_or(0, |k| k + 1));
                let list = DropList::new(entries, false, false, current);
                let scroll = gpui::ScrollHandle::new();
                crate::drop_view::reveal(&scroll, &list, true);
                self.preset_open = Some((list, scroll));
            }
            Action::Create => {
                // One at a time: a second Create while the first waits would answer "already
                // an account" over the first's success.
                if self
                    .waiting
                    .iter()
                    .any(|(_, s)| matches!(s, Sent::Account(_)))
                {
                    return;
                }
                let Some(f) = &self.form else { return };
                match f.command() {
                    Ok(cmd) => {
                        let name = f.name.trim().to_owned();
                        self.send(cmd, Sent::Account(name), cx);
                    }
                    Err(why) => self.msg = Some((why, true)),
                }
            }
            Action::Cancel => {
                if self.typing.take().is_none() {
                    self.form = None;
                }
            }
        }
    }

    /// A row of the open prop rules' list picked (`0`: none).
    pub(crate) fn pick_preset(&mut self, k: usize) {
        self.preset_open = None;
        if let Some(f) = self.form.as_mut() {
            f.preset = k.checked_sub(1);
            if let Some(p) = f.preset.and_then(|k| presets().into_iter().nth(k)) {
                f.balance = crate::settings_schema::grouped(p.account_size);
            }
        }
    }

    /// What is being typed, applied: a name or rename sent; a form field kept in the form.
    fn commit_typing(&mut self, cx: &App) {
        let Some((field, buf)) = self.typing.take() else {
            return;
        };
        match field {
            Field::NewGroup => {
                let name = buf.trim().to_owned();
                if !name.is_empty() {
                    self.send(
                        TradeCommand::CreateGroup {
                            name: name.clone(),
                            members: Vec::new(),
                        },
                        Sent::Created(name),
                        cx,
                    );
                }
            }
            Field::Rename => {
                let groups: Vec<lines_trading::group::AccountGroup> =
                    crate::trading::Trading::try_global(cx)
                        .map(|t| t.cache.read().groups().to_vec())
                        .unwrap_or_default();
                let name = buf.trim().to_owned();
                if let Some(g) = self.chosen(&groups)
                    && !name.is_empty()
                    && name != g.name
                {
                    let group = g.name.clone();
                    self.send(
                        TradeCommand::RenameGroup {
                            group,
                            name: name.clone(),
                        },
                        Sent::Renamed(name),
                        cx,
                    );
                }
            }
            Field::AccountName => {
                if let Some(f) = self.form.as_mut() {
                    f.name = buf;
                }
            }
            Field::Balance => {
                if let Some(f) = self.form.as_mut() {
                    f.balance = buf;
                }
            }
        }
    }

    /// A key on the page. `true`: handled (redraw).
    pub(crate) fn key(&mut self, ev: &KeyDownEvent, cx: &mut App) -> bool {
        let k = &ev.keystroke;
        let m = k.modifiers;
        if let Some((list, _)) = self.preset_open.as_mut() {
            match list.key(&k.key, k.key_char.as_deref(), std::time::Instant::now()) {
                Outcome::Pick(i) => self.pick_preset(i),
                Outcome::Close => self.preset_open = None,
                _ => {}
            }
            return true;
        }
        if let Some((field, buf)) = self.typing.as_mut() {
            let field = *field;
            match k.key.as_str() {
                "enter" => {
                    self.commit_typing(cx);
                    // Enter on the form's last field creates the account.
                    if field == Field::Balance {
                        self.press(Action::Create, cx);
                    }
                }
                "tab" if matches!(field, Field::AccountName) => {
                    self.commit_typing(cx);
                    self.press(Action::Type(Field::Balance), cx);
                }
                "escape" => self.typing = None,
                "backspace" => {
                    buf.pop();
                }
                "v" if (m.control || m.platform) && !m.alt => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        push_text(buf, field, text.trim());
                    }
                }
                _ => {
                    if let Some(c) = k.key_char.as_deref().filter(|_| !m.control && !m.alt) {
                        push_text(buf, field, c);
                    }
                }
            }
            return true;
        }
        if k.key == "escape" && self.form.is_some() {
            self.form = None;
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

/// `--settings-set groups:…` (screenshots): `select=NAME` (the group edited), `new-group`
/// (typing a new group's name), `new-account=NAME` (the form open, NAME in it), `preset=K`
/// (the form's prop rules: preset K, from 1), `presets=open` (their list open).
#[cfg(feature = "screenshot")]
pub(crate) fn check_shot(s: &str) -> anyhow::Result<()> {
    let ok = s == "new-group"
        || s == "presets=open"
        || s.starts_with("select=")
        || s.starts_with("new-account=")
        || s.strip_prefix("preset=")
            .and_then(|k| k.parse::<usize>().ok())
            .is_some_and(|k| (1..=presets().len()).contains(&k));
    if !ok {
        anyhow::bail!(
            "--settings-set groups:{s:?}: select=NAME, new-group, new-account=NAME, preset=K (1..={}) or presets=open",
            presets().len()
        );
    }
    Ok(())
}

#[cfg(feature = "screenshot")]
impl GroupPage {
    /// One `--settings-set groups:…` (checked by [`check_shot`]).
    pub(crate) fn shot(&mut self, s: &str) {
        if let Some(n) = s.strip_prefix("select=") {
            self.selected = Some(n.replace('_', " "));
        } else if s == "new-group" {
            self.typing = Some((Field::NewGroup, String::new()));
        } else if let Some(n) = s.strip_prefix("new-account=") {
            self.form = Some(NewAccount {
                name: n.replace('_', " "),
                ..NewAccount::default()
            });
        } else if let Some(k) = s.strip_prefix("preset=").and_then(|k| k.parse().ok()) {
            self.form.get_or_insert_with(NewAccount::default);
            self.pick_preset(k);
        } else if s == "presets=open" {
            let f = self.form.get_or_insert_with(NewAccount::default).clone();
            let mut entries = vec![Entry::new("None")];
            entries.extend(presets().into_iter().map(|p| Entry::new(p.name)));
            let list = DropList::new(entries, false, false, Some(f.preset.map_or(0, |k| k + 1)));
            self.preset_open = Some((list, gpui::ScrollHandle::new()));
        }
    }
}

/// Typed text into a field: no control characters, a name at most [`MAX_NAME`] characters, a
/// balance its digits and separators only.
fn push_text(buf: &mut String, field: Field, text: &str) {
    for c in text.chars().filter(|c| !c.is_control()) {
        match field {
            Field::Balance if !(c.is_ascii_digit() || c == ',') => continue,
            Field::Balance if buf.len() >= 12 => break,
            _ if buf.chars().count() >= MAX_NAME => break,
            _ => {}
        }
        buf.push(c);
    }
}

/// The members with `account` added at the end, or taken out.
pub fn toggled(members: &[AccountId], account: &AccountId) -> Vec<AccountId> {
    if members.contains(account) {
        members.iter().filter(|m| *m != account).cloned().collect()
    } else {
        let mut v = members.to_vec();
        v.push(account.clone());
        v
    }
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// Registers the page's controls in drawing order (for the keyboard) and says which one has
/// the keyboard.
struct Ctl<'a> {
    page: &'a GroupPage,
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
            v.groups_action(a.clone(), cx)
        },
    )
}

/// A small bordered control.
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

/// A field's box: its text (a caret while typed), the placeholder dimmed while empty.
#[allow(clippy::too_many_arguments)]
fn field_box(
    id: String,
    text: &str,
    placeholder: &str,
    typing: bool,
    locked: bool,
    action: Action,
    ctl: &mut Ctl<'_>,
    t: &Theme,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let focused = ctl.take(&action);
    let empty = text.is_empty() && !typing;
    let shown = if typing {
        format!("{text}\u{258f}")
    } else if empty {
        placeholder.to_owned()
    } else {
        text.to_owned()
    };
    div()
        .id(SharedString::from(id))
        .flex_none()
        .min_w(crate::zoom::ui(200.0))
        .px_2()
        .rounded_sm()
        .border_1()
        .when(locked, |d| d.border_dashed())
        .border_color(if typing || focused {
            t.accent
        } else {
            t.axis_line
        })
        .whitespace_nowrap()
        .text_color(if empty || locked {
            t.text_dim
        } else {
            t.text_main
        })
        .when(!locked, |d| {
            d.cursor_text()
                .on_mouse_down(MouseButton::Left, press(action, cx))
        })
        .child(shown)
        .into_any_element()
}

/// A kind chip (PAPER, SIM, LIVE): the badge's words in a box, never color alone.
fn chip(text: &'static str, t: &Theme) -> AnyElement {
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(t.axis_line)
        .text_size(crate::zoom::ui(10.5))
        .text_color(t.text_dim)
        .child(text)
        .into_any_element()
}

fn small(text: impl Into<SharedString>, t: &Theme) -> gpui::Div {
    div()
        .text_size(crate::zoom::ui(11.0))
        .text_color(t.text_dim)
        .child(text.into())
}

/// The page.
pub(crate) fn list(page: &GroupPage, t: &Theme, cx: &mut Context<SettingsWindow>) -> AnyElement {
    page.controls.borrow_mut().clear();
    let mut ctl = Ctl { page, next: 0 };
    let Some(trading) = crate::trading::Trading::try_global(cx) else {
        return small("Trading is not running", t).into_any_element();
    };
    let accounts: Vec<crate::trading::AccountInfo> = trading
        .accounts()
        .into_iter()
        .filter(|a| a.venue.is_none())
        .collect();
    let groups: Vec<lines_trading::group::AccountGroup> = trading
        .cache
        .try_read()
        .map(|c| c.groups().to_vec())
        .unwrap_or_default();
    let chosen = page.chosen(&groups).cloned();
    let typing = |f: Field| {
        page.typing
            .as_ref()
            .filter(|(x, _)| *x == f)
            .map(|(_, b)| b)
    };

    // The groups: name and member count; the one edited outlined.
    let mut side = div()
        .flex()
        .flex_col()
        .flex_none()
        .w(crate::zoom::ui(180.0))
        .gap_1();
    for (n, g) in groups.iter().enumerate() {
        let a = Action::Select(g.name.clone());
        let focused = ctl.take(&a);
        let on = chosen.as_ref().is_some_and(|c| c.name == g.name);
        side = side.child(
            div()
                .id(SharedString::from(format!("group-{n}")))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .px_2()
                .rounded_sm()
                .border_1()
                .border_color(if focused {
                    t.accent
                } else if on {
                    t.control_edge
                } else {
                    gpui::transparent_black()
                })
                .when(on, |d| d.bg(t.ghost_selected))
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, press(a, cx))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(t.text_main)
                        .child(g.name.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(t.text_dim)
                        .child(g.members.len().to_string()),
                ),
        );
    }
    side = side.child(match typing(Field::NewGroup) {
        Some(buf) => field_box(
            "group-new-name".into(),
            buf,
            "Group name",
            true,
            false,
            Action::NewGroup,
            &mut ctl,
            t,
            cx,
        ),
        None => button(
            "group-new".into(),
            "+ New group",
            Some(Action::NewGroup),
            &mut ctl,
            t,
            cx,
        ),
    });

    // The chosen group's editor.
    let mut editor = div().flex().flex_col().flex_1().min_w_0().gap_1();
    if let Some(g) = &chosen {
        let name = match typing(Field::Rename) {
            Some(buf) => field_box(
                "group-rename".into(),
                buf,
                "Group name",
                true,
                false,
                Action::Rename,
                &mut ctl,
                t,
                cx,
            ),
            None => div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(t.text_main)
                .child(g.name.clone())
                .into_any_element(),
        };
        editor = editor.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(crate::zoom::ui(80.0))
                        .flex_none()
                        .text_color(t.text_dim)
                        .child("Name"),
                )
                .child(name),
        );
        editor = editor.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .pt_1()
                .text_size(crate::zoom::ui(11.0))
                .text_color(t.text_dim)
                .child(div().flex_1().child("ACCOUNT"))
                .child(div().flex_none().child("KIND")),
        );
        for (n, a) in accounts.iter().enumerate() {
            let on = g.members.contains(&a.id);
            let act = Action::Toggle(a.id.clone());
            let focused = ctl.take(&act);
            editor = editor.child(
                div()
                    .id(SharedString::from(format!("member-{n}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(if focused {
                        t.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, press(act, cx))
                    .child(
                        div()
                            .flex_none()
                            .size(crate::zoom::ui(13.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .border_1()
                            .border_color(t.control_edge)
                            .rounded_sm()
                            .text_size(crate::zoom::ui(11.0))
                            .text_color(t.text_main)
                            .child(if on { "✓" } else { "" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(t.text_main)
                            .child(a.name.clone()),
                    )
                    .child(chip(a.badge, t)),
            );
        }
    }
    // The new paper account: a button, or its form.
    match &page.form {
        None => {
            editor = editor.child(div().pt_1().flex().flex_row().child(button(
                "account-new".into(),
                "+ New paper account…",
                Some(Action::NewAccount),
                &mut ctl,
                t,
                cx,
            )))
        }
        Some(f) => {
            let row = |label: &'static str, el: AnyElement| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(crate::zoom::ui(110.0))
                            .flex_none()
                            .text_color(t.text_main)
                            .child(label),
                    )
                    .child(el)
            };
            let name_el = field_box(
                "account-name".into(),
                typing(Field::AccountName).unwrap_or(&f.name),
                "Account name",
                typing(Field::AccountName).is_some(),
                false,
                Action::Type(Field::AccountName),
                &mut ctl,
                t,
                cx,
            );
            let balance_el = field_box(
                "account-balance".into(),
                &format!("${}", typing(Field::Balance).unwrap_or(&f.balance)),
                "",
                typing(Field::Balance).is_some(),
                f.preset.is_some(),
                Action::Type(Field::Balance),
                &mut ctl,
                t,
                cx,
            );
            let all = presets();
            let label = f
                .preset
                .and_then(|k| all.get(k))
                .map_or_else(|| "None".to_owned(), |p| p.name.clone());
            let a = Action::Presets;
            let focused = ctl.take(&a);
            let trigger = crate::drop_view::trigger(
                ("account-preset", 0),
                label.clone(),
                focused,
                page.preset_open.is_some(),
                false,
                0,
                t,
            )
            .on_mouse_down(MouseButton::Left, press(a, cx));
            let current = f.preset.map_or(0, |k| k + 1);
            let open = page.preset_open.as_ref().map(|(l, scroll)| {
                let el = crate::drop_view::list(
                    "account-preset-list",
                    l,
                    &|k| k == current,
                    crate::layout_rec::POPUP,
                    t,
                    cx,
                    |v: &mut SettingsWindow, k, _, cx| v.groups_pick_preset(k, cx),
                );
                (el, scroll.clone(), crate::drop_view::cap(), t.panel)
            });
            let preset_el = crate::dropdown::dropdown_list(trigger, open)
                .lint_trigger(&label)
                .on_dismiss(Box::new(cx.listener(
                    |v: &mut SettingsWindow, _: &gpui::MouseDownEvent, _, cx| {
                        v.groups.preset_open = None;
                        cx.notify();
                    },
                )))
                .into_any_element();
            let mut form = div()
                .mt_2()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(t.axis_line)
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(t.text_main)
                        .child("New paper account"),
                )
                .child(row("Name", name_el))
                .child(row("Starting balance", balance_el))
                .child(row("Prop rules", preset_el));
            if f.preset.is_some() {
                form = form.child(
                    small(lines_trading::rules::APPROXIMATION, t).pl(crate::zoom::ui(118.0)),
                );
            }
            form = form.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .pt_1()
                    .child(button(
                        "account-create".into(),
                        "Create",
                        Some(Action::Create),
                        &mut ctl,
                        t,
                        cx,
                    ))
                    .child(button(
                        "account-cancel".into(),
                        "Cancel",
                        Some(Action::Cancel),
                        &mut ctl,
                        t,
                        cx,
                    )),
            );
            editor = editor.child(form);
        }
    }
    if chosen.is_some() {
        editor = editor.child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .pt_2()
                .child(button(
                    "group-rename-btn".into(),
                    "Rename",
                    Some(Action::Rename),
                    &mut ctl,
                    t,
                    cx,
                ))
                .child(button(
                    "group-delete".into(),
                    "Delete group",
                    Some(Action::Delete),
                    &mut ctl,
                    t,
                    cx,
                )),
        );
    }
    if let Some((text, refused)) = &page.msg {
        editor = editor.child(
            div()
                .pt_1()
                .flex()
                .flex_row()
                .gap_1()
                .text_size(crate::zoom::ui(11.0))
                .when(*refused, |d| {
                    d.child(div().flex_none().text_color(t.destructive).child("!"))
                })
                .child(
                    div()
                        .min_w_0()
                        .text_color(if *refused { t.text_main } else { t.text_dim })
                        .child(text.clone()),
                ),
        );
    }
    div()
        .flex()
        .flex_row()
        .gap_4()
        .mt_1()
        .child(side)
        .child(editor)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balances_parse_as_whole_dollars() {
        assert_eq!(parse_balance("50,000"), Some(50_000));
        assert_eq!(parse_balance(" $100 000 "), Some(100_000));
        assert_eq!(parse_balance(""), None);
        assert_eq!(parse_balance("1.5"), None);
        assert_eq!(parse_balance("-5"), None);
    }

    #[test]
    fn the_form_makes_one_command() {
        let mut f = NewAccount::default();
        assert!(f.command().is_err(), "a name is needed");
        f.name = "  Eval 2 ".into();
        assert_eq!(
            f.command(),
            Ok(TradeCommand::AddPaperAccount {
                name: "Eval 2".into(),
                starting_balance: DEFAULT_BALANCE,
                rules: None,
            })
        );
        // A preset decides the balance (its account size), whatever was typed.
        f.balance = "7".into();
        f.preset = Some(2);
        let p = presets()[2].clone();
        assert_eq!(
            f.command(),
            Ok(TradeCommand::AddPaperAccount {
                name: "Eval 2".into(),
                starting_balance: p.account_size,
                rules: Some(p),
            })
        );
        f.preset = None;
        f.balance = "x".into();
        assert!(f.command().is_err());
    }

    #[test]
    fn a_toggle_adds_at_the_end_or_takes_out() {
        let (a, b) = (AccountId::new("a"), AccountId::new("b"));
        assert_eq!(
            toggled(std::slice::from_ref(&a), &b),
            vec![a.clone(), b.clone()]
        );
        assert_eq!(toggled(&[a.clone(), b.clone()], &a), vec![b]);
    }

    #[test]
    fn typed_text_is_bounded() {
        let mut s = String::new();
        push_text(&mut s, Field::Balance, "1a2,3\n");
        assert_eq!(s, "12,3");
        let mut s = String::new();
        push_text(&mut s, Field::NewGroup, &"x".repeat(MAX_NAME + 5));
        assert_eq!(s.chars().count(), MAX_NAME);
    }
}
