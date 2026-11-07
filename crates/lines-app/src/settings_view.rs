//! The Settings window (decision 0021, roadmap 6c): Ctrl+, opens one window with a search
//! box and the sections down the left. Every row comes from `crate::settings_schema`: no
//! setting has its own form. Changes apply at once (no OK button); Ctrl+Z undoes and ↺
//! resets one setting. Settings that affect trading are marked; loosening a guardrail asks
//! first and takes effect at the next trade date (the trading core's rule), tightening
//! applies now.
//!
//! Keys: type to search (Esc clears it, then closes), ↑↓ settings, ←→ change a choice,
//! Space or Enter toggles, Enter edits a number (Enter keeps it, Esc drops it), Tab and
//! Shift+Tab move between sections, Alt+A and Alt+P step the account and the product,
//! Ctrl+R resets, Ctrl+Z undoes.

use std::time::Instant;

use gpui::{
    AnyElement, App, AppContext, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Render, StatefulInteractiveElement, Styled,
    TitlebarOptions, Window, WindowHandle, WindowOptions, div, prelude::FluentBuilder, px,
};

use crate::brackets::{self, EditorKey};
use crate::picker_view::{settings, update_settings};
use crate::settings_schema::{
    self as schema, AccountFact, Facts, Kind, Prefs, Safety, Scope, Section, Target, Value,
    WorkspacePrefs,
};
use crate::theme::Theme;
use crate::workspace::Workspace;

/// The open Settings window, if any (there is only ever one).
#[derive(Default)]
pub struct SettingsWindowGlobal(pub Option<WindowHandle<SettingsWindow>>);
impl gpui::Global for SettingsWindowGlobal {}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Tone {
    Done,
    Problem,
}

/// A setting change waiting for the trader's yes (a guardrail loosened).
struct Pending {
    index: usize,
    value: Value,
    /// The account (and product) it was asked for: confirmed for that one only.
    target: Target,
    text: String,
}

pub struct SettingsWindow {
    focus: FocusHandle,
    theme: Theme,
    section: Section,
    query: String,
    /// The selected row among those shown.
    row: usize,
    /// Text typed into the selected number setting.
    edit: Option<String>,
    note: Option<(usize, String, Tone)>,
    pending: Option<Pending>,
    target: Target,
    facts: Facts,
    products: Vec<&'static str>,
    /// Earlier values, newest last: (setting, its target, the value before).
    undo: Vec<(usize, Target, Value)>,
    editor: Option<brackets::Editor>,
    editor_msg: Option<String>,
    /// The settings list's scroll (keeps the selected row in view).
    scroll: gpui::ScrollHandle,
    /// The row last scrolled into view (only a new selection scrolls).
    scrolled: Option<(Section, usize, bool)>,
}

/// The window's size (logical px).
pub const SIZE: (f32, f32) = (860.0, 620.0);

#[cfg(feature = "screenshot")]
fn section_named(name: &str) -> Option<Section> {
    Section::ALL
        .into_iter()
        .find(|s| s.name().eq_ignore_ascii_case(name))
}

/// `KEY=VALUE` for `--settings-set`: the setting and its value.
#[cfg(feature = "screenshot")]
fn parse_set(s: &str) -> Result<(usize, Value), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("--settings-set {s:?}: expected KEY=VALUE"))?;
    let i = schema::all()
        .iter()
        .position(|st| st.key == k)
        .ok_or_else(|| format!("--settings-set: no setting {k:?}"))?;
    let value = match (schema::all()[i].kind, v) {
        (Kind::Toggle, "on") => Value::Bool(true),
        (Kind::Toggle, "off") => Value::Bool(false),
        (Kind::Choice(_), n) => Value::Choice(
            n.parse()
                .map_err(|_| format!("{s:?}: a choice's position"))?,
        ),
        (Kind::Number { .. }, n) => {
            Value::Number(schema::parse_number(n).map_err(|e| format!("{s:?}: {e}"))?)
        }
        _ => return Err(format!("--settings-set {s:?}: not settable this way")),
    };
    Ok((i, value))
}

