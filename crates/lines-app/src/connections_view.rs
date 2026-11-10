//! Settings › Connections' page (decisions 0030 §3–4 and 0035): one card per source with its state, its
//! plan, its on/off switch, the settings it describes (text, choice, switch, secret) and Test.
//!
//! A saved secret is never shown again in full (only "Configured", where it comes from and
//! its last 4 characters): the keychain is read on a worker thread (a keychain may block) and
//! only the masked form reaches the window. Replace types a new one; Remove takes two presses.
//! Every control is reachable from the keyboard: ↑/↓ move between them, Enter or Space press.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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
/// How long a first press on Remove waits for the second.
const REMOVE_CONFIRM: Duration = Duration::from_secs(4);

/// Secrets and setup, as the worker read them.
#[derive(Default)]
pub(crate) struct Loaded {
    /// `(source, field)` → the saved secret masked to its last 4 characters (`None`: none
    /// saved). The full value never leaves the worker.
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
    /// A pane's running connection was joined (its answer is that connection's, not a new
    /// one's).
    reused: bool,
}

/// One keychain job: an optional write, then a fresh read of everything.
type Job = Box<dyn FnOnce() -> Loaded + Send>;

/// The keychain worker: one thread, jobs in order (a Remove then a Replace never land
/// out of order).
struct Worker {
    jobs: mpsc::Sender<Job>,
    done: mpsc::Receiver<Loaded>,
}

/// What the page holds between frames.
#[derive(Default)]
pub(crate) struct ConnPage {
    loaded: Option<Loaded>,
    worker: Option<Worker>,
    /// A secret's Remove pressed once: a second press within `REMOVE_CONFIRM` removes it.
    remove_armed: Option<(String, &'static str, Instant)>,
    pub(crate) edit: Option<Edit>,
    test: Option<Test>,
    /// Source → the last Test's words and whether it failed.
    results: HashMap<String, (String, bool)>,
    /// The keyboard's control, an index into `controls` (kept inside it as controls come
    /// and go).
    focus: Cell<usize>,
    /// Each running store's last status seen (by its address): a store busy with a batch
    /// keeps it, since drawing never waits on a feed.
    statuses: RefCell<HashMap<usize, FeedStatus>>,
    /// The page's controls in drawing order, as last drawn (what ↑/↓ and Enter act on).
    controls: RefCell<Vec<Action>>,
}

/// What a press on the page asks for (`SettingsWindow::conn_action` does it).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Action {
    /// A source on or off (applies at the next start).
    Switch(String, bool),
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
#[derive(Debug, PartialEq)]
pub(crate) enum Commit {
    Secret(String, &'static str, String),
    Plain(String, &'static str, String),
    /// An emptied plain field: no value (a choice's first, else unset).
    Clear(String, &'static str),
}

impl ConnPage {
    /// Read every source's secrets (masked) and setup off the UI thread, after `first` (a
    /// save or a remove) if given.
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
                    l.secrets
                        .insert((r.name.clone(), f.key), v.as_deref().map(conn::masked));
                }
                l.needs
                    .insert(r.name.clone(), conn::needs(&r.name, &r.fields));
            }
            l
        };
        // Secrets in memory (screenshots, tests) never block: read them now.
        if conn::memory_keychain() {
            self.loaded = Some(read());
            return;
        }
        if self.worker.is_none() {
            let (jobs, rx) = mpsc::channel::<Job>();
            let (tx, done) = mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("lines-keychain".into())
                .spawn(move || {
                    for job in rx {
                        if tx.send(job()).is_err() {
                            break;
                        }
                    }
                });
            if let Err(e) = spawned {
                // Say so once; the page doesn't try again every frame.
                self.loaded = Some(Loaded {
                    error: Some(format!("couldn't start the keychain reader: {e}")),
                    ..Loaded::default()
                });
                return;
            }
            self.worker = Some(Worker { jobs, done });
        }
        if let Some(w) = &self.worker {
            let _ = w.jobs.send(Box::new(read));
        }
    }

    /// Whether the page has read the keychain yet (or is reading it).
    pub(crate) fn started(&self) -> bool {
        self.loaded.is_some() || self.worker.is_some()
    }

    /// Leaving the page (another section, a search): a field being typed is dropped, so its
    /// keys can't land in it unseen.
    pub(crate) fn leave(&mut self) {
        self.edit = None;
        self.remove_armed = None;
        self.focus.set(0);
    }

