//! Settings › Connections' page (decision 0030 §3–4): one card per source with its state, its
//! plan, its on/off switch, the settings it describes (text, choice, switch, secret) and Test.
//! Secrets are read and written on a worker thread (a keychain may block), shown masked to
//! their last 4 characters, with Show, Replace and Remove.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, SharedString, Styled, div, prelude::FluentBuilder,
};
use lines_feed::extension::{Field, FieldKind};
use lines_feed::registry::Subscription;
use lines_feed::store::FeedStatus;

use crate::connections::{self as conn, Tone};
use crate::settings_view::SettingsWindow;
use crate::theme::Theme;

/// How long Test waits for the source to go live.
const TEST_FOR: Duration = Duration::from_secs(15);

/// Secrets and setup, as the worker read them.
#[derive(Default)]
pub(crate) struct Loaded {
    /// `(source, field)` → the saved secret (`None`: none saved).
    secrets: HashMap<(String, &'static str), Option<String>>,
    /// Source → the first required setting with no value.
    needs: HashMap<String, Option<&'static str>>,
    /// The keychain's complaint, if any (never a secret).
    error: Option<String>,
}

/// A text or secret field being typed.
pub(crate) struct Edit {
    pub source: String,
    pub field: Field,
    pub buf: String,
}

struct Test {
    source: String,
    sub: Subscription,
    started: Instant,
    last_reason: Option<String>,
}

/// What the page holds between frames.
#[derive(Default)]
pub(crate) struct ConnPage {
    loaded: Option<Loaded>,
    worker: Option<mpsc::Receiver<Loaded>>,
    /// Secrets shown in full (Show pressed), until the window closes.
    shown: HashSet<(String, &'static str)>,
    pub(crate) edit: Option<Edit>,
    test: Option<Test>,
    /// Source → the last Test's words and whether it failed.
    results: HashMap<String, (String, bool)>,
}

/// What a press on the page asks for (`SettingsWindow::conn_action` does it).
#[derive(Clone)]
pub(crate) enum Action {
    /// A source on or off (applies at the next start).
    Switch(String, bool),
    Show(String, &'static str),
    /// Start typing a text or secret field.
    Edit(String, Field),
    Remove(String, &'static str),
    /// A plain field's value (a choice, or a switch's `on`/`off`).
    Set(String, &'static str, String),
    Test(String),
    Save,
    Cancel,
}

/// What a finished edit writes.
pub(crate) enum Commit {
    Secret(String, &'static str, String),
    Plain(String, &'static str, String),
}

impl ConnPage {
    /// Read every source's secrets and setup off the UI thread, after `first` (a save or a
    /// remove) if given.
    pub(crate) fn reload(&mut self, first: Option<Box<dyn FnOnce() -> Result<(), String> + Send>>) {
        let rows = conn::rows();
        let read = move || {
            let mut l = Loaded::default();
            if let Some(f) = first
                && let Err(e) = f()
            {
                l.error = Some(e);
            }
            for r in &rows {
                for f in r.fields.iter().filter(|f| f.kind == FieldKind::Secret) {
                    let v = conn::secret(&r.name, f.key).unwrap_or_else(|e| {
                        l.error.get_or_insert(e);
                        None
                    });
                    l.secrets.insert((r.name.clone(), f.key), v);
                }
                l.needs
                    .insert(r.name.clone(), conn::needs(&r.name, &r.fields));
            }
            l
        };
        // Secrets in memory (screenshots, tests) never block: read them now.
        if conn::memory_keychain() {
            self.loaded = Some(read());
            self.worker = None;
            return;
        }
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("lines-keychain".into())
            .spawn(move || {
                let _ = tx.send(read());
            });
        if spawned.is_ok() {
            self.worker = Some(rx);
        }
    }

    /// Whether the page has read the keychain yet (or is reading it).
    pub(crate) fn started(&self) -> bool {
        self.loaded.is_some() || self.worker.is_some()
    }

    /// The window's poll: the worker's answer, the running Test. `true`: redraw.
    pub(crate) fn tick(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.worker
            && let Ok(l) = rx.try_recv()
        {
            self.loaded = Some(l);
            self.worker = None;
            changed = true;
        }
        if let Some(t) = &mut self.test {
            let status = t.sub.store().try_read().map(|d| d.status.clone());
            if let Some(FeedStatus::Reconnecting { reason, .. }) = &status {
                t.last_reason = Some(reason.clone());
            }
            let done = match status {
                Some(FeedStatus::Live) => Some((
                    format!("Connected in {:.1} s", t.started.elapsed().as_secs_f32()),
                    false,
                )),
                Some(FeedStatus::Failed(why)) => Some((format!("Refused: {why}"), true)),
                _ if t.started.elapsed() >= TEST_FOR => Some((
                    match &t.last_reason {
                        Some(r) => format!("No answer in {} s: {r}", TEST_FOR.as_secs()),
                        None => format!("No answer in {} s", TEST_FOR.as_secs()),
                    },
                    true,
                )),
                _ => None,
            };
            if let Some(r) = done {
                self.results.insert(t.source.clone(), r);
                // Dropping the subscription stops the feed unless a pane shows it.
                self.test = None;
                changed = true;
            }
        }
        changed
    }

    /// Test a source: join (or start) its feed on its own symbol and wait for it to go live.
    pub(crate) fn start_test(&mut self, source: &str, cx: &App) {
        let Some(env) = crate::workspace::try_env(cx) else {
            return;
        };
        let Some(entry) = env.sources.get(source) else {
            self.results.insert(
                source.to_owned(),
                (
                    "Not in this run: it starts when Lines restarts".to_owned(),
                    true,
                ),
            );
            return;
        };
        match crate::feeds::Feeds::registry(cx).subscribe(entry.base.clone()) {
            Ok(sub) => {
                self.results.remove(source);
                self.test = Some(Test {
                    source: source.to_owned(),
                    sub,
                    started: Instant::now(),
                    last_reason: None,
                });
            }
            Err(e) => {
                self.results
                    .insert(source.to_owned(), (format!("Refused: {e:#}"), true));
            }
        }
    }

    pub(crate) fn toggle_shown(&mut self, source: &str, key: &'static str) {
        let k = (source.to_owned(), key);
        if !self.shown.remove(&k) {
            self.shown.insert(k);
        }
    }

    /// A key while a field is typed: `Some` when Enter finishes it. Ctrl+V pastes.
    pub(crate) fn key(&mut self, ev: &KeyDownEvent, cx: &App) -> Option<Commit> {
        let k = &ev.keystroke;
        let m = k.modifiers;
        let edit = self.edit.as_mut()?;
        match k.key.as_str() {
            "enter" => return self.finish(),
            "escape" => self.edit = None,
            "backspace" => {
                edit.buf.pop();
            }
            "v" if (m.control || m.platform) && !m.alt => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    // One line: a pasted key often ends with a newline.
                    edit.buf.push_str(text.trim());
                }
            }
            _ => {
                if let Some(c) = k.key_char.as_deref().filter(|_| !m.control && !m.alt) {
                    edit.buf.extend(c.chars().filter(|c| !c.is_control()));
                }
            }
        }
        None
    }

    /// Enter or Save: what the edit writes (an empty secret is ignored: Remove clears one).
    pub(crate) fn finish(&mut self) -> Option<Commit> {
        let e = self.edit.take()?;
        let value = e.buf.trim().to_owned();
        match e.field.kind {
            FieldKind::Secret if value.is_empty() => None,
            FieldKind::Secret => Some(Commit::Secret(e.source, e.field.key, value)),
            _ => Some(Commit::Plain(e.source, e.field.key, value)),
        }
    }
}

/// Off | On (or a choice's values), the current one filled: a state, never a lone button.
fn segments(
    id: &str,
    options: &[(&str, bool, Action)],
    t: &Theme,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let mut seg = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .flex_none()
        .border_1()
        .border_color(t.axis_line)
        .rounded_sm();
    for (j, (name, on, action)) in options.iter().enumerate() {
        let a = action.clone();
        seg = seg.child(
            div()
                .id(SharedString::from(format!("{id}-{j}")))
                .px_2()
                .whitespace_nowrap()
                .cursor_pointer()
                .when(*on, |d| d.bg(t.accent).text_color(t.bg))
                .child((*name).to_owned())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v, _, _, cx| v.conn_action(a.clone(), cx)),
                ),
        );
    }
    seg.into_any_element()
}

fn button(
    id: String,
    label: &str,
    action: Action,
    t: &Theme,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    div()
        .id(SharedString::from(id))
        .flex_none()
        .whitespace_nowrap()
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(t.axis_line)
        .cursor_pointer()
        .child(label.to_owned())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |v, _, _, cx| v.conn_action(action.clone(), cx)),
        )
        .into_any_element()
}

/// A label column and its content, as Settings' rows lay out.
fn line(label: &str, content: AnyElement, t: &Theme) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .mt_1()
        .child(
            div()
                .w(crate::zoom::ui(110.0))
                .flex_none()
                .text_color(t.text_dim)
                .child(label.to_owned()),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_1()
                .min_w_0()
                .child(content),
        )
        .into_any_element()
}