/// Screenshot flags, checked before anything runs.
#[cfg(feature = "screenshot")]
pub fn check_shot(section: &str, set: &[String]) -> anyhow::Result<()> {
    if section_named(section).is_none() {
        anyhow::bail!(
            "--settings {section:?}: one of {}",
            Section::ALL.map(|s| s.name().to_lowercase()).join(", ")
        );
    }
    for s in set {
        parse_set(s).map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

/// Screenshots: open the window (hidden) at `section`, with a search and changes made.
#[cfg(feature = "screenshot")]
pub fn open_for_screenshot(
    section: &str,
    search: Option<&str>,
    set: &[String],
    cx: &mut App,
) -> Option<gpui::AnyWindowHandle> {
    open(None, cx);
    let h = cx.try_global::<SettingsWindowGlobal>().and_then(|g| g.0)?;
    let section = section_named(section)?;
    let changes: Vec<(usize, Value)> = set.iter().filter_map(|s| parse_set(s).ok()).collect();
    let search = search.map(str::to_owned);
    let _ = h.update(cx, |v, _, cx| {
        v.section = section;
        for (i, value) in changes {
            v.select(i);
            v.set(i, value, false, cx);
        }
        if let Some(q) = search {
            v.query = q;
            v.row = 0;
        }
        cx.notify();
    });
    Some(h.into())
}

/// Open the Settings window, or bring the open one forward; at setting `at` if given.
pub fn open(at: Option<usize>, cx: &mut App) {
    if let Some(h) = cx.try_global::<SettingsWindowGlobal>().and_then(|g| g.0) {
        let ok = h
            .update(cx, |v, window, cx| {
                if let Some(i) = at {
                    v.show(i);
                }
                window.activate_window();
                v.focus.focus(window, cx);
                cx.notify();
            })
            .is_ok();
        if ok {
            return;
        }
    }
    let Some(ws) = cx.try_global::<Workspace>() else {
        return;
    };
    let theme = ws.env.theme.get();
    let options = if ws.env.track_visibility {
        WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Lines · Settings".into()),
                ..Default::default()
            }),
            window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds::centered(
                None,
                gpui::size(px(SIZE.0), px(SIZE.1)),
                cx,
            ))),
            icon: crate::window_icon(),
            ..Default::default()
        }
    } else {
        crate::workspace::hidden_window("Lines (settings)", SIZE)
    };
    let facts = gather_facts(cx);
    let Ok(h) = cx.open_window(options, move |window, cx| {
        cx.new(|cx| {
            let mut v = SettingsWindow::new(theme, facts, cx);
            if let Some(i) = at {
                v.show(i);
            }
            v.focus.focus(window, cx);
            v
        })
    }) else {
        return;
    };
    cx.set_global(SettingsWindowGlobal(Some(h)));
    let _ = h.update(cx, |_, window, cx| {
        window.on_window_should_close(cx, |_, cx| {
            cx.set_global(SettingsWindowGlobal(None));
            true
        });
    });
}

/// Run a schema palette command: set its value, or open the window at it.
pub fn run(c: schema::SettingCmd, cx: &mut App) -> Result<Option<String>, &'static str> {
    let Some(st) = c.setting() else {
        return Err("no such setting");
    };
    match c.value() {
        Some(v) => {
            let facts = gather_facts(cx);
            let target = default_target(&facts);
            let before = current_prefs(cx);
            let old = (st.get)(&before, &target, &facts);
            let mut after = before.clone();
            st.apply(&mut after, &target, v.clone())?;
            if schema::loosening(st, &old, &v, &Default::default(), &Default::default()) {
                // A loosening needs a yes: ask in the window.
                let i = c.index as usize;
                open(Some(i), cx);
                if let Some(h) = cx.try_global::<SettingsWindowGlobal>().and_then(|g| g.0) {
                    let _ = h.update(cx, |w, _, cx| w.set(i, v, false, cx));
                }
                return Ok(None);
            }
            commit(&before, &after, &target, cx);
            Ok(Some(format!("{}: {}", st.name, st.value_text(&v))))
        }
        None => {
            open(Some(c.index as usize), cx);
            Ok(None)
        }
    }
}

/// The window's view of every value, from the live app and workspace.
fn current_prefs(cx: &App) -> Prefs {
    let ws = cx.try_global::<Workspace>();
    Prefs {
        app: settings(cx).clone(),
        ws: WorkspacePrefs {
            hide_hints: ws.is_some_and(|w| w.hide_hints),
            latency_hud: ws.is_some_and(|w| w.show_latency_hud),
        },
    }
}

/// Write `after` where it lives and apply its effects (the look, the trading core's
/// guardrails, P&L text, the workspace's preferences).
fn commit(before: &Prefs, after: &Prefs, target: &Target, cx: &mut App) {
    if after.app != before.app {
        let app = after.app.clone();
        update_settings(cx, move |s| *s = app);
    }
    if after.app.appearance != before.app.appearance {
        // After the current update: the window that asked (the Settings window, or a main
        // window running a palette command) can't be updated while it is being updated.
        let t = schema::theme_for(&after.app.appearance);
        cx.defer(move |cx| {
            crate::workspace::set_theme(t, cx);
            if let Some(h) = cx.try_global::<SettingsWindowGlobal>().and_then(|g| g.0) {
                let _ = h.update(cx, |v, _, cx| {
                    v.theme = t;
                    cx.notify();
                });
            }
        });
    }
    if after.app.pnl_display != before.app.pnl_display {
        crate::pnl::display_changed();
        cx.refresh_windows();
    }
    if after.ws.hide_hints != before.ws.hide_hints {
        crate::workspace::set_hide_hints(after.ws.hide_hints, cx);
    }
    if after.ws.latency_hud != before.ws.latency_hud {
        crate::workspace::set_show_latency_hud(after.ws.latency_hud, cx);
    }
    if after.app.guardrails != before.app.guardrails
        && let (Some(id), Some(t)) = (&target.account, crate::trading::Trading::try_global(cx))
    {
        let key = target.account_name.as_ref().unwrap_or(id);
        t.submit(
            lines_trading::Source::Api,
            Instant::now(),
            lines_trading::Command::SetGuardrails {
                account: lines_trading::AccountId::new(id),
                guardrails: after.app.guardrails.get(key).cloned().unwrap_or_default(),
            },
        );
    }
}