    /// Remove pressed: `true` on the second press within `REMOVE_CONFIRM` (remove now); the
    /// first only arms it (the button then asks to press again).
    pub(crate) fn press_remove(&mut self, source: &str, key: &'static str) -> bool {
        let now = Instant::now();
        let again = self.remove_armed.as_ref().is_some_and(|(s, k, at)| {
            s == source && *k == key && now.duration_since(*at) < REMOVE_CONFIRM
        });
        self.remove_armed = if again {
            None
        } else {
            Some((source.to_owned(), key, now))
        };
        again
    }

    fn remove_asks(&self, source: &str, key: &'static str) -> bool {
        self.remove_armed
            .as_ref()
            .is_some_and(|(s, k, at)| s == source && *k == key && at.elapsed() < REMOVE_CONFIRM)
    }

    /// ↑/↓ move the keyboard between the page's controls; Enter or Space press the one it is
    /// on (`Some`). Other keys: `None`, not handled.
    pub(crate) fn nav_key(&mut self, key: &str) -> Option<Option<Action>> {
        let n = self.controls.borrow().len();
        if n == 0 {
            return None;
        }
        match key {
            "up" => {
                self.focus
                    .set(self.focus.get().min(n - 1).saturating_sub(1));
                Some(None)
            }
            "down" => {
                self.focus.set((self.focus.get() + 1).min(n - 1));
                Some(None)
            }
            "enter" | "space" => Some(
                self.controls
                    .borrow()
                    .get(self.focus.get().min(n - 1))
                    .cloned(),
            ),
            _ => None,
        }
    }