fn field_box(text: String, editing: bool, t: &Theme) -> AnyElement {
    div()
        .flex_none()
        .min_w(crate::zoom::ui(160.0))
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(if editing { t.accent } else { t.axis_line })
        .text_color(t.text_main)
        .child(text)
        .into_any_element()
}

/// The page: one card per source (`connections::rows`).
pub(crate) fn list(page: &ConnPage, t: &Theme, cx: &mut Context<SettingsWindow>) -> AnyElement {
    let settings = crate::picker_view::settings(cx);
    let prefs = settings.sources.clone();
    let plain = settings.connections.clone();
    let env = crate::workspace::try_env(cx);
    let registry = crate::feeds::Feeds::registry(cx);
    let mut out = div().flex().flex_col().gap_2().mt_1();
    if let Some(err) = page.loaded.as_ref().and_then(|l| l.error.as_ref()) {
        out = out.child(
            div()
                .text_color(t.text_main)
                .child(format!("Keychain: {err}")),
        );
    }
    for row in conn::rows() {
        let name = row.name.clone();
        let on = prefs.get(&name).is_none_or(crate::sources::SourcePrefs::on);
        let entry = env.as_ref().and_then(|e| e.sources.get(&name));
        let feeds: Vec<FeedStatus> = entry
            .as_ref()
            .map(|e| {
                registry
                    .stores_of(&lines_feed::registry::source_key(&e.base.source))
                    .iter()
                    .filter_map(|s| s.try_read().map(|d| d.status.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let needs = page
            .loaded
            .as_ref()
            .and_then(|l| l.needs.get(&name).copied())
            .flatten();
        let state = conn::state(on, entry.is_some(), needs, &feeds);
        let checking = !row.fields.is_empty() && page.loaded.is_none();
        let dot = match state.tone() {
            Tone::Good => div().size(crate::zoom::ui(8.0)).rounded_full().bg(t.accent),
            Tone::Bad => div()
                .size(crate::zoom::ui(8.0))
                .rounded_full()
                .bg(t.destructive),
            Tone::Busy => div().size(crate::zoom::ui(8.0)).rounded_full().bg(t.warn),
            Tone::Quiet => div()
                .size(crate::zoom::ui(8.0))
                .rounded_full()
                .border_1()
                .border_color(t.text_dim),
        };
        let state_text = if checking {
            "Checking…".to_owned()
        } else {
            state.text()
        };
        let switch = segments(
            &format!("conn-{name}-switch"),
            &[
                ("Off", !on, Action::Switch(name.clone(), false)),
                ("On", on, Action::Switch(name.clone(), true)),
            ],
            t,
            cx,
        );
        let mut card = div()
            .flex()
            .flex_col()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(t.axis_line)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_none()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(t.text_main)
                            .child(crate::sources::source_label(&name)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .flex_1()
                            .min_w_0()
                            .child(dot.flex_none())
                            .child(div().min_w_0().text_color(t.text_main).child(state_text)),
                    )
                    .child(switch),
            )
            .child(line(
                "Plan",
                div().child(row.plan.clone()).into_any_element(),
                t,
            ));
        for f in &row.fields {
            let editing = page
                .edit
                .as_ref()
                .filter(|e| e.source == name && e.field.key == f.key);
            let id = format!("conn-{name}-{}", f.key);
            let content = match (&f.kind, editing) {
                (FieldKind::Secret | FieldKind::Text, Some(e)) => {
                    let shown = if f.kind == FieldKind::Secret {
                        "•".repeat(e.buf.chars().count())
                    } else {
                        e.buf.clone()
                    };
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .child(field_box(format!("{shown}▏"), true, t))
                        .child(button(format!("{id}-save"), "Save", Action::Save, t, cx))
                        .child(button(
                            format!("{id}-cancel"),
                            "Cancel",
                            Action::Cancel,
                            t,
                            cx,
                        ))
                        .into_any_element()
                }
                (FieldKind::Secret, None) => {
                    let saved = page
                        .loaded
                        .as_ref()
                        .map(|l| l.secrets.get(&(name.clone(), f.key)).cloned().flatten());
                    let mut d = div().flex().flex_row().gap_1();
                    match saved {
                        None => d = d.child(field_box("…".to_owned(), false, t)),
                        Some(None) => {
                            d = d
                                .child(field_box("Not set".to_owned(), false, t))
                                .child(button(
                                    format!("{id}-enter"),
                                    "Enter",
                                    Action::Edit(name.clone(), f.clone()),
                                    t,
                                    cx,
                                ))
                        }
                        Some(Some(v)) => {
                            let open = page.shown.contains(&(name.clone(), f.key));
                            d = d
                                .child(field_box(if open { v } else { conn::masked(&v) }, false, t))
                                .child(button(
                                    format!("{id}-show"),
                                    if open { "Hide" } else { "Show" },
                                    Action::Show(name.clone(), f.key),
                                    t,
                                    cx,
                                ))
                                .child(button(
                                    format!("{id}-replace"),
                                    "Replace",
                                    Action::Edit(name.clone(), f.clone()),
                                    t,
                                    cx,
                                ))
                                .child(button(
                                    format!("{id}-remove"),
                                    "Remove",
                                    Action::Remove(name.clone(), f.key),
                                    t,
                                    cx,
                                ));
                        }
                    }
                    d.into_any_element()
                }
                (FieldKind::Text, None) => div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(field_box(
                        conn::plain_value(&plain, &name, f).unwrap_or_else(|| "Not set".to_owned()),
                        false,
                        t,
                    ))
                    .child(button(
                        format!("{id}-edit"),
                        "Edit",
                        Action::Edit(name.clone(), f.clone()),
                        t,
                        cx,
                    ))
                    .into_any_element(),
                (FieldKind::Choice(values), _) => {
                    let now = conn::plain_value(&plain, &name, f);
                    let opts: Vec<(&str, bool, Action)> = values
                        .iter()
                        .map(|v| {
                            (
                                *v,
                                now.as_deref() == Some(*v),
                                Action::Set(name.clone(), f.key, (*v).to_owned()),
                            )
                        })
                        .collect();
                    segments(&id, &opts, t, cx)
                }
                (FieldKind::Switch, _) => {
                    let now = conn::plain_value(&plain, &name, f).as_deref() == Some("on");
                    segments(
                        &id,
                        &[
                            ("Off", !now, Action::Set(name.clone(), f.key, "off".into())),
                            ("On", now, Action::Set(name.clone(), f.key, "on".into())),
                        ],
                        t,
                        cx,
                    )
                }
            };
            card = card.child(line(f.label, content, t));
            // An environment variable wins over what is saved here (tests, headless runs).
            if let Some(var) = f
                .env
                .filter(|v| std::env::var(v).is_ok_and(|x| !x.is_empty()))
            {
                card = card.child(line(
                    "",
                    div()
                        .text_color(t.text_dim)
                        .child(format!("{var} is set and overrides this"))
                        .into_any_element(),
                    t,
                ));
            }
        }
        let testing = page.test.as_ref().is_some_and(|x| x.source == name);
        let mut test_row = div().flex().flex_row().items_center().gap_2();
        test_row = if testing {
            test_row.child(field_box("Testing…".to_owned(), false, t))
        } else {
            test_row.child(button(
                format!("conn-{name}-test"),
                "Test",
                Action::Test(name.clone()),
                t,
                cx,
            ))
        };
        if let Some((words, bad)) = page.results.get(&name).filter(|_| !testing) {
            test_row = test_row.child(
                div()
                    .min_w_0()
                    .text_color(if *bad { t.destructive } else { t.text_main })
                    .child(words.clone()),
            );
        }
        card = card.child(line("", test_row.into_any_element(), t));
        out = out.child(card);
    }
    out.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(kind: FieldKind, buf: &str) -> ConnPage {
        ConnPage {
            edit: Some(Edit {
                source: "acme".into(),
                field: Field {
                    key: "k",
                    label: "K",
                    kind,
                    required: true,
                    env: None,
                },
                buf: buf.into(),
            }),
            ..ConnPage::default()
        }
    }

    /// Decision 0030 §4: a typed secret goes to the keychain (trimmed); an empty one writes
    /// nothing (Remove clears a secret); a plain field writes what was typed.
    #[test]
    fn a_finished_field_writes_where_its_kind_belongs() {
        let mut p = edit(FieldKind::Secret, "  db-key-1234  ");
        assert!(matches!(
            p.finish(),
            Some(Commit::Secret(s, "k", v)) if s == "acme" && v == "db-key-1234"
        ));
        assert!(p.edit.is_none());
        assert!(edit(FieldKind::Secret, "   ").finish().is_none());
        assert!(matches!(
            edit(FieldKind::Text, "GLBX").finish(),
            Some(Commit::Plain(_, "k", v)) if v == "GLBX"
        ));
        assert!(ConnPage::default().finish().is_none());
    }

    /// Show toggles a secret in full and back.
    #[test]
    fn show_toggles_one_secret() {
        let mut p = ConnPage::default();
        p.toggle_shown("acme", "k");
        assert!(p.shown.contains(&("acme".to_owned(), "k")));
        p.toggle_shown("acme", "k");
        assert!(p.shown.is_empty());
    }
}