fn gather_facts(cx: &App) -> Facts {
    let mut f = Facts::default();
    if let Some(ws) = cx.try_global::<Workspace>()
        && let Some(st) = &ws.env.base.store
    {
        f.store_path = st.root.display().to_string();
        f.store_budget_gb = (st.budget_bytes as f64 / (1u64 << 30) as f64 * 10.0).round() / 10.0;
        f.depth_products = st.depth_products.clone();
    } else {
        f.store_path = "Not recording (this source has no store)".to_owned();
    }
    f.config_dir = crate::persist::config_dir().map_or_else(
        || "None (nothing is saved)".to_owned(),
        |d| d.display().to_string(),
    );
    if let Some(t) = crate::trading::Trading::try_global(cx) {
        f.accounts = t
            .accounts
            .iter()
            .map(|a| AccountFact {
                id: a.id.0.clone(),
                name: a.name.clone(),
                paper: a.paper,
                rules: a.rules.as_ref().map(|r| r.name.clone()),
            })
            .collect();
    }
    f
}

fn default_target(f: &Facts) -> Target {
    let a = f.accounts.first();
    Target {
        account: a.map(|a| a.id.clone()),
        account_name: a.map(|a| a.name.clone()),
        product: Some("ES".to_owned()),
    }
}

impl SettingsWindow {
    fn new(theme: Theme, facts: Facts, cx: &mut Context<Self>) -> SettingsWindow {
        SettingsWindow {
            focus: cx.focus_handle(),
            theme,
            section: Section::Appearance,
            query: String::new(),
            row: 0,
            edit: None,
            note: None,
            pending: None,
            target: default_target(&facts),
            products: lines_feed::catalog::all()
                .iter()
                .map(|p| p.symbol)
                .collect(),
            facts,
            undo: Vec::new(),
            editor: None,
            editor_msg: None,
            scroll: gpui::ScrollHandle::new(),
            scrolled: None,
        }
    }

    /// Leave whatever is half done (a number being typed, a confirmation, the preset
    /// editor): the account, product or section changed under it.
    fn drop_unfinished(&mut self) {
        self.edit = None;
        self.pending = None;
        self.editor = None;
        self.editor_msg = None;
    }

    /// Show setting `i`: its section, selected, the search cleared.
    fn show(&mut self, i: usize) {
        if let Some(st) = schema::all().get(i) {
            self.query.clear();
            self.section = st.section;
            self.row = self.rows().iter().position(|&r| r == i).unwrap_or(0);
            self.edit = None;
        }
    }

    /// The settings shown: the search's matches, else the section's.
    fn rows(&self) -> Vec<usize> {
        if self.query.trim().is_empty() {
            schema::all()
                .iter()
                .enumerate()
                .filter(|(_, s)| s.section == self.section)
                .map(|(i, _)| i)
                .collect()
        } else {
            schema::search(&self.query)
        }
    }

    fn selected(&self) -> Option<usize> {
        self.rows().get(self.row).copied()
    }

    /// Set setting `i` to `v` (validated; a loosened guardrail asks first unless `sure`).
    fn set(&mut self, i: usize, v: Value, sure: bool, cx: &mut Context<Self>) {
        let st = &schema::all()[i];
        let before = current_prefs(cx);
        let old = (st.get)(&before, &self.target, &self.facts);
        let mut after = before.clone();
        if let Err(e) = st.apply(&mut after, &self.target, v.clone()) {
            self.note = Some((i, e.to_owned(), Tone::Problem));
            cx.notify();
            return;
        }
        let guard = st.key.starts_with("guard.");
        let account = |p: &Prefs| {
            [&self.target.account_name, &self.target.account]
                .into_iter()
                .flatten()
                .find_map(|k| p.app.guardrails.get(k).cloned())
                .unwrap_or_default()
        };
        let (g0, g1) = (account(&before), account(&after));
        let loosens = schema::loosening(st, &old, &v, &g0, &g1);
        if loosens && !sure {
            let (from, to) = (st.value_text(&old), st.value_text(&v));
            self.pending = Some(Pending {
                index: i,
                value: v.clone(),
                target: self.target.clone(),
                text: if guard {
                    format!(
                        "Loosening ({from} → {to}) takes effect tomorrow, at the next trade date."
                    )
                } else {
                    format!("This loosens a trading safeguard ({from} → {to}).")
                },
            });
            self.note = None;
            cx.notify();
            return;
        }
        commit(&before, &after, &self.target, cx);
        self.undo.push((i, self.target.clone(), old.clone()));
        self.pending = None;
        let text = if guard {
            if loosens {
                format!(
                    "Loosened {} → {}: takes effect at the next trade date.",
                    st.value_text(&old),
                    st.value_text(&v)
                )
            } else {
                format!(
                    "{} {} → {}: applies now.",
                    if old == v { "Kept" } else { "Tightened" },
                    st.value_text(&old),
                    st.value_text(&v)
                )
            }
        } else {
            // The control shows the new value; a note would only repeat it.
            self.note = None;
            cx.notify();
            return;
        };
        self.note = Some((i, text, Tone::Done));
        cx.notify();
    }