    /// The window's poll: the worker's answer, the running Test. `true`: redraw.
    pub(crate) fn tick(&mut self) -> bool {
        let mut changed = false;
        if let Some(w) = &self.worker {
            loop {
                match w.done.try_recv() {
                    Ok(l) => {
                        self.loaded = Some(l);
                        changed = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        // The worker died (a panic): say so; the next press starts another.
                        self.loaded = Some(Loaded {
                            error: Some("the keychain reader stopped".to_owned()),
                            ..Loaded::default()
                        });
                        self.worker = None;
                        changed = true;
                        break;
                    }
                }
            }
        }
        if self
            .remove_armed
            .as_ref()
            .is_some_and(|(_, _, at)| at.elapsed() >= REMOVE_CONFIRM)
        {
            self.remove_armed = None;
            changed = true;
        }
        if let Some(t) = &mut self.test {
            let status = t.sub.store().try_read().map(|d| d.status.clone());
            if let Some(FeedStatus::Reconnecting { reason, .. }) = &status {
                t.last_reason = Some(reason.clone());
            }
            let done = match status {
                Some(FeedStatus::Live) if t.reused => Some((
                    "Connected (the connection a pane already uses)".to_owned(),
                    false,
                )),
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
        let registry = crate::feeds::Feeds::registry(cx);
        let reused = !registry
            .stores_of(&lines_feed::registry::source_key(&entry.base.source))
            .is_empty();
        match registry.subscribe(entry.base.clone()) {
            Ok(sub) => {
                self.results.remove(source);
                self.test = Some(Test {
                    source: source.to_owned(),
                    sub,
                    started: Instant::now(),
                    last_reason: None,
                    reused,
                });
            }
            Err(e) => {
                self.results
                    .insert(source.to_owned(), (format!("Refused: {e:#}"), true));
            }
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
                    // One line: a pasted key often carries a newline; no control characters.
                    edit.buf
                        .extend(text.trim().chars().filter(|c| !c.is_control()));
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

    /// Enter or Save: what the edit writes. An empty secret writes nothing (Remove clears
    /// one); an emptied plain field clears its value.
    pub(crate) fn finish(&mut self) -> Option<Commit> {
        let e = self.edit.take()?;
        let value = e.buf.trim().to_owned();
        match e.field.kind {
            FieldKind::Secret if value.is_empty() => None,
            FieldKind::Secret => Some(Commit::Secret(e.source, e.field.key, value)),
            _ if value.is_empty() => Some(Commit::Clear(e.source, e.field.key)),
            _ => Some(Commit::Plain(e.source, e.field.key, value)),
        }
    }
}

/// Registers the page's controls in drawing order (for the keyboard) and says which one has
/// the keyboard.
struct Ctl<'a> {
    page: &'a ConnPage,
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

/// Off | On (or a choice's values), the current one filled: a state, never a lone button.
fn segments(
    id: &str,
    options: &[(&str, bool, Action)],
    ctl: &mut Ctl<'_>,
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
        let focused = ctl.take(action);
        let a = action.clone();
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
    ctl: &mut Ctl<'_>,
    t: &Theme,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let focused = ctl.take(&action);
    div()
        .id(SharedString::from(id))
        .flex_none()
        .whitespace_nowrap()
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(if focused { t.accent } else { t.axis_line })
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

/// An empty field: "Not set", dimmed so it never reads as a saved value.
fn empty_box(t: &Theme) -> AnyElement {
    div()
        .flex_none()
        .min_w(crate::zoom::ui(160.0))
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(t.axis_line)
        .text_color(t.text_dim)
        .child("Not set")
        .into_any_element()
}

/// The environment variable that overrides this field, when it is set.
fn env_override(f: &Field) -> Option<&'static str> {
    f.env
        .filter(|v| std::env::var(v).is_ok_and(|x| !x.is_empty()))
}

/// The page: one card per source (`connections::rows`).
pub(crate) fn list(page: &ConnPage, t: &Theme, cx: &mut Context<SettingsWindow>) -> AnyElement {
    page.controls.borrow_mut().clear();
    let mut ctl = Ctl { page, next: 0 };
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
            .map(|e| {
                registry
                    .stores_of(&lines_feed::registry::source_key(&e.base.source))
                    .iter()
                    // Never wait on a feed while drawing: a store busy with a batch keeps the
                    // status last seen (none yet: connecting).
                    .map(|s| {
                        let id = std::sync::Arc::as_ptr(s) as usize;
                        match s.try_read() {
                            Some(d) => {
                                page.statuses.borrow_mut().insert(id, d.status.clone());
                                d.status.clone()
                            }
                            None => page
                                .statuses
                                .borrow()
                                .get(&id)
                                .cloned()
                                .unwrap_or(FeedStatus::Connecting),
                        }
                    })
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
                .border_2()
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
            &mut ctl,
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
                        .child(button(
                            format!("{id}-save"),
                            "Save",
                            Action::Save,
                            &mut ctl,
                            t,
                            cx,
                        ))
                        .child(button(
                            format!("{id}-cancel"),
                            "Cancel",
                            Action::Cancel,
                            &mut ctl,
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
                    let mut d = div().flex().flex_row().items_center().gap_1();
                    if let Some(var) = env_override(f) {
                        // Where the value comes from, never the value.
                        d = d.child(div().text_color(t.text_main).child(format!(
                            "Configured · environment variable {var} (overrides)"
                        )));
                    }
                    match saved {
                        None => d = d.child(field_box("…".to_owned(), false, t)),
                        Some(None) => {
                            if env_override(f).is_none() {
                                d = d.child(empty_box(t));
                            }
                            d = d.child(button(
                                format!("{id}-enter"),
                                "Enter",
                                Action::Edit(name.clone(), f.clone()),
                                &mut ctl,
                                t,
                                cx,
                            ));
                        }
                        Some(Some(masked)) => {
                            d = d
                                .child(
                                    div()
                                        .flex_none()
                                        .text_color(t.text_main)
                                        .child("Configured · keychain"),
                                )
                                .child(field_box(masked, false, t))
                                .child(button(
                                    format!("{id}-replace"),
                                    "Replace",
                                    Action::Edit(name.clone(), f.clone()),
                                    &mut ctl,
                                    t,
                                    cx,
                                ))
                                .child(div().ml_3().text_color(t.destructive).child(button(
                                    format!("{id}-remove"),
                                    if page.remove_asks(&name, f.key) {
                                        "Press again to remove"
                                    } else {
                                        "Remove"
                                    },
                                    Action::Remove(name.clone(), f.key),
                                    &mut ctl,
                                    t,
                                    cx,
                                )));
                        }
                    }
                    d.into_any_element()
                }
                (FieldKind::Text, None) => {
                    let mut d = div().flex().flex_row().gap_1().child(
                        match conn::plain_value(&plain, &name, f).filter(|v| !v.is_empty()) {
                            Some(v) => field_box(v, false, t),
                            None => empty_box(t),
                        },
                    );
                    d = d.child(button(
                        format!("{id}-edit"),
                        "Edit",
                        Action::Edit(name.clone(), f.clone()),
                        &mut ctl,
                        t,
                        cx,
                    ));
                    if let Some(var) = env_override(f) {
                        d = d.child(
                            div()
                                .text_color(t.text_dim)
                                .child(format!("{var} is set and overrides this")),
                        );
                    }
                    d.into_any_element()
                }
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
                    segments(&id, &opts, &mut ctl, t, cx)
                }
                (FieldKind::Switch, _) => {
                    let now = conn::plain_value(&plain, &name, f).as_deref() == Some("on");
                    segments(
                        &id,
                        &[
                            ("Off", !now, Action::Set(name.clone(), f.key, "off".into())),
                            ("On", now, Action::Set(name.clone(), f.key, "on".into())),
                        ],
                        &mut ctl,
                        t,
                        cx,
                    )
                }
            };
            card = card.child(line(f.label, content, t));
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
                &mut ctl,
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
    // Controls come and go (a Remove drops two): keep the keyboard on one that exists.
    let n = page.controls.borrow().len();
    if n > 0 && page.focus.get() >= n {
        page.focus.set(n - 1);
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
                    keychain: None,
                },
                buf: buf.into(),
            }),
            ..ConnPage::default()
        }
    }

    /// Decisions 0030 §4 and 0035: a typed secret goes to the keychain (trimmed); an empty one writes
    /// nothing (Remove clears a secret); a plain field writes what was typed, or clears its
    /// value when emptied.
    #[test]
    fn a_finished_field_writes_where_its_kind_belongs() {
        let mut p = edit(FieldKind::Secret, "  db-key-1234  ");
        assert_eq!(
            p.finish(),
            Some(Commit::Secret("acme".into(), "k", "db-key-1234".into()))
        );
        assert!(p.edit.is_none());
        assert_eq!(edit(FieldKind::Secret, "   ").finish(), None);
        assert_eq!(
            edit(FieldKind::Text, "GLBX").finish(),
            Some(Commit::Plain("acme".into(), "k", "GLBX".into()))
        );
        assert_eq!(
            edit(FieldKind::Text, "  ").finish(),
            Some(Commit::Clear("acme".into(), "k"))
        );
        assert_eq!(ConnPage::default().finish(), None);
    }

    /// Decisions 0030 §4 and 0035: Remove deletes a saved secret only on a second press; leaving the
    /// page drops a field being typed (its keys can't land in it unseen).
    #[test]
    fn remove_takes_two_presses_and_leaving_drops_an_edit() {
        let mut p = edit(FieldKind::Secret, "half-typed");
        assert!(!p.press_remove("acme", "k"));
        assert!(p.remove_asks("acme", "k"));
        assert!(!p.remove_asks("acme", "other"));
        assert!(p.press_remove("acme", "k"));
        assert!(!p.remove_asks("acme", "k"));
        p.leave();
        assert!(p.edit.is_none());
    }

    /// design.md §6: every control on the page is reachable from the keyboard: ↑/↓ move
    /// through them in drawing order, Enter or Space press the one with the keyboard.
    #[test]
    fn the_keyboard_reaches_every_control() {
        let mut p = ConnPage::default();
        assert_eq!(p.nav_key("down"), None, "no controls drawn: not handled");
        *p.controls.borrow_mut() = vec![
            Action::Switch("acme".into(), false),
            Action::Switch("acme".into(), true),
            Action::Test("acme".into()),
        ];
        assert_eq!(
            p.nav_key("enter"),
            Some(Some(Action::Switch("acme".into(), false)))
        );
        assert_eq!(p.nav_key("down"), Some(None));
        assert_eq!(p.nav_key("down"), Some(None));
        assert_eq!(p.nav_key("down"), Some(None), "stays on the last");
        assert_eq!(p.nav_key("space"), Some(Some(Action::Test("acme".into()))));
        assert_eq!(p.nav_key("up"), Some(None));
        assert_eq!(
            p.nav_key("enter"),
            Some(Some(Action::Switch("acme".into(), true)))
        );
        assert_eq!(p.nav_key("x"), None);
    }
}