    /// Yes to the pending loosening, for the account it was asked for.
    fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.pending.take() else { return };
        let keep = std::mem::replace(&mut self.target, p.target);
        self.set(p.index, p.value, true, cx);
        self.target = keep;
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let Some((i, target, v)) = self.undo.pop() else {
            self.note = None;
            return;
        };
        let keep = std::mem::replace(&mut self.target, target);
        let n = self.undo.len();
        // An undo that loosens asks like any other change.
        self.set(i, v, false, cx);
        self.undo.truncate(n);
        self.target = keep;
        if let Some(n) = &mut self.note {
            n.1 = format!("Undone · {}", n.1);
        }
    }

    fn reset(&mut self, i: usize, cx: &mut Context<Self>) {
        if let Some(d) = schema::all()[i].default.clone() {
            self.set(i, d, false, cx);
        }
    }

    fn step_account(&mut self) {
        let a = &self.facts.accounts;
        if a.is_empty() {
            return;
        }
        let k = a
            .iter()
            .position(|x| Some(&x.id) == self.target.account.as_ref())
            .map_or(0, |k| (k + 1) % a.len());
        self.target.account = Some(a[k].id.clone());
        self.target.account_name = Some(a[k].name.clone());
        self.drop_unfinished();
    }

    fn step_product(&mut self) {
        let k = self
            .products
            .iter()
            .position(|p| Some(*p) == self.target.product.as_deref())
            .map_or(0, |k| (k + 1) % self.products.len());
        self.target.product = Some(self.products[k].to_owned());
        self.drop_unfinished();
    }

    fn open_editor(&mut self, cx: &App) {
        let (Some(a), Some(sym)) = (&self.target.account, &self.target.product) else {
            self.editor_msg = Some("Choose an account and a product".to_owned());
            return;
        };
        let Some(product) = lines_feed::catalog::by_symbol(sym) else {
            return;
        };
        let s = settings(cx);
        let units = brackets::units_for(&s.bracket_presets, a);
        let presets = brackets::presets_for(&s.bracket_presets, a, sym);
        self.editor = Some(brackets::Editor::new(&product, units, &presets));
        self.editor_msg = None;
    }

    fn editor_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let Some(ed) = self.editor.as_mut() else {
            return;
        };
        match ed.key(
            k.key.as_str(),
            k.key_char.as_deref(),
            k.modifiers.control,
            k.modifiers.shift,
        ) {
            EditorKey::Typing | EditorKey::Ignored => self.editor_msg = None,
            EditorKey::Refused(m) => self.editor_msg = Some(m),
            EditorKey::Cancel => {
                self.editor = None;
                self.editor_msg = None;
            }
            EditorKey::Save(presets) => {
                if let (Some(a), Some(sym)) =
                    (self.target.account.clone(), self.target.product.clone())
                {
                    update_settings(cx, |s| {
                        s.bracket_presets
                            .entry(a)
                            .or_default()
                            .products
                            .insert(sym, presets);
                    });
                    self.note = schema::index_of("trading.brackets")
                        .map(|i| (i, "Presets saved".to_owned(), Tone::Done));
                }
                self.editor = None;
                self.editor_msg = None;
            }
        }
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if self.editor.is_some() {
            self.editor_key(ev, cx);
            return;
        }
        let k = &ev.keystroke;
        let m = k.modifiers;
        let key = k.key.as_str();
        let rows = self.rows();
        let sel = rows.get(self.row).copied();
        let kind = sel.map(|i| schema::all()[i].kind);
        // A number being typed takes digits, Backspace, Enter and Esc.
        if let (Some(text), Some(i)) = (self.edit.as_mut(), sel) {
            match key {
                "enter" => {
                    let parsed = schema::parse_number(text);
                    self.edit = None;
                    match parsed {
                        Ok(n) => self.set(i, Value::Number(n), false, cx),
                        Err(e) => self.note = Some((i, e.to_owned(), Tone::Problem)),
                    }
                }
                "escape" => self.edit = None,
                "backspace" => {
                    text.pop();
                }
                _ => {
                    if let Some(c) = k.key_char.as_deref().filter(|_| !m.control && !m.alt)
                        && c.chars().all(|c| c.is_ascii_digit() || c == ',')
                    {
                        text.push_str(c);
                    }
                }
            }
            cx.notify();
            return;
        }
        if self.pending.is_some() {
            match key {
                "enter" => self.confirm(cx),
                "escape" => {
                    self.pending = None;
                    self.note = None;
                }
                _ => {}
            }
            cx.notify();
            return;
        }
        match (key, m.control, m.shift, m.alt) {
            ("z", true, false, false) => self.undo(cx),
            ("r", true, false, false) => {
                if let Some(i) = sel {
                    self.reset(i, cx);
                }
            }
            ("a", false, false, true) => self.step_account(),
            ("p", false, false, true) => self.step_product(),
            ("up", ..) => self.row = self.row.saturating_sub(1),
            ("down", ..) => self.row = (self.row + 1).min(rows.len().saturating_sub(1)),
            ("tab", false, shift, false) => {
                let k = Section::ALL
                    .iter()
                    .position(|s| *s == self.section)
                    .unwrap_or(0);
                let n = Section::ALL.len();
                self.section = Section::ALL[if shift { (k + n - 1) % n } else { (k + 1) % n }];
                self.query.clear();
                self.row = 0;
                self.drop_unfinished();
            }
            ("escape", ..) => {
                if self.query.is_empty() {
                    window.remove_window();
                    cx.set_global(SettingsWindowGlobal(None));
                    return;
                }
                self.query.clear();
                self.row = 0;
            }
            ("backspace", ..) => {
                self.query.pop();
                self.row = 0;
            }
            ("left" | "right", false, false, false) => {
                if let (Some(i), Some(kind)) = (sel, kind) {
                    let st = &schema::all()[i];
                    let cur = (st.get)(&current_prefs(cx), &self.target, &self.facts);
                    match (kind, cur) {
                        (Kind::Choice(c), Value::Choice(k)) if !st.read_only() => {
                            let k = if key == "left" {
                                k.saturating_sub(1)
                            } else {
                                (k + 1).min(c.len() - 1)
                            };
                            self.set(i, Value::Choice(k), false, cx);
                        }
                        (Kind::Toggle, _) if !st.read_only() => {
                            self.set(i, Value::Bool(key == "right"), false, cx)
                        }
                        _ => {}
                    }
                }
            }
            // While searching, Space is a space and Enter goes to the selected setting:
            // typing never changes a setting.
            ("space", false, false, false) if !self.query.is_empty() => {
                self.query.push(' ');
                self.row = 0;
            }
            ("enter", false, false, false) if !self.query.trim().is_empty() => {
                if let Some(i) = sel {
                    self.show(i);
                }
            }
            ("enter" | "space", false, false, false) => {
                if let (Some(i), Some(kind)) = (sel, kind) {
                    self.activate(i, kind, cx);
                }
            }
            _ => {
                if let Some(c) = k.key_char.as_deref().filter(|_| !m.control && !m.alt) {
                    self.query.push_str(c);
                    self.row = 0;
                }
            }
        }
        cx.notify();
    }

    /// Enter or a click on a setting's control.
    fn activate(&mut self, i: usize, kind: Kind, cx: &mut Context<Self>) {
        let st = &schema::all()[i];
        if st.read_only() && !matches!(kind, Kind::Editor) {
            return;
        }
        let cur = (st.get)(&current_prefs(cx), &self.target, &self.facts);
        match (kind, cur) {
            (Kind::Toggle, Value::Bool(b)) => self.set(i, Value::Bool(!b), false, cx),
            (Kind::Choice(c), Value::Choice(k)) => {
                self.set(i, Value::Choice((k + 1) % c.len()), false, cx)
            }
            (Kind::Number { .. }, Value::Number(n)) => {
                self.edit = Some(n.map(|n| n.to_string()).unwrap_or_default());
            }
            (Kind::Editor, _) => self.open_editor(cx),
            _ => {}
        }
    }

    // ---- rendering ----------------------------------------------------------------------

    fn nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let counts: Vec<usize> = if self.query.trim().is_empty() {
            Vec::new()
        } else {
            let hits = schema::search(&self.query);
            Section::ALL
                .iter()
                .map(|s| {
                    hits.iter()
                        .filter(|&&i| schema::all()[i].section == *s)
                        .count()
                })
                .collect()
        };
        let mut nav = div()
            .flex()
            .flex_col()
            .w(px(150.0))
            .flex_none()
            .py_2()
            .border_r_1()
            .border_color(t.axis_line);
        for (k, s) in Section::ALL.into_iter().enumerate() {
            let on = s == self.section && self.query.trim().is_empty();
            let count = counts.get(k).copied().filter(|&n| n > 0);
            nav = nav.child(
                div()
                    .id(("section", k))
                    .flex()
                    .flex_row()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .text_color(if on { t.text_main } else { t.text_dim })
                    .when(on, |d| d.bg(t.panel).border_l_2().border_color(t.accent))
                    .child(s.name())
                    .children(count.map(|n| div().text_size(px(11.0)).child(n.to_string())))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |v, _, _, cx| {
                            v.section = s;
                            v.query.clear();
                            v.row = 0;
                            v.drop_unfinished();
                            cx.notify();
                        }),
                    ),
            );
        }
        nav
    }

    fn control(&self, i: usize, value: &Value, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let st = &schema::all()[i];
        let selected = self.selected() == Some(i);
        match (st.kind, value) {
            (Kind::Choice(c), Value::Choice(k)) => {
                let mut seg = div()
                    .flex()
                    .flex_row()
                    .flex_none()
                    .border_1()
                    .border_color(t.axis_line)
                    .rounded_sm();
                for (j, name) in c.iter().enumerate() {
                    let on = j == *k;
                    seg = seg.child(
                        div()
                            .id(("choice", i * 64 + j))
                            .px_2()
                            .cursor_pointer()
                            .when(on, |d| d.bg(t.accent).text_color(t.bg))
                            .child(*name)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |v, _, _, cx| {
                                    v.select(i);
                                    v.set(i, Value::Choice(j), false, cx);
                                }),
                            ),
                    );
                }
                seg.into_any_element()
            }
            (Kind::Toggle, Value::Bool(b)) => {
                let b = *b;
                div()
                    .id(("toggle", i))
                    .flex_none()
                    .px_2()
                    .rounded_sm()
                    .border_1()
                    .cursor_pointer()
                    // On and off by text as well as fill.
                    .when(b, |d| {
                        d.bg(t.accent).border_color(t.accent).text_color(t.bg)
                    })
                    .when(!b, |d| d.border_color(t.axis_line).text_color(t.text_dim))
                    .child(if b { "On" } else { "Off" })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |v, _, _, cx| {
                            v.select(i);
                            v.set(i, Value::Bool(!b), false, cx);
                        }),
                    )
                    .into_any_element()
            }
            (Kind::Number { unit, .. }, Value::Number(n)) => {
                let editing = selected && self.edit.is_some();
                let text = match (&self.edit, editing) {
                    (Some(e), true) => format!("{e}▏"),
                    _ => n.map_or_else(|| "none".to_owned(), schema::grouped),
                };
                let mut d = div()
                    .id(("number", i))
                    .flex_none()
                    .min_w(px(72.0))
                    .px_2()
                    .rounded_sm()
                    .border_1()
                    .cursor_pointer()
                    .flex()
                    .justify_end()
                    .border_color(if editing { t.accent } else { t.axis_line });
                if unit == "$" {
                    d = d.child(format!("${text}"));
                } else {
                    d = d.child(text);
                    if !unit.is_empty() {
                        d = d.child(div().ml_1().text_color(t.text_dim).child(unit));
                    }
                }
                d.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v, _, _, cx| {
                        v.select(i);
                        v.activate(i, schema::all()[i].kind, cx);
                        cx.notify();
                    }),
                )
                .into_any_element()
            }
            (Kind::Editor, _) => div()
                .id(("editor", i))
                .flex_none()
                .px_2()
                .rounded_sm()
                .border_1()
                .border_color(t.axis_line)
                .cursor_pointer()
                .child("Edit…")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v, _, _, cx| {
                        v.select(i);
                        v.open_editor(cx);
                        cx.notify();
                    }),
                )
                .into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn select(&mut self, i: usize) {
        if let Some(r) = self.rows().iter().position(|&x| x == i) {
            self.row = r;
        }
        self.edit = None;
    }

    fn row_view(&self, i: usize, prefs: &Prefs, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let st = &schema::all()[i];
        let value = (st.get)(prefs, &self.target, &self.facts);
        let selected = self.selected() == Some(i);
        let searching = !self.query.trim().is_empty();
        let mut name = div().flex().flex_row().items_center().gap_2();
        if searching {
            name = name.child(
                div()
                    .text_size(px(11.0))
                    .text_color(t.text_dim)
                    .child(format!("{} ›", st.section.name())),
            );
        }
        name = name.child(div().text_color(t.text_main).child(st.name));
        if st.safety == Safety::Trading {
            name = name.child(
                div()
                    .px_1()
                    .text_size(px(10.0))
                    .font_weight(FontWeight::BOLD)
                    .border_1()
                    .border_color(t.warn)
                    .text_color(t.warn)
                    .rounded_sm()
                    .child("TRADING"),
            );
        }
        let mut left = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(name)
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(t.text_dim)
                    .child(st.description),
            );
        if let (Kind::Info, Value::Text(s)) = (st.kind, &value) {
            left = left.child(div().text_color(t.text_main).child(s.clone()));
        }
        if let (Kind::Editor, Value::Text(s)) = (st.kind, &value) {
            left = left.child(
                div()
                    .text_size(px(11.5))
                    .text_color(t.text_main)
                    .child(s.clone()),
            );
        }
        if let Some((n, text, tone)) = self.note.as_ref().filter(|n| n.0 == i) {
            let _ = n;
            left = left.child(
                div()
                    .text_size(px(11.5))
                    .text_color(match tone {
                        Tone::Problem => t.warn,
                        Tone::Done => t.text_main,
                    })
                    .child(text.clone()),
            );
        }
        let changed = st.changed(prefs, &self.target, &self.facts);
        let scope = match st.scope {
            Scope::Account | Scope::AccountProduct => {
                let mut s = self
                    .target
                    .account_name
                    .clone()
                    .unwrap_or_else(|| "no account".to_owned());
                if st.scope == Scope::AccountProduct {
                    s.push_str(" · ");
                    s.push_str(self.target.product.as_deref().unwrap_or("?"));
                }
                s
            }
            s => s.label().to_owned(),
        };
        let mut row = div()
            .id(("row", i))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(t.axis_line)
            .when(selected, |d| d.bg(t.panel))
            .when(st.safety == Safety::Trading, |d| {
                d.border_l_2().border_color(t.warn)
            })
            .child(left)
            .child(self.control(i, &value, cx))
            .child(
                div()
                    .flex_none()
                    .px_1()
                    .text_size(px(10.5))
                    .rounded_sm()
                    .border_1()
                    .border_color(t.axis_line)
                    .text_color(t.text_dim)
                    .child(scope),
            )
            .child(
                div()
                    .id(("reset", i))
                    .w(px(16.0))
                    .flex_none()
                    .text_color(t.text_dim)
                    .when(changed, |d| {
                        crate::tooltip::tip_div(d, "Reset to default (Ctrl+R)", None, &t)
                            .cursor_pointer()
                            .child("↺")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |v, _, _, cx| {
                                    v.select(i);
                                    v.reset(i, cx);
                                }),
                            )
                    }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, _, _, cx| {
                    v.select(i);
                    cx.notify();
                }),
            )
            .into_any_element();
        if let Some(p) = self.pending.as_ref().filter(|p| p.index == i) {
            row = div()
                .flex()
                .flex_col()
                .child(row)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .mx_2()
                        .my_1()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(t.warn)
                        .child(div().child(format!("⚠ {}", p.text)))
                        .child(
                            div()
                                .id("loosen-yes")
                                .px_2()
                                .border_1()
                                .border_color(t.warn)
                                .rounded_sm()
                                .font_weight(FontWeight::BOLD)
                                .cursor_pointer()
                                .child("Loosen from tomorrow · Enter")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|v, _, _, cx| v.confirm(cx)),
                                ),
                        )
                        .child(
                            div()
                                .id("loosen-no")
                                .px_2()
                                .border_1()
                                .border_color(t.axis_line)
                                .rounded_sm()
                                .cursor_pointer()
                                .child("Cancel · Esc")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|v, _, _, cx| {
                                        v.pending = None;
                                        cx.notify();
                                    }),
                                ),
                        ),
                )
                .into_any_element();
        }
        row
    }

    fn keys_list(&self) -> AnyElement {
        let t = self.theme;
        let mut list = div().flex().flex_col().mt_1();
        let mut seen = Vec::new();
        for b in crate::commands::BINDINGS {
            if seen.contains(&b.cmd) {
                continue;
            }
            seen.push(b.cmd);
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .gap_2()
                    .px_2()
                    .border_b_1()
                    .border_color(t.axis_line)
                    .child(
                        div()
                            .text_color(t.text_main)
                            .child(crate::commands::label(b.cmd)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(t.text_dim)
                            .child(crate::commands::hint(b.cmd)),
                    ),
            );
        }
        list.into_any_element()
    }

    fn editor_view(&self) -> Option<AnyElement> {
        let t = self.theme;
        let ed = self.editor.as_ref()?;
        let mut v = div()
            .flex()
            .flex_col()
            .gap_1()
            .mx_2()
            .my_1()
            .p_2()
            .rounded_sm()
            .border_1()
            .border_color(t.accent)
            .child(div().font_weight(FontWeight::BOLD).child(format!(
                "Bracket presets · {} · {} ({})",
                self.target.account_name.as_deref().unwrap_or("?"),
                ed.product.symbol,
                ed.units.word()
            )))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .text_color(t.text_dim)
                    .child(div().w(px(90.0)).child("Stop"))
                    .child(div().w(px(90.0)).child("Target"))
                    .child(div().w(px(140.0)).child("Name")),
            );
        for (r, row) in ed.rows.iter().enumerate() {
            let cell = |text: &str, on: bool, w: f32| {
                div()
                    .w(px(w))
                    .px_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(if on { t.accent } else { t.axis_line })
                    .child(format!("{text}{}", if on { "▏" } else { "" }))
            };
            let on = |f| r == ed.row && ed.field == f;
            v = v.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(cell(&row.stop, on(brackets::Field::Stop), 90.0))
                    .child(cell(&row.target, on(brackets::Field::Target), 90.0))
                    .child(cell(&row.name, on(brackets::Field::Name), 140.0)),
            );
        }
        let problem = self
            .editor_msg
            .clone()
            .or_else(|| ed.cell_problem(ed.row, ed.field));
        v = v.child(
            div()
                .text_size(px(11.5))
                .text_color(if problem.is_some() { t.warn } else { t.text_dim })
                .child(problem.unwrap_or_else(|| {
                    "Enter saves · Esc cancels · Insert adds · Delete removes · Ctrl+↑↓ reorders · Tab moves".to_owned()
                })),
        );
        Some(v.into_any_element())
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let prefs = current_prefs(cx);
        let rows = self.rows();
        if self.row >= rows.len() {
            self.row = rows.len().saturating_sub(1);
        }
        let searching = !self.query.trim().is_empty();
        let title = if searching {
            format!("{} results", rows.len())
        } else {
            self.section.name().to_owned()
        };
        let mut main = div()
            .id("settings-main")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px_3()
            .py_2()
            .child(div().font_weight(FontWeight::BOLD).pb_1().child(title));
        // Per-account settings: which account (and product) they mean.
        let per_account = rows.iter().any(|&i| {
            matches!(
                schema::all()[i].scope,
                Scope::Account | Scope::AccountProduct
            )
        });
        // Children before the rows (title, account line): the selected row's index for
        // scrolling it into view.
        let mut child = 1;
        if per_account {
            child += 1;
            main = main.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .pb_1()
                    .text_size(px(12.0))
                    .text_color(t.text_dim)
                    .child(
                        div()
                            .id("account-step")
                            .px_1()
                            .border_1()
                            .border_color(t.axis_line)
                            .rounded_sm()
                            .cursor_pointer()
                            .child(format!(
                                "Account: {} ▾",
                                self.target.account_name.as_deref().unwrap_or("none")
                            ))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|v, _, _, cx| {
                                    v.step_account();
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("product-step")
                            .px_1()
                            .border_1()
                            .border_color(t.axis_line)
                            .rounded_sm()
                            .cursor_pointer()
                            .child(format!(
                                "Product: {} ▾",
                                self.target.product.as_deref().unwrap_or("none")
                            ))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|v, _, _, cx| {
                                    v.step_product();
                                    cx.notify();
                                }),
                            ),
                    ),
            );
        }
        if rows.is_empty() {
            main = main.child(div().text_color(t.text_dim).child("No setting matches"));
        }
        let mut selected_child = None;
        for (r, &i) in rows.iter().enumerate() {
            if r == self.row {
                selected_child = Some(child);
            }
            main = main.child(self.row_view(i, &prefs, cx));
            child += 1;
            if schema::all()[i].key == "trading.brackets"
                && let Some(e) = self.editor_view()
            {
                main = main.child(e);
                child += 1;
            }
            if schema::all()[i].key == "keys.list" {
                main = main.child(self.keys_list());
                child += 1;
            }
        }
        let at = (self.section, self.row, searching);
        if let Some(c) = selected_child
            && self.scrolled != Some(at)
        {
            self.scroll.scroll_to_item(c);
            self.scrolled = Some(at);
        }
        let search = div()
            .flex_1()
            .mx_3()
            .px_2()
            .rounded_sm()
            .border_1()
            .border_color(if searching { t.accent } else { t.axis_line })
            .text_color(if searching { t.text_main } else { t.text_dim })
            .child(if searching {
                format!("{}▏", self.query)
            } else {
                "Search settings (type to filter)…".to_owned()
            });
        div()
            .id("settings")
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .text_color(t.text_main)
            .text_size(px(13.0))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(t.axis_line)
                    .child(div().font_weight(FontWeight::BOLD).child("Settings"))
                    .child(search)
                    .child(div().text_color(t.text_dim).text_size(px(12.0)).child("Esc closes")),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(self.nav(cx))
                    .child(main),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(t.axis_line)
                    .text_size(px(11.5))
                    .text_color(t.text_dim)
                    .child("↑↓ settings · ←→ change · Enter edit · Tab sections · Ctrl+Z undo · Ctrl+R reset")
                    .child("Changes apply at once · every setting is also a palette command"),
            )
    }
}
