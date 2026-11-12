//! The Settings window (decision 0021, roadmap 6c): Ctrl+, opens one window with a search
//! box and the sections down the left under their groups (App, Panes, Trading, Data, About).
//! Every row comes from `crate::settings_schema` and its pages (`schema::page`): no setting
//! has its own form. Rows are quiet: a label and its brief description, always shown, so a
//! row's height never changes with the pointer or the keys (owner, 2026-11-12); a ● marks a setting changed from its default (on its row and its section), and ↺
//! resets it. Most changes apply at once (Data folder's after a restart; no OK button);
//! Ctrl+Z undoes. Loosening a guardrail asks first and takes effect at the next trade date
//! (the trading core's rule), tightening applies now; Accounts says so once, under its one
//! account picker.
//!
//! Keys: type to search (Esc clears it, then closes), ↑↓ settings, ←→ change a choice (or move
//! between the two numbers of one line), Space or Enter toggles, Enter edits a number (Enter
//! keeps it, Esc drops it), Tab and Shift+Tab move between sections, Alt+A and Alt+P step the
//! account and the product, Ctrl+R resets, Ctrl+Z undoes; a loosening is confirmed with Y (Esc
//! cancels). On Data folder, Enter opens Change… (or Cancel while a folder waits) and Alt+D
//! uses the default folder; in its choice M moves, S starts empty (U uses the data already
//! there). On Accounts, Enter on the prop values opens (or folds) their overrides.

use std::time::Instant;

use gpui::{
    AnyElement, App, AppContext, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Render, StatefulInteractiveElement, Styled,
    TitlebarOptions, Window, WindowHandle, WindowOptions, div, prelude::FluentBuilder, px,
};

use crate::brackets::{self, EditorKey};
use crate::display_zone;
use crate::drop_list::{DropList, Entry, Outcome};
use crate::drop_view;
use crate::fonts;
use crate::picker_view::{settings, update_settings};
use crate::search_list;
use crate::settings_schema::{
    self as schema, AccountFact, Facts, Kind, Prefs, Scope, Section, Target, Value, WorkspacePrefs,
};
use crate::theme::Theme;
use crate::workspace::Workspace;

/// The open Settings window, if any (there is only ever one).
#[derive(Default)]
pub struct SettingsWindowGlobal(pub Option<WindowHandle<SettingsWindow>>);
impl gpui::Global for SettingsWindowGlobal {}

/// An error from the data folder's choices, worded as a failure.
fn failed(e: String) -> String {
    if crate::data_dir::is_problem(&e) {
        e
    } else {
        format!("Failed: {e}")
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Tone {
    Done,
    Problem,
}

/// One change Ctrl+Z can take back: the value before, for the account it was made for. A
/// look change keeps the look as saved and this run's flags too: its undo puts both back
/// (a flag is never saved, even by an undo).
#[derive(Clone)]
struct UndoEntry {
    index: usize,
    target: Target,
    value: Value,
    look: Option<(schema::Appearance, schema::Appearance)>,
}

/// The look as saved and this run's look flags.
fn look_now(cx: &App) -> (schema::Appearance, schema::Appearance) {
    (
        settings(cx).appearance,
        cx.try_global::<Workspace>()
            .map(|w| w.env.look_flags.get())
            .unwrap_or_default(),
    )
}

/// Put one look field back as it was saved and flagged (an undo of a look change).
fn restore_look(
    field: schema::LookField,
    then: (schema::Appearance, schema::Appearance),
    cx: &mut App,
) {
    let (saved, flags) = schema::Appearance::undo_field(field, look_now(cx), then);
    update_settings(cx, move |s| s.appearance = saved);
    if let Some(w) = cx.try_global::<Workspace>() {
        w.env.look_flags.set(flags);
    }
    let t = schema::theme_for(&saved.with_flags(flags));
    cx.defer(move |cx| apply_theme(t, cx));
}

/// A new look in every window: the main windows and their panes (hidden tabs too), popped-out
/// pane windows (`workspace::set_theme`) and the Settings window. Call it outside any window's
/// update (deferred): the window that asked can't be updated while it is being updated.
fn apply_theme(t: Theme, cx: &mut App) {
    crate::workspace::set_theme(t, cx);
    if let Some(h) = cx.try_global::<SettingsWindowGlobal>().and_then(|g| g.0) {
        let _ = h.update(cx, |v, _, cx| {
            v.theme = t;
            cx.notify();
        });
    }
}

/// A window reported the OS appearance (dark or light). Under Theme: System, every window
/// changes to it through the same path as a Settings change ([`apply_theme`]); otherwise it
/// is only remembered. Screenshots never follow the OS (they stay deterministic).
pub(crate) fn os_appearance(dark: bool, cx: &mut App) {
    let frozen = cx.try_global::<Workspace>().is_none_or(|w| w.env.frozen);
    let (saved, flags) = look_now(cx);
    let look = saved.with_flags(flags);
    match schema::os_report(frozen, schema::os_dark(), dark, look.theme_choice()) {
        schema::OsReport::Ignore => {}
        schema::OsReport::Remember => {
            schema::set_os_dark(dark);
        }
        schema::OsReport::Apply => {
            schema::set_os_dark(dark);
            let t = schema::theme_for(&look);
            cx.defer(move |cx| apply_theme(t, cx));
        }
    }
}

/// Follow the OS appearance from this window (Theme: System): each window reports it, the
/// first report of a change applies it everywhere.
pub(crate) fn observe_os_appearance<V: 'static>(
    window: &mut Window,
    cx: &mut Context<V>,
) -> gpui::Subscription {
    cx.observe_window_appearance(window, |_, window, cx| {
        os_appearance(crate::workspace::is_dark(window.appearance()), cx);
    })
}

/// A risk change sent to the trading core, until it answers: a refusal puts the account's
/// saved entry back and says why on the row.
struct Awaiting {
    index: usize,
    id: lines_trading::CommandId,
    /// The entries this change wrote: (key, before, after). A refusal puts `before` back
    /// unless a later change has rewritten the entry since (its own answer decides then).
    prop: Vec<(
        String,
        Option<schema::PropRulesDoc>,
        Option<schema::PropRulesDoc>,
    )>,
    guard: Vec<(
        String,
        Option<lines_trading::Guardrails>,
        Option<lines_trading::Guardrails>,
    )>,
    /// Where its undo entry went (the undo list's length before it), if it made one.
    undo_at: Option<usize>,
}

/// A refused change's entries back as they were, each unless rewritten since (a later
/// change's own answer decides then).
fn put_back<V: PartialEq>(
    map: &mut std::collections::BTreeMap<String, V>,
    entries: Vec<(String, Option<V>, Option<V>)>,
) {
    for (k, was, wrote) in entries {
        if map.get(&k) == wrote.as_ref() {
            match was {
                Some(v) => map.insert(k, v),
                None => map.remove(&k),
            };
        }
    }
}

/// The entries of `map` that differ between `before` and `after`: (key, before, after).
fn changed_entries<V: Clone + PartialEq>(
    before: &std::collections::BTreeMap<String, V>,
    after: &std::collections::BTreeMap<String, V>,
) -> Vec<(String, Option<V>, Option<V>)> {
    before
        .keys()
        .chain(after.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|k| before.get(*k) != after.get(*k))
        .map(|k| ((*k).clone(), before.get(k).cloned(), after.get(k).cloned()))
        .collect()
}

/// A setting change waiting for the trader's yes (a guardrail loosened).
struct Pending {
    index: usize,
    value: Value,
    /// The account (and product) it was asked for: confirmed for that one only.
    target: Target,
    text: String,
    /// It is an undo's: a cancel puts the undo back; a yes adds no new undo entry.
    undo: Option<UndoEntry>,
    /// A guardrail: takes effect at the next trade date (else at once).
    tomorrow: bool,
}

/// A setting's open dropdown: its rows and its scroll (the lit row kept in view).
struct OpenDrop {
    /// The setting.
    i: usize,
    list: DropList,
    /// A multi-select's row keys (a product's symbol, an option's name), fixed when it opens:
    /// a toggle and its ✓ go by key, never by a row index into a list that may have changed.
    keys: Vec<String>,
    scroll: gpui::ScrollHandle,
    /// Frames waited for its first layout before the lit row is scrolled to (GPUI scrolls
    /// against the list's last bounds, none on its first frame); `None` once done.
    reveal_wait: Option<u8>,
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
    undo: Vec<UndoEntry>,
    editor: Option<brackets::Editor>,
    editor_msg: Option<String>,
    /// The time zone's or the UI font's searchable list, while open (`crate::search_list`).
    list_pick: Option<(List, search_list::Pick)>,
    /// Data folder: the folder picked, waiting for "Move existing data" or "Start empty".
    data_choice: Option<std::path::PathBuf>,
    /// Why the picked folder can't take a move (it already holds Lines data).
    data_holds: Option<String>,
    /// Screenshots: a trade refusal shown as if a position and orders were open.
    data_forced: Option<String>,
    /// A setting's dropdown, while open (one at a time; `crate::drop_list`).
    drop: Option<OpenDrop>,
    /// The data folder's revision last shown (a copy's progress repaints the window).
    seen_data: u64,
    /// The settings list's scroll (keeps the selected row in view).
    scroll: gpui::ScrollHandle,
    /// The window's width as of this frame (logical px; 0 before the first): where a long
    /// choice's chips go (`line_view`).
    win_w: f32,
    /// Risk changes the trading core hasn't answered yet, oldest first.
    awaiting: Vec<Awaiting>,
    /// The trading accounts' revisions last shown (the watcher repaints when they move).
    seen_revision: u64,
    /// The row last scrolled into view (only a new selection or search scrolls).
    scrolled: Option<(Section, usize, String)>,
    /// Settings › Connections (decision 0030): secrets read off-thread, a field being typed,
    /// a running Test.
    pub(crate) conn: crate::connections_view::ConnPage,
    /// Settings › Plugins (`crate::plugins_view`).
    pub(crate) plugins: crate::plugins_view::PluginPage,
    /// Settings › Keys (`crate::keys_view`).
    pub(crate) keys: crate::keys_view::KeysPage,
    /// Accounts › Prop rules: the overrides are shown (Override…).
    overrides_open: bool,
    /// Parts hidden while their guardrail is off (the cooldown without a losing streak), as
    /// of the last frame (`schema::dormant`).
    dormant: Vec<usize>,
    /// A selection waiting for the list's first layout to scroll into view, and how many
    /// frames it has waited (a few at most).
    scroll_wait: Option<((Section, usize, String), u8)>,
}

/// The window's size (logical px).
pub const SIZE: (f32, f32) = (860.0, 620.0);
/// The smallest the window can be made (logical px): every section's rows stay readable.
pub const MIN_SIZE: (f32, f32) = (640.0, 420.0);
/// A row's label and description never get narrower than this (logical px): a control that
/// doesn't fit beside them moves to its own line below, and a choice's chips wrap.
pub const LABEL_MIN_W: f32 = 240.0;
/// The section list's width (logical px).
const NAV_W: f32 = 150.0;
/// The ● slot before a row's control (every row has it: a change moves nothing).
const DOT_W: f32 = 12.0;
/// The fade at the bottom of a page longer than the window (an overflow cue).
const FADE_H: f32 = 16.0;
/// The ↺ slot (its hit target, each way).
const RESET_W: f32 = 24.0;
/// A number field's narrowest.
const NUMBER_W: f32 = 72.0;
/// A number field's narrowest between its − and + (the three as wide as the field was alone,
/// near enough: a row keeps its line).
const NUMBER_STEP_W: f32 = 40.0;
/// The tighter/looser rule, said once under the account picker.
const RULE_NOTE: &str = "Tighter applies at once; looser waits for the next session.";

#[cfg(feature = "screenshot")]
fn section_named(name: &str) -> Option<Section> {
    Section::parse(name).filter(|s| *s != Section::Plugins || has_plugins())
}

/// Whether a plugin is installed: Plugins is a section (in the sidebar, Tab, search) only then.
fn has_plugins() -> bool {
    !crate::plugin_settings::plugins().is_empty()
}

/// A search's rows: Plugins' only while a plugin is installed.
fn search_hits(query: &str) -> Vec<usize> {
    let mut hits = schema::search(query);
    if !has_plugins() {
        hits.retain(|&i| schema::all()[i].key != schema::PLUGINS_LIST);
    }
    hits
}

/// `KEY=VALUE` for `--settings-set`: the setting and its value.
#[cfg(feature = "screenshot")]
fn parse_set(s: &str) -> Result<(usize, Value, bool), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("--settings-set {s:?}: expected KEY=VALUE"))?;
    // `KEY=VALUE!`: a loosening confirmed, as with Y.
    let (v, sure) = v.strip_suffix('!').map_or((v, false), |v| (v, true));
    let i = schema::all()
        .iter()
        .position(|st| st.key == k)
        .ok_or_else(|| format!("--settings-set: no setting {k:?}"))?;
    let value = match (schema::all()[i].kind, v) {
        (Kind::Toggle, "on") => Value::Bool(true),
        (Kind::Toggle, "off") => Value::Bool(false),
        (Kind::Choice(_), n) => match n.parse() {
            Ok(k) => Value::Choice(k),
            // A typed Bar period or grouping ("charts.timeframe=750t").
            Err(_) if schema::takes_custom(k) => Value::Text(n.to_owned()),
            Err(_) => return Err(format!("{s:?}: a choice's position")),
        },
        (Kind::Number { .. }, n) => {
            Value::Number(schema::parse_number(n).map_err(|e| format!("{s:?}: {e}"))?)
        }
        (Kind::Products | Kind::Picks(_), n) => Value::List(
            n.split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_owned)
                .collect(),
        ),
        _ => return Err(format!("--settings-set {s:?}: not settable this way")),
    };
    Ok((i, value, sure))
}

/// Screenshot flags, checked before anything runs.
#[cfg(feature = "screenshot")]
pub fn check_shot(section: &str, set: &[String]) -> anyhow::Result<()> {
    if section_named(section).is_none() {
        anyhow::bail!(
            "--settings {section:?}: one of {} (or risk, trading, diagnostics, data)",
            Section::ALL.map(Section::slug).join(", ")
        );
    }
    // `account=NAME` picks the window's account (by name or id), `appearance.time_zone=list:Q`
    // (or `appearance.ui_font=list:Q`) opens that setting's list with Q typed; the rest are
    // settings.
    for s in set {
        if let Some((i, _)) = drop_open(s) {
            let st = &schema::all()[i];
            let drops = match st.kind {
                Kind::Choice(c) => schema::drops_down(c),
                Kind::Products | Kind::Picks(_) => true,
                Kind::Editor => List::of(st.key).is_some(),
                _ => false,
            };
            if !drops {
                anyhow::bail!("--settings-set {s:?}: {} has no dropdown", st.key);
            }
        }
    }
    for s in set.iter().filter_map(|s| s.strip_prefix("keys:")) {
        let known = ["bind:", "capture:", "press:", "filter:", "chip:"];
        if !known.iter().any(|k| s.starts_with(k)) {
            anyhow::bail!("--settings-set keys:{s:?}: keys:bind|capture|press|filter|chip");
        }
    }
    for s in set {
        if let Some(key) = s.strip_prefix(SELECT)
            && schema::index_of(key).is_none()
        {
            anyhow::bail!("--settings-set {s:?}: no setting {key:?}");
        }
    }
    for s in set.iter().filter(|s| {
        !s.starts_with("account=")
            && !s.starts_with("keys:")
            && !s.starts_with(SELECT)
            && List::typed(s).is_none()
            && !s.starts_with(DATA_PICK)
            && drop_open(s).is_none()
    }) {
        parse_set(s).map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

/// `--settings-set` infix after a list setting's key (`appearance.time_zone=list:Q`): its
/// list, open, with the rest typed in it.
#[cfg(feature = "screenshot")]
const LIST_OPEN: &str = "=list:";

/// `--settings-set KEY=open`: setting KEY's dropdown (or list) open; `KEY=custom:TEXT`: its
/// "Custom…" field open with TEXT typed. The setting and the text.
#[cfg(feature = "screenshot")]
fn drop_open(s: &str) -> Option<(usize, Option<&str>)> {
    if let Some(key) = s.strip_suffix("=open") {
        return Some((schema::index_of(key)?, None));
    }
    let (key, text) = s.split_once("=custom:")?;
    schema::takes_custom(key).then_some(())?;
    Some((schema::index_of(key)?, Some(text)))
}

/// `--settings-set select=KEY`: that setting's row selected, nothing changed (a row's height
/// selected and not, side by side).
#[cfg(feature = "screenshot")]
const SELECT: &str = "select=";

/// `--settings-set` prefix: the data folder picked (`data.folder=pick:PATH`, the Move existing
/// data / Start empty question), picked and refused for an open trade
/// (`data.folder=refused:PATH`), or picked and refused a move as it holds Lines data
/// (`data.folder=held:PATH`). Nothing is written.
#[cfg(feature = "screenshot")]
const DATA_PICK: &str = "data.folder=";

/// Screenshots: open the window (hidden) at `section`, with a search and changes made.
#[cfg(feature = "screenshot")]
pub fn open_for_screenshot(
    section: &str,
    search: Option<&str>,
    set: &[String],
    size: (f32, f32),
    cx: &mut App,
) -> Option<gpui::AnyWindowHandle> {
    open_sized(None, size, cx);
    let h = cx.try_global::<SettingsWindowGlobal>().and_then(|g| g.0)?;
    let section = section_named(section)?;
    let search = search.map(str::to_owned);
    let set = set.to_vec();
    let _ = h.update(cx, |v, _, cx| {
        v.section = section;
        // A plugin switch's question asked, a choice's list open (`--plugin-ask`, `--plugin-open`).
        let shot = crate::plugin_settings::shot();
        if let Some((src, key)) = shot.ask {
            v.plugins
                .press(crate::plugins_view::Action::Set(src, key, "true".into()), cx);
        }
        if let Some((src, key)) = shot.open {
            v.plugins
                .press(crate::plugins_view::Action::Open(src, key), cx);
        }
        for s in &set {
            if s.starts_with("keys:") {
                if let Err(e) = crate::keys_view::shot(&mut v.keys, s, cx) {
                    tracing::warn!("{e}");
                }
                continue;
            }
            if let Some(key) = s.strip_prefix(SELECT) {
                if let Some(i) = schema::index_of(key) {
                    v.select(i);
                }
                continue;
            }
            if let Some(name) = s.strip_prefix("account=") {
                if let Some(a) = v
                    .facts
                    .accounts
                    .iter()
                    .find(|a| a.id == name || a.name == name)
                {
                    v.target.account = Some(a.id.clone());
                    v.target.account_name = Some(a.name.clone());
                }
                continue;
            }
            if let Some((i, typed)) = drop_open(s) {
                v.select(i);
                match List::of(schema::all()[i].key) {
                    Some(list) => v.open_list(list, cx),
                    None => v.open_drop(i, cx),
                }
                if let (Some(text), Some(d)) = (typed, v.drop.as_mut()) {
                    d.list.lit = d.list.entries.len();
                    d.list.typing = Some(text.to_owned());
                }
                continue;
            }
            if let Some((list, q)) = List::typed(s) {
                if let Some(i) = schema::index_of(list.key()) {
                    v.select(i);
                    v.open_list(list, cx);
                    if let Some((_, p)) = v.list_pick.as_mut() {
                        p.type_text(q);
                    }
                }
                continue;
            }
            if let Some(rest) = s.strip_prefix(DATA_PICK)
                && let Some(i) = schema::index_of(schema::DATA_FOLDER_KEY)
            {
                v.select(i);
                if let Some(p) = rest.strip_prefix("pick:") {
                    v.data_choice = Some(p.into());
                } else if let Some(p) = rest.strip_prefix("refused:") {
                    v.data_choice = Some(p.into());
                    v.data_forced = crate::data_dir::move_refusal(1, 2);
                } else if let Some(p) = rest.strip_prefix("held:") {
                    v.data_choice = Some(p.into());
                    v.data_holds = Some(format!(
                        "Refused: {p} already holds Lines data (its store) · moving never overwrites it"
                    ));
                }
                continue;
            }
            if let Ok((i, value, sure)) = parse_set(s) {
                v.select(i);
                v.set(i, value, sure, cx);
            }
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
    open_sized(at, SIZE, cx);
}

/// [`open`] at a section's first setting (a pane menu's "Settings…").
pub fn open_section(section: schema::Section, cx: &mut App) {
    open(schema::all().iter().position(|s| s.section == section), cx);
}

/// [`open`] at a given size (screenshots; the app opens at [`SIZE`]).
fn open_sized(at: Option<usize>, size: (f32, f32), cx: &mut App) {
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
                gpui::size(px(size.0), px(size.1)),
                cx,
            ))),
            window_min_size: Some(gpui::size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
            icon: crate::window_icon(),
            ..Default::default()
        }
    } else {
        crate::workspace::hidden_window("Lines (settings)", size)
    };
    let facts = gather_facts(cx);
    // A shown window follows the core while idle (hidden ones, screenshots and tests, repaint
    // when asked).
    let watch = ws.env.track_visibility;
    let Ok(h) = cx.open_window(options, move |window, cx| {
        cx.new(|cx| {
            let mut v = SettingsWindow::new(theme, facts, cx);
            if watch {
                SettingsWindow::watch(cx);
            }
            observe_os_appearance(window, cx).detach();
            // A key being captured ends when the window loses focus (no key is left blocked).
            cx.observe_window_activation(window, |v: &mut SettingsWindow, window, cx| {
                if !window.is_window_active() && v.keys.capturing() {
                    v.keys.leave();
                    cx.notify();
                }
            })
            .detach();
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
            // The account the change lands on: the open window's, else the first. The check
            // for a loosening and the change itself use the same one.
            let target = cx
                .try_global::<SettingsWindowGlobal>()
                .and_then(|g| g.0)
                .and_then(|h| h.read(cx).ok().map(|w| w.target.clone()))
                .unwrap_or_else(|| default_target(&facts));
            let before = current_prefs(cx);
            let mut before = before;
            if st.key.starts_with("prop.") {
                schema::seed_prop_doc(&mut before, &target, &facts);
            }
            let old = (st.get)(&before, &target, &facts);
            let mut after = before.clone();
            st.apply(&mut after, &target, v.clone())?;
            // The same rule as the window's rows: the account's guardrails and prop rules.
            if schema::change_loosens(st, (&old, &v), (&before, &after), &target, &facts) {
                // A loosening needs a yes: ask in the window.
                let i = c.index as usize;
                open(Some(i), cx);
                if let Some(h) = cx.try_global::<SettingsWindowGlobal>().and_then(|g| g.0) {
                    let _ = h.update(cx, |w, _, cx| w.set(i, v, false, cx));
                }
                return Ok(None);
            }
            let _ = commit(&before, &after, &target, cx);
            Ok(Some(format!("{}: {}", st.name, st.value_text(&v))))
        }
        None => {
            let i = c.index as usize;
            open(Some(i), cx);
            if let Some(list) = List::of(st.key)
                && let Some(h) = cx.try_global::<SettingsWindowGlobal>().and_then(|g| g.0)
            {
                // The time zone's and the UI font's palette commands open the list, ready to
                // type in.
                let _ = h.update(cx, |w, _, cx| {
                    w.open_list(list, cx);
                    cx.notify();
                });
            }
            Ok(None)
        }
    }
}

/// The window's view of every value, from the live app and workspace.
fn current_prefs(cx: &App) -> Prefs {
    let ws = cx.try_global::<Workspace>();
    let mut app = settings(cx).clone();
    // As shown: this run's look flags over what is saved.
    if let Some(w) = ws {
        app.appearance = app.appearance.with_flags(w.env.look_flags.get());
    }
    Prefs {
        app,
        ws: WorkspacePrefs {
            latency_hud: ws.is_some_and(|w| w.show_latency_hud),
        },
    }
}

/// Write `after` where it lives and apply its effects (the look, the trading core's
/// guardrails, P&L text, the workspace's preferences).
fn commit(
    before: &Prefs,
    after: &Prefs,
    target: &Target,
    cx: &mut App,
) -> Option<lines_trading::CommandId> {
    let mut sent = None;
    // The look as saved and this run's flags: only the fields changed now are saved, and a
    // field the trader set drops its flag (flags are never saved).
    let flags = cx
        .try_global::<Workspace>()
        .map(|w| w.env.look_flags.get())
        .unwrap_or_default();
    let (saved_look, flags) = schema::Appearance::changed(
        settings(cx).appearance,
        flags,
        before.app.appearance,
        after.app.appearance,
    );
    if after.app != before.app {
        let mut app = after.app.clone();
        app.appearance = saved_look;
        update_settings(cx, move |s| *s = app);
    }
    // The size an open pane shows and sends: every pane redraws its buttons now, so none says
    // an old size while a click would send the new one.
    if after.app.new_panes.default_qty != before.app.new_panes.default_qty {
        crate::trade_bar::size_changed();
    }
    if after.app.ui_zoom != before.app.ui_zoom
        && crate::zoom::set(after.app.ui_zoom.unwrap_or(crate::zoom::DEFAULT))
    {
        cx.defer(crate::workspace::applied_ui_zoom);
    }
    if let Some(w) = cx.try_global::<Workspace>() {
        w.env.look_flags.set(flags);
    }
    if after.app.appearance != before.app.appearance {
        // After the current update: the window that asked (the Settings window, or a main
        // window running a palette command) can't be updated while it is being updated.
        let t = schema::theme_for(&saved_look.with_flags(flags));
        cx.defer(move |cx| apply_theme(t, cx));
    }
    if after.app.time_zone != before.app.time_zone {
        let problem = crate::display_zone::apply(&lines_core::display_tz::ZoneChoice::from_saved(
            after.app.time_zone.as_deref(),
        ));
        if let Some(p) = problem {
            crate::workspace::notice(p, cx);
        }
        cx.refresh_windows();
    }
    if after.app.ui_font != before.app.ui_font {
        // Live in every window: text shaped in the old family is shaped again (`fonts`).
        if let Some(p) = fonts::apply(&fonts::UiFont::from_saved(after.app.ui_font.as_deref())) {
            crate::workspace::notice(p, cx);
        }
        cx.refresh_windows();
    }
    if after.app.hide_join_buttons != before.app.hide_join_buttons {
        // The DOMs' trade panels lay out again (the Join row is one of their rows).
        crate::pnl::display_changed();
        cx.refresh_windows();
    }
    if after.app.pnl_display != before.app.pnl_display {
        crate::pnl::display_changed();
        crate::journal_shots::set_pnl(after.app.pnl_display);
        cx.refresh_windows();
    }
    if after.ws.latency_hud != before.ws.latency_hud {
        crate::workspace::set_show_latency_hud(after.ws.latency_hud, cx);
    }
    if after.app.prop_rules != before.app.prop_rules
        && let (Some(id), Some(t)) = (&target.account, crate::trading::Trading::try_global(cx))
        && let Some(doc) = [&target.account_name, &target.account]
            .into_iter()
            .flatten()
            .find_map(|k| after.app.prop_rules.get(k))
        && let Ok(rules) = doc.resolve()
    {
        sent = Some(t.submit(
            lines_trading::Source::Api,
            Instant::now(),
            lines_trading::Command::SetRules {
                account: lines_trading::AccountId::new(id),
                rules,
            },
        ));
    }
    if after.app.max_order_qty != before.app.max_order_qty
        && let (Some(id), Some(t)) = (&target.account, crate::trading::Trading::try_global(cx))
    {
        // Paper accounts only (a broker's own limits are the broker's); the next order and
        // resize are checked against it.
        let account = lines_trading::AccountId::new(id);
        if t.account(&account).is_some_and(|a| a.paper) {
            sent = Some(t.submit(
                lines_trading::Source::Api,
                Instant::now(),
                lines_trading::Command::SetMaxOrderQty {
                    account,
                    max: target.max_order(after),
                },
            ));
        } else {
            // Never saved for a broker account: nothing would enforce it.
            let keys = [target.account_name.clone(), target.account.clone()];
            crate::picker_view::update_settings(cx, |s| {
                for k in keys.iter().flatten() {
                    s.max_order_qty.remove(k);
                }
            });
            crate::workspace::notice(
                "Largest order is for paper accounts: a broker account keeps the broker's limits"
                    .to_owned(),
                cx,
            );
        }
    }
    if after.app.guardrails != before.app.guardrails
        && let (Some(id), Some(t)) = (&target.account, crate::trading::Trading::try_global(cx))
    {
        // Saved under the name or the id (as startup reads them): the entry just edited.
        sent = Some(t.submit(
            lines_trading::Source::Api,
            Instant::now(),
            lines_trading::Command::SetGuardrails {
                account: lines_trading::AccountId::new(id),
                guardrails: target.guardrails(after),
            },
        ));
    }
    sent
}

/// The facts for the window, waiting for the trading cache (off the paint path).
fn gather_facts(cx: &App) -> Facts {
    facts_now(cx, true).unwrap_or_default()
}

/// The facts as of now; with `wait` false (paint), `None` when the trading cache is busy.
fn facts_now(cx: &App, wait: bool) -> Option<Facts> {
    let mut f = Facts {
        folder: crate::data_dir::facts(),
        ..Facts::default()
    };
    (f.store_start, f.store_flags) = crate::data_dir::store_at_start();
    f.look_flags = cx
        .try_global::<Workspace>()
        .map(|w| w.env.look_flags.get())
        .unwrap_or_default();
    f.version = env!("CARGO_PKG_VERSION").to_owned();
    f.can_update =
        crate::update::verify::PUBLIC_KEY.is_some() && crate::update::manifest::supported();
    f.rollback = crate::persist::config_dir().and_then(|d| crate::update::rollback_target(&d));
    f.config_dir = crate::persist::config_dir().map_or_else(
        || "None (nothing is saved)".to_owned(),
        |d| d.display().to_string(),
    );
    if let Some(t) = crate::trading::Trading::try_global(cx) {
        f.accounts = t
            .accounts()
            .iter()
            .map(|a| AccountFact {
                id: a.id.0.clone(),
                name: a.name.clone(),
                paper: a.paper,
                kind: a.badge,
                rules: a.rules.as_ref().map(|r| r.name.clone()),
                ..Default::default()
            })
            .collect();
        // What the core reports now: rules and guardrails in force, what waits, lockouts.
        // Paint never waits on the trading thread: a busy cache keeps last frame's facts.
        let cache = if wait {
            t.cache.read()
        } else {
            t.cache.try_read()?
        };
        for fa in &mut f.accounts {
            let id = lines_trading::AccountId::new(&fa.id);
            if let Some(v) = cache.account(&id) {
                fa.rules_in_force = v.limits.rules_in_force.clone();
                fa.rules_pending = v.limits.rules_pending.clone();
                fa.guard_in_force = v.limits.guardrails_in_force.clone();
                fa.guard_pending = v.limits.guardrails_pending.clone();
                fa.account_size = Some(v.limits.account_size).filter(|s| *s > 0);
                fa.breach = v.blocked.map(|b| {
                    let what = match b {
                        lines_trading::rules::Breach::TrailingDrawdown => {
                            "the trailing drawdown was reached (final)"
                        }
                        lines_trading::rules::Breach::DailyLoss => {
                            "the daily loss limit was reached (until the next session)"
                        }
                    };
                    let can = match v.trading {
                        lines_trading::rules::TradingState::Halted => "no new orders",
                        lines_trading::rules::TradingState::ReducingOnly => {
                            "orders may only close positions"
                        }
                        lines_trading::rules::TradingState::Active => "orders are allowed",
                    };
                    format!("{what}; {can}")
                });
            }
            fa.lock = cache.guard(&id).map(|g| {
                g.reason
                    .clone()
                    .unwrap_or_else(|| "a guardrail is in force".to_owned())
            });
        }
    }
    Some(f)
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
            list_pick: None,
            data_choice: None,
            data_holds: None,
            data_forced: None,
            drop: None,
            seen_data: 0,
            scroll: gpui::ScrollHandle::new(),
            win_w: 0.0,
            scrolled: None,
            awaiting: Vec::new(),
            conn: Default::default(),
            plugins: Default::default(),
            keys: Default::default(),
            seen_revision: 0,
            overrides_open: false,
            dormant: Vec::new(),
            scroll_wait: None,
        }
    }

    /// The trading accounts' revisions, summed (lock-free).
    fn trading_revision(cx: &App) -> u64 {
        crate::trading::Trading::try_global(cx).map_or(0, |t| {
            t.accounts()
                .iter()
                .map(|a| t.cache.revision(&a.id))
                .fold(0u64, u64::wrapping_add)
        })
    }

    /// Repaint while open when the core answers a change or an account moves (a lockout, a
    /// breach, rules coming into force), so the rows never show a stale state while idle.
    fn watch(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(200))
                    .await;
                let alive = this.update(cx, |v, cx| {
                    v.check_answers(cx);
                    if v.conn.tick() {
                        cx.notify();
                    }
                    let r = Self::trading_revision(cx);
                    let d = crate::data_dir::revision();
                    if r != v.seen_revision || d != v.seen_data {
                        v.seen_revision = r;
                        v.seen_data = d;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// The core's answers to risk changes sent from here: a refusal puts the saved entry
    /// back (unless rewritten since), drops its undo entry and says why on the row.
    fn check_answers(&mut self, cx: &mut Context<Self>) {
        if self.awaiting.is_empty() {
            return;
        }
        let answers: Vec<Option<Result<Vec<lines_trading::OrderId>, lines_trading::Reject>>> =
            match crate::trading::Trading::try_global(cx).and_then(|t| t.cache.try_read()) {
                Some(c) => self
                    .awaiting
                    .iter()
                    .map(|w| c.result(w.id).cloned())
                    .collect(),
                None => return,
            };
        let mut waiting = Vec::new();
        let mut refused = false;
        for (w, answer) in std::mem::take(&mut self.awaiting).into_iter().zip(answers) {
            let Some(result) = answer else {
                waiting.push(w);
                continue;
            };
            let Err(r) = result else { continue };
            refused = true;
            let (prop, guard) = (w.prop.clone(), w.guard.clone());
            update_settings(cx, move |s| {
                put_back(&mut s.prop_rules, prop);
                put_back(&mut s.guardrails, guard);
            });
            if let Some(at) = w.undo_at
                && self
                    .undo
                    .get(at)
                    .is_some_and(|u| u.index == w.index && u.target == self.target)
            {
                self.undo.remove(at);
            }
            self.note = Some((
                w.index,
                format!("Not changed: {}", r.message),
                Tone::Problem,
            ));
        }
        self.awaiting = waiting;
        if refused {
            cx.notify();
        }
    }

    /// Leave whatever is half done (a number being typed, a confirmation, the preset
    /// editor): the account, product or section changed under it.
    fn drop_unfinished(&mut self) {
        self.conn.leave();
        self.keys.leave();
        self.plugins.leave();
        self.edit = None;
        self.cancel_pending();
        self.editor = None;
        self.editor_msg = None;
        self.list_pick = None;
        self.drop = None;
        self.data_choice = None;
        self.data_holds = None;
    }

    /// Show setting `i`: its section, selected, the search cleared (a prop override unfolds).
    fn show(&mut self, i: usize) {
        if let Some(st) = schema::all().get(i) {
            self.drop_unfinished();
            self.query.clear();
            self.section = st.section;
            if schema::PROP_OVERRIDES.contains(&st.key) {
                self.overrides_open = true;
            }
            self.row = self.rows().iter().position(|&r| r == i).unwrap_or(0);
        }
    }

    /// Whether setting `i` is shown as a line on its page (the prop overrides only while
    /// unfolded; never a source's switch, which is on its card).
    fn on_page(&self, i: usize) -> bool {
        let key = schema::all()[i].key;
        // The sources' switches are drawn on their Connections cards.
        !schema::SOURCE_SWITCHES.contains(&key)
            && !self.dormant.contains(&i)
            && (self.overrides_open || !schema::PROP_OVERRIDES.contains(&key))
    }

    /// The settings shown, in the order ↑↓ walk them: the search's matches, else the
    /// section's page, line by line (a line of two settings has both, left to right).
    fn rows(&self) -> Vec<usize> {
        if self.query.trim().is_empty() {
            schema::page(self.section)
                .iter()
                .flat_map(|g| g.lines.iter())
                .flat_map(|l| l.keys())
                .filter_map(schema::index_of)
                .filter(|&i| self.on_page(i))
                .collect()
        } else {
            search_hits(&self.query)
        }
    }

    /// Change the section shown (the sidebar, Tab): the search cleared, the first row.
    fn go_to(&mut self, s: Section) {
        self.section = s;
        self.overrides_open = false;
        self.query.clear();
        self.row = 0;
        self.drop_unfinished();
    }

    /// ←→ on a line of two numbers ("after 3 losses, for 15 min"): the other field.
    fn step_field(&mut self, i: usize, right: bool) -> bool {
        if !self.query.trim().is_empty() {
            return false;
        }
        let Some(p) = schema::places()[i] else {
            return false;
        };
        let keys: Vec<usize> = p.line.keys().filter_map(schema::index_of).collect();
        let Some(k) = keys.iter().position(|&x| x == i) else {
            return false;
        };
        let next = if right {
            keys.get(k + 1)
        } else {
            k.checked_sub(1).and_then(|k| keys.get(k))
        };
        match next {
            Some(&n) => {
                self.select(n);
                true
            }
            None => false,
        }
    }

    fn selected(&self) -> Option<usize> {
        self.rows().get(self.row).copied()
    }

    /// Set setting `i` to `v` (validated; a loosened guardrail asks first unless `sure`).
    fn set(&mut self, i: usize, v: Value, sure: bool, cx: &mut Context<Self>) {
        let st = &schema::all()[i];
        // A flag rules it this run: shown dimmed, not changed here.
        if schema::flag_value(st, &self.facts).is_some() {
            return;
        }
        let mut before = current_prefs(cx);
        // A prop-rule change on an account without a settings entry builds on its rules in
        // force (not saved unless the change is).
        if st.key.starts_with("prop.") {
            schema::seed_prop_doc(&mut before, &self.target, &self.facts);
        }
        let old = (st.get)(&before, &self.target, &self.facts);
        if old == v && !st.key.starts_with("guard.") {
            // The value shown already: nothing to change (nor to save over).
            return;
        }
        let mut after = before.clone();
        if let Err(e) = st.apply(&mut after, &self.target, v.clone()) {
            self.note = Some((i, e.to_owned(), Tone::Problem));
            cx.notify();
            return;
        }
        // Whatever was waiting for a yes is dropped (an undo's goes back on the list).
        self.cancel_pending();
        let guard = st.key.starts_with("guard.");
        let prop = st.key.starts_with("prop.");
        let loosens =
            schema::change_loosens(st, (&old, &v), (&before, &after), &self.target, &self.facts);
        // A preset for another account size is refused here, with the reason (the core
        // refuses it too).
        if st.key == "prop.preset"
            && let Value::Choice(k) = v
            && k > 0
            && let Some(size) = self
                .facts
                .accounts
                .iter()
                .find(|a| Some(&a.id) == self.target.account.as_ref())
                .and_then(|a| a.account_size)
            && lines_trading::rules::presets()
                .get(k - 1)
                .is_some_and(|p| p.account_size != size)
        {
            self.note = Some((
                i,
                format!(
                    "This account is ${}: choose a preset for that size",
                    schema::grouped(size)
                ),
                Tone::Problem,
            ));
            cx.notify();
            return;
        }
        if loosens && !sure {
            let (from, to) = (
                st.value_text(&old),
                schema::shown_after(st, &after, &self.target, &self.facts),
            );
            self.pending = Some(Pending {
                index: i,
                value: v.clone(),
                target: self.target.clone(),
                text: if guard || prop {
                    format!(
                        "Loosening {} for {} ({from} → {to}) takes effect at the next session.",
                        st.name.to_lowercase(),
                        self.target
                            .account_name
                            .as_deref()
                            .unwrap_or("this account")
                    )
                } else {
                    format!("This loosens a trading safeguard ({from} → {to}): it applies now.")
                },
                undo: None,
                tomorrow: guard || prop,
            });
            self.note = None;
            cx.notify();
            return;
        }
        let look = schema::LookField::of(st.key).map(|_| look_now(cx));
        let saved = settings(cx);
        let (prop_wrote, guard_wrote) = (
            changed_entries(&saved.prop_rules, &after.app.prop_rules),
            changed_entries(&saved.guardrails, &after.app.guardrails),
        );
        if let Some(id) = commit(&before, &after, &self.target, cx) {
            self.awaiting.push(Awaiting {
                index: i,
                id,
                prop: prop_wrote,
                guard: guard_wrote,
                undo_at: Some(self.undo.len()),
            });
        }
        self.undo.push(UndoEntry {
            index: i,
            target: self.target.clone(),
            value: old.clone(),
            look,
        });
        let to = schema::shown_after(st, &after, &self.target, &self.facts);
        let text = if guard {
            if loosens {
                format!(
                    "Loosened {} → {to}: takes effect at the next session.",
                    st.value_text(&old),
                )
            } else {
                format!(
                    "{} {} → {to}: applies now.",
                    if old == v { "Kept" } else { "Tightened" },
                    st.value_text(&old),
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
        let n = self.undo.len();
        self.set(p.index, p.value, true, cx);
        if p.undo.is_some() {
            // An undo's yes: it was undone, not a new change to undo.
            self.undo.truncate(n);
            self.no_undo_entry(n);
        }
        self.target = keep;
    }

    /// No to the pending loosening (an undo's goes back on the undo list).
    fn cancel_pending(&mut self) {
        if let Some(p) = self.pending.take()
            && let Some(u) = p.undo
        {
            self.undo.push(u);
        }
        self.note = None;
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.undo.pop() else {
            self.note = None;
            return;
        };
        let i = entry.index;
        // The setting it undoes, for its account: what is undone (or asked) is in view.
        self.show(i);
        if let (Some(then), Some(field)) = (entry.look, schema::LookField::of(schema::all()[i].key))
        {
            // A look change: that field as saved and flagged before it, nothing else.
            restore_look(field, then, cx);
            self.note = Some((i, "Undone".to_owned(), Tone::Done));
            cx.notify();
            return;
        }
        self.target = entry.target.clone();
        let n = self.undo.len();
        // An undo that loosens asks like any other change.
        self.set(i, entry.value.clone(), false, cx);
        self.undo.truncate(n);
        self.no_undo_entry(n);
        if let Some(p) = &mut self.pending {
            p.undo = Some(entry);
        } else if let Some(n) = &mut self.note {
            n.1 = format!("Undone · {}", n.1);
        }
    }

    /// The change just sent (an undo's) made no undo entry: a refusal removes none.
    fn no_undo_entry(&mut self, at: usize) {
        if let Some(w) = self.awaiting.last_mut()
            && w.undo_at == Some(at)
        {
            w.undo_at = None;
        }
    }

    /// ↺ / Ctrl+R: back to the value a reset gives (a loosening asks first, as any change).
    fn reset(&mut self, i: usize, cx: &mut Context<Self>) {
        let st = &schema::all()[i];
        let prefs = current_prefs(cx);
        if !schema::marks(st, &prefs, &self.target, &self.facts).1 {
            return;
        }
        if let Some(d) = st.reset_value(&prefs, &self.target) {
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
        self.overrides_open = false;
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

    /// Open a setting's list, on the choice in force.
    fn open_list(&mut self, list: List, cx: &App) {
        let s = settings(cx);
        let pick = match list {
            List::Zone => display_zone::pick(&lines_core::display_tz::ZoneChoice::from_saved(
                s.time_zone.as_deref(),
            )),
            List::Font => fonts::pick(&fonts::UiFont::from_saved(s.ui_font.as_deref())),
        };
        self.list_pick = Some((list, pick));
    }

    /// A key while a list is open: it filters, moves, chooses or closes.
    fn list_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let Some((list, p)) = self.list_pick.as_mut() else {
            return;
        };
        let list = *list;
        match p.key(k.key.as_str(), k.key_char.as_deref()) {
            search_list::PickKey::Typing | search_list::PickKey::Ignored => {}
            search_list::PickKey::Cancel => self.list_pick = None,
            search_list::PickKey::Choose(k) => self.choose(list, k, cx),
        }
        cx.notify();
    }

    /// Set the list's setting to its choice `k`, and close the list.
    fn choose(&mut self, list: List, k: usize, cx: &mut Context<Self>) {
        self.list_pick = None;
        if let Some(i) = schema::index_of(list.key()) {
            self.set(i, Value::Text(list.value(k)), false, cx);
        }
    }

    /// Setting `i`'s dropdown rows for its value `cur`, lit at the value in force (a choice of
    /// five or more or with long names, a timeframe or grouping with "Custom…", products,
    /// columns); `None`: the setting has buttons or a field instead.
    fn drop_rows(i: usize, cur: &Value, prefs: &Prefs) -> Option<DropList> {
        let st = &schema::all()[i];
        let a = &prefs.app.appearance;
        Some(match (st.kind, cur) {
            (Kind::Choice(c), cur) if schema::drops_down(c) => {
                let rows = c
                    .iter()
                    .enumerate()
                    .map(|(j, n)| {
                        let label = schema::choice_label(st.key, j, n, a).into_owned();
                        match schema::choice_group(st.key, n) {
                            Some(g) => Entry::grouped(label, g),
                            None => Entry::new(label),
                        }
                    })
                    .collect();
                let custom = schema::takes_custom(st.key);
                let at = match cur {
                    Value::Choice(k) => Some(*k),
                    // A typed value: "Custom…" is lit.
                    _ if custom => Some(c.len()),
                    _ => None,
                };
                DropList::new(rows, false, custom, at)
            }
            (Kind::Products, Value::List(l)) => DropList::new(
                schema::depth_choices(l)
                    .into_iter()
                    .map(|(sym, known)| {
                        Entry::new(if known {
                            sym
                        } else {
                            format!("{sym} (unavailable)")
                        })
                    })
                    .collect(),
                true,
                false,
                None,
            ),
            (Kind::Picks(names), _) => DropList::new(
                names.iter().map(|n| Entry::new(*n)).collect(),
                true,
                false,
                None,
            ),
            _ => return None,
        })
    }

    /// A multi-select's row keys for its value `cur` (each product's symbol, each option's
    /// name), row for row with [`Self::drop_rows`]; empty for a one-pick list.
    fn drop_keys(i: usize, cur: &Value) -> Vec<String> {
        match (schema::all()[i].kind, cur) {
            (Kind::Products, Value::List(l)) => schema::depth_choices(l)
                .into_iter()
                .map(|(s, _)| s)
                .collect(),
            (Kind::Picks(names), _) => names.iter().map(|n| (*n).to_owned()).collect(),
            _ => Vec::new(),
        }
    }

    /// Whether row `k` of a dropdown is the value in force (one pick, `cur` a choice) or on
    /// (several: its key, from `keys`, is in `cur`).
    fn drop_checked(cur: &Value, keys: &[String], k: usize) -> bool {
        match cur {
            Value::Choice(j) => *j == k,
            Value::List(l) => keys
                .get(k)
                .is_some_and(|key| l.iter().any(|x| x.eq_ignore_ascii_case(key))),
            _ => false,
        }
    }

    /// Open setting `i`'s dropdown (closing any other list).
    fn open_drop(&mut self, i: usize, cx: &App) {
        let prefs = current_prefs(cx);
        let cur = (schema::all()[i].get)(&prefs, &self.target, &self.facts);
        if let Some(list) = Self::drop_rows(i, &cur, &prefs) {
            // One thing at a time: a question waiting (a loosening's Y, the data folder's Move
            // or Start empty) is left, as a click elsewhere leaves it.
            self.list_pick = None;
            self.edit = None;
            self.cancel_pending();
            self.data_choice = None;
            self.data_holds = None;
            self.drop = Some(OpenDrop {
                i,
                list,
                keys: Self::drop_keys(i, &cur),
                scroll: gpui::ScrollHandle::new(),
                reveal_wait: Some(0),
            });
        }
    }

    /// What a key or a click in the open dropdown did.
    fn drop_outcome(&mut self, o: Outcome, cx: &mut Context<Self>) {
        let Some(d) = self.drop.as_mut() else {
            return;
        };
        let i = d.i;
        let st = &schema::all()[i];
        match o {
            Outcome::None => drop_view::reveal(&d.scroll, &d.list, false),
            Outcome::Close => self.drop = None,
            Outcome::Pick(k) => {
                self.drop = None;
                self.set(i, Value::Choice(k), false, cx);
            }
            // Refused (not a timeframe, out of range): the field stays, with what was typed,
            // and the row says why.
            Outcome::Submit(text) => {
                let mut p = current_prefs(cx);
                match st.apply(&mut p, &self.target, Value::Text(text.clone())) {
                    Ok(()) => {
                        self.drop = None;
                        self.set(i, Value::Text(text), false, cx);
                    }
                    Err(e) => self.note = Some((i, e.to_owned(), Tone::Problem)),
                }
            }
            Outcome::Toggle(k) => {
                d.list.lit = k;
                let key = d.keys.get(k).cloned();
                let cur = (st.get)(&current_prefs(cx), &self.target, &self.facts);
                let next = match (st.kind, cur, key) {
                    (Kind::Products, Value::List(l), Some(sym)) => Some(schema::toggled(&l, &sym)),
                    // In the options' order; `key` flips, the rest stay.
                    (Kind::Picks(names), Value::List(l), Some(key)) => Some(
                        names
                            .iter()
                            .filter(|m| (**m == key) != l.iter().any(|x| x == *m))
                            .map(|m| (*m).to_owned())
                            .collect(),
                    ),
                    _ => None,
                };
                if let Some(next) = next {
                    self.set(i, Value::List(next), false, cx);
                }
            }
        }
    }

    /// A key while a dropdown is open: it moves, jumps, picks, toggles or closes.
    fn drop_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let Some(d) = self.drop.as_mut() else {
            return;
        };
        let o = d
            .list
            .key(k.key.as_str(), k.key_char.as_deref(), Instant::now());
        self.drop_outcome(o, cx);
        cx.notify();
    }

    /// The editor a setting of kind `Editor` opens.
    fn open_editor_for(&mut self, i: usize, cx: &App) {
        if let Some(list) = List::of(schema::all()[i].key) {
            self.open_list(list, cx);
        } else {
            self.open_editor(cx);
        }
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
        // Settings › Keys: a key being captured takes every key first (it never runs a
        // command, not even the zoom's or Flatten); the page finds and moves otherwise.
        if (self.keys.capturing()
            || (self.section == Section::Keys && self.query.trim().is_empty()))
            && self.keys.key(ev, cx)
        {
            cx.notify();
            return;
        }
        if self.editor.is_some() {
            self.editor_key(ev, cx);
            return;
        }
        // Settings › Plugins: its fields, lists, questions and controls take the keys (a
        // value being typed or a list open takes them on a search's results too).
        if ((self.section == Section::Plugins && self.query.trim().is_empty())
            || self.plugins.edit.is_some()
            || self.plugins.open.is_some())
            && self.plugins.key(ev, cx)
        {
            cx.notify();
            return;
        }
        // A source's field being typed takes every key (Enter saves, Esc cancels).
        if self.conn.edit.is_some()
            && self.section == Section::Connections
            && self.query.trim().is_empty()
        {
            if let Some(c) = self.conn.key(ev, cx) {
                self.conn_commit(c, cx);
            }
            cx.notify();
            return;
        }
        // On Connections, ↑/↓ move between the cards' controls and Enter or Space press one
        // (design.md §6: every action reachable from the keyboard).
        if self.section == Section::Connections
            && self.query.trim().is_empty()
            && self.pending.is_none()
            && !ev.keystroke.modifiers.control
            && !ev.keystroke.modifiers.alt
            && let Some(pressed) = self.conn.nav_key(ev.keystroke.key.as_str())
        {
            if let Some(a) = pressed {
                self.conn_action(a, cx);
            }
            cx.notify();
            return;
        }
        if self.drop.is_some() {
            self.drop_key(ev, cx);
            return;
        }
        if self.list_pick.is_some() {
            self.list_key(ev, cx);
            return;
        }
        // The UI zoom is app-wide: its keys work here too (not while a number is typed).
        if self.edit.is_none()
            && let Some(
                c @ (crate::commands::Command::ZoomIn
                | crate::commands::Command::ZoomOut
                | crate::commands::Command::ZoomReset),
            ) = crate::commands::command_for(&ev.keystroke)
        {
            cx.defer(move |cx| crate::workspace::zoom_command(c, cx));
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
        if let (Some(_), Some(i)) = (&self.data_choice, sel) {
            let plain = !m.control && !m.alt && !m.shift && !m.platform;
            match key {
                "m" if plain => self.data_choose(i, true, cx),
                // Start empty, or (the folder already holds Lines data) Use its data.
                "s" if plain && self.data_holds.is_none() => self.data_choose(i, false, cx),
                "u" if plain && self.data_holds.is_some() => self.data_choose(i, false, cx),
                "escape" => {
                    self.data_choice = None;
                    self.data_holds = None;
                    self.note = None;
                }
                _ => {}
            }
            cx.notify();
            return;
        }
        if self.pending.is_some() {
            match key {
                // Y, not Enter: an Enter typed out of habit after a number never says yes.
                "y" if !m.control && !m.alt && !m.shift && !m.platform => self.confirm(cx),
                "escape" => self.cancel_pending(),
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
            // Settings › Data folder: Use the default folder (its button's key).
            ("d", false, false, true) => {
                let f = &self.facts.folder;
                if let Some(i) = sel.filter(|&i| schema::all()[i].key == schema::DATA_FOLDER_KEY)
                    && f.can_use_default()
                {
                    self.data_pick(i, true, cx);
                }
            }
            ("p", false, false, true) => self.step_product(),
            ("up", ..) => self.row = self.row.saturating_sub(1),
            ("down", ..) => self.row = (self.row + 1).min(rows.len().saturating_sub(1)),
            ("tab", false, shift, false) => {
                // The sidebar's sections (Plugins only while one is installed).
                let all: Vec<Section> = Section::ALL
                    .into_iter()
                    .filter(|s| {
                        *s != Section::Plugins || !crate::plugin_settings::plugins().is_empty()
                    })
                    .collect();
                let k = all.iter().position(|s| *s == self.section).unwrap_or(0);
                let n = all.len();
                self.go_to(all[if shift { (k + n - 1) % n } else { (k + 1) % n }]);
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
                        // A line of two numbers: the other one.
                        (Kind::Number { .. }, _) => {
                            self.step_field(i, key == "right");
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
        if (st.read_only() && !matches!(kind, Kind::Editor))
            || schema::flag_value(st, &self.facts).is_some()
        {
            return;
        }
        let cur = (st.get)(&current_prefs(cx), &self.target, &self.facts);
        match (kind, cur) {
            (Kind::Toggle, Value::Bool(b)) => self.set(i, Value::Bool(!b), false, cx),
            (Kind::Choice(c), _) if schema::drops_down(c) => self.open_drop(i, cx),
            (Kind::Products | Kind::Picks(_), _) => self.open_drop(i, cx),
            (Kind::Choice(c), Value::Choice(k)) => {
                self.set(i, Value::Choice((k + 1) % c.len()), false, cx)
            }
            (Kind::Number { .. }, Value::Number(n)) => {
                self.edit = Some(n.map(|n| n.to_string()).unwrap_or_default());
            }
            (Kind::Editor, _) if st.key == schema::RESET_KEY => self.reset_press(i, false, cx),
            (Kind::Editor, _) if st.key == schema::ROLLBACK_KEY => self.rollback_press(cx),
            (Kind::Editor, _) if st.key == schema::PROP_OVERRIDES_KEY => {
                self.overrides_open = !self.overrides_open;
            }
            (Kind::Editor, _) if st.key == schema::DATA_FOLDER_KEY => {
                if crate::data_dir::facts().pending() {
                    self.data_cancel(i, cx);
                } else {
                    self.data_pick(i, false, cx);
                }
            }
            (Kind::Editor, _) => self.open_editor_for(i, cx),
            _ => {}
        }
    }

    // ---- the data folder ----------------------------------------------------------------

    /// "Change…" (a folder dialog) or "Use the default folder" (`default`): the folder is
    /// checked off the UI thread, then the row asks Move existing data / Start empty.
    fn data_pick(&mut self, i: usize, default: bool, cx: &mut Context<Self>) {
        let f = crate::data_dir::facts();
        let (Some(in_use), true) = (f.in_use.clone(), f.changeable()) else {
            return;
        };
        if f.pending() {
            return;
        }
        self.data_choice = None;
        self.data_holds = None;
        self.note = None;
        let os_default = f.os_default.clone();
        let config = crate::persist::config_dir();
        let chosen: Option<std::path::PathBuf> = if default { os_default.clone() } else { None };
        let dialog = (!default).then(|| {
            cx.prompt_for_paths(gpui::PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some("Use as data folder".into()),
            })
        });
        cx.spawn(async move |this, cx| {
            let picked = match (chosen, dialog) {
                (Some(d), _) => Ok(d),
                (None, Some(rx)) => match rx.await {
                    Ok(Ok(Some(paths))) => match paths.into_iter().next() {
                        Some(p) => Ok(p),
                        None => return,
                    },
                    Ok(Ok(None)) => return,
                    _ => Err("No folder dialog on this system".to_owned()),
                },
                (None, None) => return,
            };
            let checked = match picked {
                Ok(p) => {
                    cx.background_executor()
                        .spawn(async move {
                            if default {
                                std::fs::create_dir_all(&p)
                                    .map_err(|e| format!("can't make {}: {e}", p.display()))?;
                            }
                            let holds = crate::data_dir::check_pick(
                                &in_use,
                                &p,
                                config.as_deref(),
                                os_default.as_deref(),
                            )?;
                            Ok::<_, String>((p, holds))
                        })
                        .await
                }
                Err(e) => Err(e),
            };
            let _ = this.update(cx, |v, cx| {
                match checked {
                    Ok((p, holds)) => {
                        v.data_choice = Some(p);
                        v.data_holds = holds;
                    }
                    Err(e) => v.note = Some((i, format!("Refused: {e}"), Tone::Problem)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Why nothing can be chosen now: a position or a working order is open.
    fn trade_refusal(&self, cx: &App) -> Option<String> {
        if let Some(r) = &self.data_forced {
            return Some(r.clone());
        }
        // Busy this instant: no refusal shown; the press checks again.
        crate::update::activity_now(cx)
            .ok()
            .and_then(|a| crate::data_dir::move_refusal(a.positions, a.orders))
    }

    /// The folder picked: with the data or empty. Refused (the box says why) while a position
    /// or order is open, and a move into a folder that already holds Lines data.
    fn data_choose(&mut self, i: usize, with_data: bool, cx: &mut Context<Self>) {
        let Some(to) = self.data_choice.clone() else {
            return;
        };
        if with_data && self.data_holds.is_some() {
            return;
        }
        if self.data_forced.is_some() {
            return;
        }
        match crate::update::activity_now(cx) {
            Ok(a) if crate::data_dir::move_refusal(a.positions, a.orders).is_some() => {
                cx.notify();
                return;
            }
            Ok(_) => {}
            Err(e) => {
                self.note = Some((i, format!("Refused: {e}"), Tone::Problem));
                cx.notify();
                return;
            }
        }
        self.data_choice = None;
        self.data_holds = None;
        self.note = crate::data_dir::choose(to, with_data)
            .err()
            .map(|e| (i, failed(e), Tone::Problem));
        cx.notify();
    }

    /// Cancel (or, after a failure, dismiss) a new folder or a move waiting for the restart.
    fn data_cancel(&mut self, i: usize, cx: &mut Context<Self>) {
        self.data_choice = None;
        self.data_holds = None;
        self.note = crate::data_dir::cancel()
            .err()
            .map(|e| (i, failed(e), Tone::Problem));
        cx.notify();
    }

    /// The Data folder row's buttons: Change… and Use the default folder, or Cancel (Dismiss
    /// after a failure) while a new folder waits for the restart. With `LINES_DATA_DIR`,
    /// Change… shows disabled.
    fn data_buttons(&self, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let f = &self.facts.folder;
        let mut row = div().flex().flex_row().flex_wrap().gap_1().flex_none();
        let button = |id: usize, label: &'static str, on: bool| {
            div()
                .id(("data-folder", i * 4 + id))
                .flex_none()
                .whitespace_nowrap()
                .px_2()
                .rounded_sm()
                .border_1()
                .border_color(t.control_edge)
                .when(on, |d| d.cursor_pointer())
                // Dimmed, not faded (design.md §6): a 2 px dashed muted edge and dim text, the
                // extra pixel taken from the padding and the margin so it keeps its layout size and
                // center beside an enabled button.
                .when(!on, |d| {
                    d.border_2()
                        .border_dashed()
                        .border_color(t.border_disabled)
                        .px(crate::zoom::ui(8.0) - px(1.0))
                        .my(px(-1.0))
                        .text_color(t.text_dim)
                })
                .child(label)
        };
        if !f.changeable() {
            if f.in_use.is_some() {
                row = row.child(button(3, "Change…", false));
            }
            return row.into_any_element();
        }
        if f.pending() {
            let label = if f.problem.is_some() {
                "Dismiss"
            } else {
                "Cancel"
            };
            row = row.child(button(0, label, true).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, _, _, cx| {
                    v.select(i);
                    v.data_cancel(i, cx);
                }),
            ));
            return row.into_any_element();
        }
        row = row.child(button(1, "Change…", true).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |v, _, _, cx| {
                v.select(i);
                v.data_pick(i, false, cx);
            }),
        ));
        if f.can_use_default() {
            row = row.child(
                button(2, "Use the default folder · Alt+D", true).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v, _, _, cx| {
                        v.select(i);
                        v.data_pick(i, true, cx);
                    }),
                ),
            );
        }
        row.into_any_element()
    }

    /// Text that wraps between words but never inside one of `paths`.
    fn words(&self, text: &str, color: gpui::Hsla, paths: &[String]) -> AnyElement {
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_x_1()
            .min_w_0()
            .text_color(color);
        for w in crate::data_dir::wrap_tokens(text, paths) {
            row = row.child(div().flex_none().whitespace_nowrap().child(w));
        }
        row.into_any_element()
    }

    /// Paths a Data row may name.
    fn data_paths(&self) -> Vec<String> {
        let mut v = self.facts.folder.paths();
        if let Some(c) = &self.data_choice {
            v.push(c.display().to_string());
        }
        v.sort_by_key(|p| std::cmp::Reverse(p.len()));
        v
    }

    /// The folder picked, under its row: Move existing data · M, Start empty · S (Use its data
    /// · U when it already holds Lines data), Cancel · Esc.
    /// A refusal is said inside the box, and what it refuses is shown disabled.
    fn data_choice_view(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = self.theme;
        let to = self.data_choice.as_ref()?;
        let i = schema::index_of(schema::DATA_FOLDER_KEY)?;
        let trade = self.trade_refusal(cx);
        let can_move = trade.is_none() && self.data_holds.is_none();
        let can_empty = trade.is_none();
        let button = |id: &'static str, label: &'static str, strong: bool, on: bool| {
            div()
                .id(id)
                .flex_none()
                .whitespace_nowrap()
                .px_2()
                .border_1()
                .border_color(if strong && on {
                    t.accent
                } else {
                    t.control_edge
                })
                .rounded_sm()
                .when(strong && on, |d| d.font_weight(FontWeight::BOLD))
                .when(on, |d| d.cursor_pointer())
                // Dimmed, not faded (design.md §6): a 2 px dashed muted edge and dim text, the
                // extra pixel taken from the padding and the margin so it keeps its layout size and
                // center beside an enabled button.
                .when(!on, |d| {
                    d.border_2()
                        .border_dashed()
                        .border_color(t.border_disabled)
                        .px(crate::zoom::ui(8.0) - px(1.0))
                        .my(px(-1.0))
                        .text_color(t.text_dim)
                })
                .child(label)
        };
        let paths = self.data_paths();
        let mut boxed = div()
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
            .border_color(if trade.is_some() {
                t.destructive
            } else {
                t.accent
            })
            .child(div().w_full().min_w_0().child(self.words(
                &format!("Use {} after restart:", to.display()),
                t.text_main,
                &paths,
            )));
        for why in trade.iter().chain(self.data_holds.iter()) {
            boxed = boxed.child(
                div()
                    .w_full()
                    .min_w_0()
                    .text_size(crate::zoom::ui(11.5))
                    .child(self.words(why, t.destructive, &paths)),
            );
        }
        let mut mv = button("data-move", "Move existing data · M", true, can_move);
        if can_move {
            mv = mv.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, _, _, cx| v.data_choose(i, true, cx)),
            );
        }
        // A folder that already holds Lines data is used as it is: never "empty".
        let label = if self.data_holds.is_some() {
            "Use its data · U"
        } else {
            "Start empty · S"
        };
        let mut empty = button("data-empty", label, false, can_empty);
        if can_empty {
            empty = empty.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, _, _, cx| v.data_choose(i, false, cx)),
            );
        }
        Some(
            boxed
                .child(mv)
                .child(empty)
                .child(
                    button("data-no", "Cancel · Esc", false, true).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|v, _, _, cx| {
                            v.data_choice = None;
                            v.data_holds = None;
                            v.note = None;
                            cx.notify();
                        }),
                    ),
                )
                .into_any_element(),
        )
    }

    // ---- rendering ----------------------------------------------------------------------

    /// The sidebar: the sections under their groups' headers (App, Panes, Trading, Data, then
    /// About alone). A ● marks a section with a setting changed from its default; while
    /// searching, each section's count of matches.
    fn nav(&self, prefs: &Prefs, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let searching = !self.query.trim().is_empty();
        let hits = if searching {
            search_hits(&self.query)
        } else {
            Vec::new()
        };
        // Scrolls when the window is too short for every entry (the smallest window, a large
        // UI zoom).
        let mut nav = div()
            .id("settings-nav")
            .flex()
            .flex_col()
            .w(crate::zoom::ui(NAV_W))
            .flex_none()
            .overflow_y_scroll()
            .pb_2()
            .border_r_1()
            .border_color(t.axis_line);
        let mut group = None;
        let has_plugins = has_plugins();
        for (k, s) in Section::ALL.into_iter().enumerate() {
            // Plugins only while one is installed.
            if s == Section::Plugins && !has_plugins {
                continue;
            }
            // About stands alone: no header (not even an empty one).
            if group != Some(s.group()) && !s.group().is_empty() {
                nav = nav.child(
                    div()
                        .flex_none()
                        .px_3()
                        .pt_2()
                        .pb_1()
                        .text_size(crate::zoom::ui(11.0))
                        .text_color(t.text_dim)
                        .child(s.group()),
                );
            }
            group = Some(s.group());
            let on = s == self.section && !searching;
            let right = if searching {
                // Lines, as the page shows them (a line of two counts once).
                let in_s: Vec<usize> = hits
                    .iter()
                    .copied()
                    .filter(|&i| schema::all()[i].section == s)
                    .collect();
                let n = schema::line_count(&in_s);
                (n > 0).then(|| (n.to_string(), t.text_dim))
            } else {
                schema::section_changed(s, prefs, &self.target, &self.facts)
                    .then(|| ("●".to_owned(), t.accent))
            };
            nav = nav.child(
                div()
                    .id(("section", k))
                    .flex_none()
                    // About, alone, set apart from the Data group above it.
                    .when(s.group().is_empty(), |d| d.mt_3())
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_1()
                    .pr_3()
                    .py_1()
                    .cursor_pointer()
                    .when(on, |d| d.bg(t.panel))
                    .text_color(if on { t.text_main } else { t.text_dim })
                    // The selected section's edge, in a slot every entry has.
                    .child(
                        div()
                            .w(crate::zoom::ui(2.0))
                            .self_stretch()
                            .flex_none()
                            .when(on, |d| d.bg(t.accent)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .pl(crate::zoom::ui(10.0))
                            .child(s.name()),
                    )
                    .children(right.map(|(text, color)| {
                        div()
                            .flex_none()
                            .text_size(crate::zoom::ui(11.0))
                            .text_color(color)
                            .child(text)
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |v, _, _, cx| {
                            v.go_to(s);
                            cx.notify();
                        }),
                    ),
            );
        }
        nav
    }

    /// A setting's control. `unit`: a number shows its unit ("15 min"; on a line of two the
    /// words around it may say it instead). `focused`: the selected field of a line of two.
    #[allow(clippy::too_many_arguments)]
    fn control(
        &self,
        i: usize,
        value: &Value,
        unit_shown: bool,
        focused: bool,
        muted: bool,
        prefs: &Prefs,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        let st = &schema::all()[i];
        let selected = self.selected() == Some(i);
        // A flag rules the row this run: its value in force, dimmed; nothing reacts (`set`).
        let locked = schema::flag_value(st, &self.facts).is_some();
        match (st.kind, value) {
            (Kind::Choice(c), Value::Choice(_) | Value::Text(_)) if schema::drops_down(c) => {
                let text = match value {
                    Value::Choice(k) => c
                        .get(*k)
                        .map(|n| {
                            schema::choice_label(st.key, *k, n, &prefs.app.appearance).into_owned()
                        })
                        .unwrap_or_default(),
                    Value::Text(s) => s.clone(),
                    _ => String::new(),
                };
                self.drop_control(i, text, value, focused, locked, cx)
            }
            (Kind::Products, Value::List(l)) => {
                self.drop_control(i, products_summary(l), value, focused, locked, cx)
            }
            (Kind::Picks(names), Value::List(l)) => {
                self.drop_control(i, picks_summary(l, names.len()), value, focused, locked, cx)
            }
            (Kind::Editor, Value::Text(s)) if List::of(st.key).is_some() => {
                self.list_control(i, s.clone(), focused, cx)
            }
            (Kind::Choice(c), Value::Choice(k)) => {
                // Wraps onto more lines when the row is narrower than its chips.
                let mut seg = div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .min_w_0()
                    .border_1()
                    .border_color(t.axis_line)
                    .rounded_sm();
                // A palette row whose palette isn't shown now (the other appearance's): its
                // choice outlined in accent (3:1 on the row, `idle_choice_outline_is_3_to_1`),
                // not filled; still editable. An overlay, so the chip keeps its size.
                let a = &prefs.app.appearance;
                let idle = schema::palette_in_use(st.key, a, schema::os_dark()) == Some(false);
                for (j, name) in c.iter().enumerate() {
                    let on = j == *k;
                    // What the chip draws (also what the layout lint is told).
                    let label = schema::choice_label(st.key, j, name, a);
                    seg = seg.child(
                        div()
                            .id(("choice", i * 64 + j))
                            .flex_none()
                            .whitespace_nowrap()
                            .px_2()
                            .cursor_pointer()
                            .when(on && !idle, |d| {
                                d.bg(t.accent)
                                    .text_color(t.bg)
                                    .font_weight(FontWeight::BOLD)
                            })
                            .when(on && idle, |d| {
                                d.relative()
                                    .text_color(t.text_main)
                                    .font_weight(FontWeight::BOLD)
                                    .child(
                                        div()
                                            .absolute()
                                            .inset_0()
                                            .border_1()
                                            .border_color(t.accent),
                                    )
                            })
                            .relative()
                            .children(crate::layout_rec::text_marker(&label))
                            .child(label)
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
                // Off | On, the current one filled: a state, never a lone button.
                let mut seg = div()
                    .flex()
                    .flex_row()
                    .flex_none()
                    .border_1()
                    .border_color(t.axis_line)
                    .rounded_sm()
                    .when(locked, |d| d.border_dashed());
                for (j, (name, val)) in [("Off", false), ("On", true)].into_iter().enumerate() {
                    let on = val == *b;
                    seg = seg.child(
                        div()
                            .id(("toggle", i * 2 + j))
                            .px_2()
                            .when(!locked, |d| d.cursor_pointer())
                            .when(on && !locked, |d| {
                                d.bg(t.accent)
                                    .text_color(t.bg)
                                    .font_weight(FontWeight::BOLD)
                            })
                            .when(on && locked, |d| {
                                d.bg(t.element_selected).text_color(t.text_main)
                            })
                            .child(name)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |v, _, _, cx| {
                                    v.select(i);
                                    v.set(i, Value::Bool(val), false, cx);
                                }),
                            ),
                    );
                }
                seg.into_any_element()
            }
            (Kind::Number { unit, optional, .. }, Value::Number(n)) => {
                let editing = selected && self.edit.is_some();
                // The unit lives in the value: "$1,000", "15 min", "5 contracts".
                let with_unit = |num: String| match unit {
                    "$" => format!("${num}"),
                    "" => num,
                    u if unit_shown => format!("{num} {u}"),
                    _ => num,
                };
                let (text, dim) = match (&self.edit, editing, n) {
                    (Some(e), true, _) => (with_unit(format!("{e}▏")), false),
                    (_, _, Some(n)) => (with_unit(schema::grouped(*n)), false),
                    // Off: no limit.
                    (_, _, None) if optional => ("Off".to_owned(), true),
                    // A part that needs another on first (the cooldown without a losing
                    // streak, the news window with the blackout off): empty, its unit muted.
                    (_, _, None) if unit_shown => (unit.to_owned(), true),
                    (_, _, None) => (String::new(), true),
                };
                let dim = dim || muted;
                let text = schema::flag_text(st, &self.facts)
                    .filter(|_| locked)
                    .unwrap_or(text);
                let steps = !(locked || muted || editing || st.read_only())
                    && steps_fit(i, self.win_w, crate::zoom::get());
                let field = div()
                    .id(("number", i))
                    .flex_none()
                    .font_features(gpui::FontFeatures(std::sync::Arc::new(vec![(
                        "tnum".into(),
                        1,
                    )])))
                    .min_w(crate::zoom::ui(if steps {
                        NUMBER_STEP_W
                    } else {
                        NUMBER_W
                    }))
                    .px_2()
                    .rounded_sm()
                    .border_1()
                    .when(!locked, |d| d.cursor_pointer())
                    .when(locked, |d| d.border_dashed())
                    .flex()
                    .justify_end()
                    .whitespace_nowrap()
                    .border_color(if editing {
                        t.accent
                    } else if focused {
                        t.text_main
                    } else {
                        t.axis_line
                    })
                    .text_color(if dim { t.text_dim } else { t.text_main })
                    .child(text)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |v, _, _, cx| {
                            v.select(i);
                            v.activate(i, schema::all()[i].kind, cx);
                            cx.notify();
                        }),
                    );
                self.stepper(i, *n, field, !steps, cx)
            }
            (Kind::Editor, _) if st.key == schema::RESET_KEY => self.reset_buttons(i, cx),
            (Kind::Editor, _) if st.key == schema::DATA_FOLDER_KEY => self.data_buttons(i, cx),
            (Kind::Editor, _) if st.key == schema::ROLLBACK_KEY => {
                match self.facts.rollback.clone() {
                    None => div().into_any_element(),
                    Some(v) => self
                        .button(("rollback", i), format!("Roll back to v{v}"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |v, _, _, cx| {
                                v.select(i);
                                v.rollback_press(cx);
                            }),
                        )
                        .into_any_element(),
                }
            }
            (Kind::Editor, _) => {
                let label = if st.key == schema::PROP_OVERRIDES_KEY {
                    if self.overrides_open {
                        "Hide overrides"
                    } else {
                        "Override…"
                    }
                } else {
                    "Edit…"
                };
                self.button(("editor", i), label.to_owned())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |v, _, _, cx| {
                            v.select(i);
                            v.activate(i, Kind::Editor, cx);
                            cx.notify();
                        }),
                    )
                    .into_any_element()
            }
            _ => div().into_any_element(),
        }
    }

    /// A dropdown: `text` (the value in force, or a summary) and ▾; open, its list floats under
    /// it (above when there's more room there), capped and scrolling.
    fn drop_control(
        &self,
        i: usize,
        text: String,
        value: &Value,
        focused: bool,
        locked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        let open = self.drop.as_ref().filter(|d| d.i == i);
        let label = text.clone();
        let trigger = drop_view::trigger(("drop", i), text, focused, open.is_some(), locked, 0, &t)
            .when(!locked, |d| {
                d.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v, _, _, cx| {
                        v.select(i);
                        v.activate(i, schema::all()[i].kind, cx);
                        cx.notify();
                    }),
                )
            });
        let list = open.map(|d| {
            let checked = |k: usize| Self::drop_checked(value, &d.keys, k);
            let el = drop_view::list(
                "drop-list",
                &d.list,
                &checked,
                crate::layout_rec::POPUP,
                &t,
                cx,
                |v: &mut Self, k, _, cx| {
                    if let Some(d) = v.drop.as_mut() {
                        let o = d.list.choose(k);
                        v.drop_outcome(o, cx);
                    }
                },
            );
            (el, d.scroll.clone(), drop_view::cap(), t.panel)
        });
        crate::dropdown::dropdown_list(trigger, list)
            .lint_trigger(&label)
            .on_dismiss(Box::new(cx.listener(
                |v, _: &gpui::MouseDownEvent, _, cx| {
                    v.drop = None;
                    cx.notify();
                },
            )))
            .into_any_element()
    }

    /// Time zone, UI font: the value in force and ▾; open, its searchable list floats under it.
    fn list_control(
        &self,
        i: usize,
        text: String,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        let Some(list) = List::of(schema::all()[i].key) else {
            return div().into_any_element();
        };
        let open = self.list_pick.as_ref().is_some_and(|(l, _)| *l == list);
        let label = text.clone();
        let trigger = drop_view::trigger(("list", i), text, focused, open, false, 0, &t)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, _, _, cx| {
                    v.select(i);
                    if open {
                        v.list_pick = None;
                    } else {
                        v.drop = None;
                        v.open_list(list, cx);
                    }
                    cx.notify();
                }),
            );
        crate::dropdown::dropdown(trigger, self.list_view(list.key(), cx))
            .lint_trigger(&label)
            .on_dismiss(Box::new(cx.listener(
                |v, _: &gpui::MouseDownEvent, _, cx| {
                    v.list_pick = None;
                    cx.notify();
                },
            )))
            .into_any_element()
    }

    /// A number field between − and +, each a step (`schema::stepped`: $100, 5 minutes, else
    /// 1); none while a flag rules it, it rests, or it is being typed.
    fn stepper(
        &self,
        i: usize,
        n: Option<i64>,
        field: impl IntoElement,
        quiet: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let st = &schema::all()[i];
        if quiet || st.read_only() {
            return field.into_any_element();
        }
        let t = self.theme;
        let kind = st.kind;
        // A prop override's empty is "the preset's" (not Off): − stops at the least.
        let to_empty = !schema::PROP_OVERRIDES.contains(&st.key);
        let next =
            move |up: bool| schema::stepped(kind, n, up).filter(|next| next.is_some() || to_empty);
        let step = |up: bool| {
            let can = next(up).is_some();
            let hover = t.ghost_selected;
            div()
                .id(("step", i * 2 + usize::from(up)))
                .flex_none()
                .w(crate::zoom::ui(20.0))
                .flex()
                .justify_center()
                .rounded_sm()
                .border_1()
                // At its end (Off, the least, the most): quieter, and nothing happens.
                .border_color(if can { t.control_edge } else { t.axis_line })
                .text_color(if can { t.text_main } else { t.text_dim })
                .when(can, |d| d.cursor_pointer().hover(move |s| s.bg(hover)))
                .child(if up { "+" } else { "−" })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v, _, _, cx| {
                        v.select(i);
                        if let Some(next) = next(up) {
                            v.set(i, Value::Number(next), false, cx);
                        }
                        cx.notify();
                    }),
                )
        };
        let (minus, plus) = (step(false), step(true));
        div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap_1()
            .child(minus)
            .child(field)
            .child(plus)
            .into_any_element()
    }

    /// A quiet outlined button (Edit…, Change…, Override…).
    fn button(&self, id: (&'static str, usize), label: String) -> gpui::Stateful<gpui::Div> {
        let t = self.theme;
        div()
            .id(id)
            .flex_none()
            .whitespace_nowrap()
            .px_2()
            .rounded_sm()
            .border_1()
            .border_color(t.axis_line)
            .cursor_pointer()
            .child(label)
    }

    /// Reset paper account's buttons: keep the journal, or clear it too. An armed one says
    /// "Press again" (`crate::paper_reset`); a broker account has none.
    fn reset_buttons(&self, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let account = self
            .target
            .account
            .as_deref()
            .map(lines_trading::AccountId::new);
        let trading = crate::trading::Trading::try_global(cx);
        let paper = account
            .as_ref()
            .and_then(|a| trading.and_then(|t| t.account(a)))
            .is_some_and(|a| a.paper);
        if !paper {
            return div().into_any_element();
        }
        let mut row = div().flex().flex_row().flex_wrap().gap_1().flex_none();
        let armed_any = [false, true].map(|j| {
            account
                .as_ref()
                .zip(trading)
                .is_some_and(|(a, t)| t.reset_armed(a, j))
        });
        for (k, journal) in [(0usize, false), (1, true)] {
            let armed = armed_any[k];
            // While one is armed it is the only target: the other goes.
            if !armed && armed_any.contains(&true) {
                continue;
            }
            let label = match (armed, journal) {
                (true, false) => "Press again: reset",
                (true, true) => "Press again: reset and clear journal",
                (false, false) => "Reset…",
                (false, true) => "Reset and clear journal…",
            };
            row = row.child(
                div()
                    .id(("reset", i * 2 + k))
                    .flex_none()
                    .whitespace_nowrap()
                    .px_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(if armed { t.warn } else { t.axis_line })
                    .when(armed, |d| d.font_weight(FontWeight::BOLD))
                    .cursor_pointer()
                    .child(label)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |v, _, _, cx| {
                            v.select(i);
                            v.reset_press(i, journal, cx);
                        }),
                    ),
            );
        }
        row.into_any_element()
    }

    /// The armed reset's words for the chosen account, if one is waiting for its second press.
    fn reset_confirm(&self, cx: &App) -> Option<String> {
        let account = self
            .target
            .account
            .as_deref()
            .map(lines_trading::AccountId::new)?;
        let t = crate::trading::Trading::try_global(cx)?;
        let info = t.account(&account)?;
        [false, true]
            .into_iter()
            .find(|&j| t.reset_armed(&account, j))
            .map(|j| crate::paper_reset::confirm_text(&info.name, &t.source, info.start_balance, j))
    }

    /// Roll back to the version the last update replaced (Settings › About): held while
    /// anything is trading, and said why.
    fn rollback_press(&mut self, cx: &mut Context<Self>) {
        if let Err(why) = crate::update::roll_back(cx)
            && cx.has_global::<crate::workspace::Workspace>()
        {
            crate::workspace::notice(why, cx);
        }
        cx.notify();
    }

    /// A press of a reset button (or Enter on the row: the one keeping the journal).
    fn reset_press(&mut self, i: usize, journal: bool, cx: &mut Context<Self>) {
        let Some(account) = self
            .target
            .account
            .as_deref()
            .map(lines_trading::AccountId::new)
        else {
            self.note = Some((i, "Choose an account".to_owned(), Tone::Problem));
            cx.notify();
            return;
        };
        let Some(t) = crate::trading::Trading::try_global(cx) else {
            self.note = Some((i, "Trading is not running".to_owned(), Tone::Problem));
            cx.notify();
            return;
        };
        // A person's command from a window (the core has no Settings source; Palette is the
        // person's, so its inner Flatten pauses strategies on the account as a click would).
        self.note = match t.press_reset(&account, journal, lines_trading::Source::Palette) {
            // The row says what the second press does while it waits (`reset_confirm`), and
            // drops it when the wait is over.
            Ok(Some(_)) => {
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(crate::paper_reset::CONFIRM)
                        .await;
                    let _ = this.update(cx, |_, cx| cx.notify());
                })
                .detach();
                None
            }
            Ok(None) => Some((
                i,
                "Resetting: anything open is flattened first".to_owned(),
                Tone::Done,
            )),
            Err(why) => Some((i, why, Tone::Problem)),
        };
        cx.notify();
    }

    fn select(&mut self, i: usize) {
        // A prop override selected (a palette command, a screenshot's change) unfolds.
        if schema::all()
            .get(i)
            .is_some_and(|st| schema::PROP_OVERRIDES.contains(&st.key))
        {
            self.overrides_open = true;
        }
        if let Some(r) = self.rows().iter().position(|&x| x == i) {
            self.row = r;
        }
        self.edit = None;
    }

    /// One line of a page: its label (its description under it, always: selecting a line only
    /// tints it, so no row grows), then the ● slot, the control(s) and the ↺ slot. Lines under it:
    /// what the core has in force, a Data line's flag or restart, a summary, a note.
    fn line_view(
        &self,
        label: &str,
        parts: &[schema::Part],
        sub: bool,
        prefs: &Prefs,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        let idx: Vec<usize> = parts
            .iter()
            .filter_map(|p| match p {
                schema::Part::Field(k, _) => schema::index_of(k),
                schema::Part::Word(_) => None,
            })
            .collect();
        let Some(&first) = idx.first() else {
            return div().into_any_element();
        };
        let sel = self.selected().filter(|s| idx.contains(s));
        let selected = sel.is_some();
        let value_of = |i: usize| {
            let st = &schema::all()[i];
            // A flag ruling a Data row this run: its value in force, not the saved one.
            schema::flag_value(st, &self.facts)
                .unwrap_or_else(|| (st.get)(prefs, &self.target, &self.facts))
        };
        let data = schema::all()[first].section == Section::DataFolder;
        let paths = if data { self.data_paths() } else { Vec::new() };
        // A failure or refusal in the error color (its words say so too); the rest amber.
        let tone_color = |line: &str| {
            if crate::data_dir::is_problem(line) {
                t.destructive
            } else {
                t.warn
            }
        };
        let small = |text: String, color: gpui::Hsla| -> AnyElement {
            div()
                .text_size(crate::zoom::ui(11.5))
                .text_color(color)
                // The layout lint sees each line of text a row draws (screenshot builds).
                .relative()
                .children(crate::layout_rec::text_marker(&text))
                .child(if data {
                    self.words(&text, color, &paths)
                } else {
                    div().child(text).into_any_element()
                })
                .into_any_element()
        };
        let mut left = div()
            .flex()
            .flex_col()
            .flex_grow_1()
            .flex_basis(crate::zoom::ui(LABEL_MIN_W))
            .min_w(crate::zoom::ui(LABEL_MIN_W))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_baseline()
                    .gap_x_2()
                    .child(
                        div()
                            .text_color(t.text_main)
                            .relative()
                            .children(crate::layout_rec::text_marker(label))
                            .child(label.to_owned()),
                    )
                    // Where the value applies when that isn't the app (decision 0021: no
                    // hidden inheritance): "New panes", "This workspace". Rows under an
                    // account picker have it there.
                    .children(schema::all()[first].scope.label().map(|w| {
                        div()
                            .flex_none()
                            .text_size(crate::zoom::ui(11.0))
                            .text_color(t.text_dim)
                            .relative()
                            .children(crate::layout_rec::text_marker(w))
                            .child(w)
                    })),
            );
        // The description, on every line, selected or not (a line of two settings: its first
        // with one). Static: a row is as tall selected, hovered or not.
        if let Some(d) = idx
            .iter()
            .map(|&i| schema::all()[i].description)
            .find(|d| !d.is_empty())
        {
            left = left.child(
                div()
                    .text_size(crate::zoom::ui(11.5))
                    .text_color(t.text_dim)
                    .relative()
                    .children(crate::layout_rec::text_marker(d))
                    .child(d),
            );
        }
        // Theme: System says what it shows now ("Now: Dark · Standard"), selected or not.
        if idx
            .iter()
            .any(|&i| schema::all()[i].key == "appearance.theme")
            && let Some(now) = schema::system_now(&prefs.app.appearance, schema::os_dark())
        {
            left = left.child(
                div()
                    .text_size(crate::zoom::ui(11.5))
                    .text_color(t.text_main)
                    .relative()
                    .children(crate::layout_rec::text_marker(&now))
                    .child(now),
            );
        }
        // A choice whose chips would wrap even on a line of their own, at this window's width
        // and zoom (Bar period's 23 chips; at 150 % in a narrow window, shorter ones too), takes
        // that line under the label, in a column: its chips wrap at a width the column gives
        // them, so the row's height is measured with them wrapped. A wrapping chip group as an
        // item of the wrapping row (and of a wrapping control group) could be measured on one
        // line and then shrunk, painting its wrapped chips over the description and the next
        // row. Chips that fit their line never wrap, so they keep their place beside the label.
        // A sub line (a prop override) is indented 16 px: that much less room.
        let line_w = content_line_w(self.win_w, crate::zoom::get()) - if sub { 16.0 } else { 0.0 };
        // In the column the line's parts stack: every long choice today is alone on its line.
        let own_line = idx
            .iter()
            .any(|&k| matches!(schema::all()[k].kind, Kind::Choice(c) if !schema::drops_down(c) && long_choice(c, line_w)));
        let mut right = if own_line {
            div().flex().flex_col().items_end().w_full().min_w_0()
        } else {
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap_2()
                .ml_auto()
                .min_w_0()
                .max_w_full()
        };
        let mut dot = false;
        let mut resettable: Option<usize> = None;
        // A part hidden while its guardrail is off: the line shows its first control alone
        // ("Cooldown  Off"), no words.
        let alone = idx
            .iter()
            .any(|&i| schema::dormant(&schema::all()[i], prefs, &self.target) == Some(None));
        for part in parts {
            if alone
                && !matches!(part, schema::Part::Field(k, _) if schema::index_of(k) == Some(first))
            {
                continue;
            }
            let i = match part {
                schema::Part::Word(w) => {
                    right = right.child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .text_color(t.text_dim)
                            .child(*w),
                    );
                    continue;
                }
                schema::Part::Field(k, unit) => match schema::index_of(k) {
                    Some(i) => (i, *unit),
                    None => continue,
                },
            };
            let (i, unit) = i;
            let st = &schema::all()[i];
            // The news window while the blackout is off: the minutes it starts with, muted.
            let resting = schema::dormant(st, prefs, &self.target).flatten();
            let value = resting.map_or_else(|| value_of(i), |n| Value::Number(Some(n)));
            let muted = resting.is_some();
            let (changed, reset) = schema::marks(st, prefs, &self.target, &self.facts);
            dot |= changed;
            if reset && (resettable.is_none() || sel == Some(i)) {
                resettable = Some(i);
            }
            match (st.kind, &value) {
                (Kind::Info, Value::Text(s)) => {
                    // A lockout or breach is the row's warning (by its words and the lock,
                    // not color alone).
                    let color = if s.starts_with('🔒') {
                        t.warn
                    } else {
                        t.text_main
                    };
                    if inline_text(s) {
                        right = right.child(
                            div()
                                .flex_none()
                                .whitespace_nowrap()
                                .text_color(color)
                                .child(s.clone()),
                        );
                    } else if !s.is_empty() {
                        left = left.child(div().text_color(color).child(s.clone()));
                    }
                }
                (Kind::Editor, Value::Text(s)) if summary_beside(st.key) => {
                    right = right
                        .child(
                            div()
                                .flex_none()
                                .whitespace_nowrap()
                                .text_color(t.text_dim)
                                .child(s.clone()),
                        )
                        .child(self.control(i, &value, unit, false, muted, prefs, cx));
                }
                _ => {
                    let focused = idx.len() > 1 && sel == Some(i);
                    right = right.child(self.control(i, &value, unit, focused, muted, prefs, cx));
                }
            }
        }
        // Long summaries go under the label: the data folder, the reset's account, the
        // rollback's version.
        for &i in &idx {
            let st = &schema::all()[i];
            let summary_color = if st.key == schema::DATA_FOLDER_KEY
                && self.facts.folder.by == crate::data_dir::By::Env
            {
                // The folder in use, dimmed (in an AA token) while LINES_DATA_DIR rules it.
                t.text_dim
            } else {
                t.text_main
            };
            let summary = match (st.kind, value_of(i)) {
                (Kind::Editor, Value::Text(s))
                    if !summary_beside(st.key) && List::of(st.key).is_none() =>
                {
                    Some(small(s, summary_color))
                }
                _ => None,
            };
            // What is in force now, when the core holds a looser value back; a Data row's
            // flag, restart, progress or failure line.
            let status = schema::status(st, prefs, &self.target, &self.facts).map(|line| {
                let color = tone_color(&line);
                small(line, color)
            });
            // The data folder: the folder in use, then what waits; other editors: the status
            // first, then their summary.
            if st.key == schema::DATA_FOLDER_KEY {
                left = left.children(summary).children(status);
            } else {
                left = left.children(status).children(summary);
            }
            // A reset waiting for its second press says what that press will do.
            if st.key == schema::RESET_KEY
                && let Some(text) = self.reset_confirm(cx)
            {
                left = left.child(
                    div()
                        .text_size(crate::zoom::ui(11.5))
                        .text_color(t.warn)
                        // Armed shows by weight too, not only by color.
                        .font_weight(FontWeight::BOLD)
                        .child(text),
                );
            }
            if let Some((_, text, tone)) = self.note.as_ref().filter(|n| n.0 == i) {
                let color = match tone {
                    Tone::Problem => tone_color(text),
                    Tone::Done => t.text_main,
                };
                left = left.child(small(text.clone(), color));
            }
        }
        let reset_slot = div()
            .id(("reset", first))
            // A 24 px hit each way.
            .w(crate::zoom::hit(RESET_W))
            .h(crate::zoom::hit(RESET_W))
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            // The glyph sits small in its em: at 16 px it reads as large as 13 px text.
            .text_size(crate::zoom::ui(16.0))
            .text_color(t.text_main);
        let reset_slot = match resettable {
            Some(r) => crate::tooltip::tip_keys(
                reset_slot,
                &if idx.len() > 1 {
                    // Which of the line's settings it resets.
                    format!("Reset {} to default", schema::all()[r].name.to_lowercase())
                } else {
                    "Reset to default".to_owned()
                },
                vec!["Ctrl".into(), "R".into()],
                &t,
            )
            .cursor_pointer()
            .child("↺")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, _, _, cx| {
                    v.select(r);
                    v.reset(r, cx);
                }),
            ),
            None => reset_slot,
        };
        let mut row = div()
            .id(("row", first))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .pr_2()
            .py_1()
            .border_b_1()
            .border_color(t.axis_line)
            .when(selected, |d| d.bg(t.panel))
            // The selected line's edge, in a slot every line has (selecting moves nothing).
            .child(
                div()
                    .w(crate::zoom::ui(2.0))
                    .self_stretch()
                    .flex_none()
                    .when(selected, |b| b.bg(t.accent)),
            )
            .child(
                // Label (and description), then the controls: beside them when they fit, else
                // on their own line below (right-aligned). Nothing squeezes the label.
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .flex_1()
                    .min_w_0()
                    // Controls sit on the title line: a description that grows (the Theme
                    // row's "Now:" line) never moves them.
                    .items_start()
                    .gap_x_2()
                    .gap_y_1()
                    .when(sub, |d| d.pl(crate::zoom::ui(16.0)))
                    .child(left)
                    .child(right),
            )
            // ● changed from its default (a shape, on its row and its section's entry), then
            // ↺: one fixed column at the right, wherever the controls wrap.
            .child(
                div()
                    .w(crate::zoom::ui(DOT_W))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .text_color(t.accent)
                    .when(dot, |d| d.child("●")),
            )
            .child(reset_slot)
            .on_mouse_down(MouseButton::Left, {
                // A press on the line selects it, unless one of its fields already is (a
                // field's own press chose which).
                let idx = idx.clone();
                cx.listener(move |v, _, _, cx| {
                    if v.selected().is_none_or(|s| !idx.contains(&s)) {
                        v.select(first);
                    }
                    cx.notify();
                })
            })
            .into_any_element();
        if idx.contains(&schema::index_of(schema::DATA_FOLDER_KEY).unwrap_or(usize::MAX))
            && let Some(choice) = self.data_choice_view(cx)
        {
            row = div()
                .flex()
                .flex_col()
                .child(row)
                .child(choice)
                .into_any_element();
        }
        if let Some(p) = self.pending.as_ref().filter(|p| idx.contains(&p.index)) {
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
                        .child(div().w_full().min_w_0().child(format!("⚠ {}", p.text)))
                        .child(
                            div()
                                .id("loosen-yes")
                                .px_2()
                                .border_1()
                                .border_color(t.warn)
                                .rounded_sm()
                                .font_weight(FontWeight::BOLD)
                                .cursor_pointer()
                                .child(if p.tomorrow {
                                    "Loosen from the next session · Y"
                                } else {
                                    "Loosen now · Y"
                                })
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
                                        v.cancel_pending();
                                        cx.notify();
                                    }),
                                ),
                        ),
                )
                .into_any_element();
        }
        row
    }

    /// A press on the Keys page.
    pub(crate) fn keys_action(&mut self, a: crate::keys_view::Action, cx: &mut Context<Self>) {
        self.keys.press(a, cx);
        cx.notify();
    }

    /// A press on the Plugins page.
    pub(crate) fn plugin_action(&mut self, a: crate::plugins_view::Action, cx: &mut Context<Self>) {
        self.plugins.press(a, cx);
        cx.notify();
    }

    /// A row of an open plugin choice's list clicked.
    pub(crate) fn plugin_pick(&mut self, k: usize, cx: &mut Context<Self>) {
        if let Some(o) = self.plugins.open.take()
            && let Some(v) = o.values.get(k)
        {
            self.plugins.press(
                crate::plugins_view::Action::Set(o.source, o.key, v.clone()),
                cx,
            );
        }
        cx.notify();
    }

    /// A press on the Connections page (decision 0030).
    pub(crate) fn conn_action(
        &mut self,
        a: crate::connections_view::Action,
        cx: &mut Context<Self>,
    ) {
        use crate::connections_view::Action;
        match a {
            Action::Switch(name, on) => update_settings(cx, |s| {
                if on {
                    s.sources.remove(&name);
                } else {
                    s.sources.insert(
                        name,
                        crate::sources::SourcePrefs {
                            enabled: Some(false),
                        },
                    );
                }
            }),
            Action::Edit(name, field) => {
                let buf = match field.kind {
                    // A secret is typed afresh (Replace); never put back in a field.
                    lines_feed::extension::FieldKind::Secret => String::new(),
                    _ => crate::connections::plain_value(&settings(cx).connections, &name, &field)
                        .unwrap_or_default(),
                };
                self.conn.edit = Some(crate::connections_view::Edit {
                    source: name,
                    field,
                    buf,
                });
            }
            Action::Remove(name, key) => {
                if self.conn.press_remove(&name, key) {
                    self.conn.reload(Some(Box::new(move || {
                        crate::connections::remove_secret(&name, key)
                    })));
                }
            }
            Action::Set(name, key, value) => {
                self.conn_commit(crate::connections_view::Commit::Plain(name, key, value), cx)
            }
            Action::Test(name) => self.conn.start_test(&name, cx),
            Action::Save => {
                if let Some(c) = self.conn.finish() {
                    self.conn_commit(c, cx);
                }
            }
            Action::Cancel => self.conn.edit = None,
        }
        cx.notify();
    }

    /// Write a finished Connections field: a secret to the keychain (off-thread), a plain
    /// value to `settings.json` (and to the feeds that read it).
    fn conn_commit(&mut self, c: crate::connections_view::Commit, cx: &mut Context<Self>) {
        use crate::connections_view::Commit;
        match c {
            Commit::Secret(name, key, value) => self.conn.reload(Some(Box::new(move || {
                crate::connections::set_secret(&name, key, &value)
            }))),
            Commit::Plain(name, key, value) => {
                update_settings(cx, |s| {
                    s.connections
                        .entry(name)
                        .or_default()
                        .insert(key.to_owned(), value);
                });
                crate::connections::sync_plain(&settings(cx).connections);
                // Setup may have changed ("not set up").
                self.conn.reload(None);
            }
            Commit::Clear(name, key) => {
                update_settings(cx, |s| {
                    if let Some(m) = s.connections.get_mut(&name) {
                        m.remove(key);
                        if m.is_empty() {
                            s.connections.remove(&name);
                        }
                    }
                });
                crate::connections::sync_plain(&settings(cx).connections);
                self.conn.reload(None);
            }
        }
    }

    /// A group's title in small capitals; on Order entry › Brackets, the account and product
    /// the presets are for beside it.
    fn group_title(&self, title: &str, product: bool, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let mut d = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .pt_3()
            .pb_1()
            .text_size(crate::zoom::ui(11.0))
            .text_color(t.text_dim)
            .child(div().flex_none().child(title.to_owned()));
        if product {
            d = d
                .child(self.account_field(cx))
                .child(self.product_field(cx))
                .children(
                    self.account_kind()
                        .map(|k| crate::trade_bar::account_badge(k, &self.theme)),
                );
        }
        d.into_any_element()
    }

    /// The account picker (a click steps to the next account, as Alt+A).
    fn account_field(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        div()
            .id("account-step")
            .min_w_0()
            .max_w_full()
            .truncate()
            .px_2()
            .border_1()
            .border_color(t.axis_line)
            .rounded_sm()
            .cursor_pointer()
            .text_color(t.text_main)
            .child(format!(
                "{} ▾",
                self.target.account_name.as_deref().unwrap_or("No account")
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, _, _, cx| {
                    v.step_account();
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// The product picker (a click steps to the next product, as Alt+P).
    fn product_field(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        div()
            .id("product-step")
            .flex_none()
            .px_2()
            .border_1()
            .border_color(t.axis_line)
            .rounded_sm()
            .cursor_pointer()
            .text_color(t.text_main)
            .child(format!(
                "{} ▾",
                self.target.product.as_deref().unwrap_or("No product")
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, _, _, cx| {
                    v.step_product();
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// The chosen account's facts.
    fn account(&self) -> Option<&AccountFact> {
        self.facts
            .accounts
            .iter()
            .find(|a| Some(&a.id) == self.target.account.as_ref())
    }

    /// The chosen account's kind, for its badge beside the picker (PAPER, SIM, LIVE).
    fn account_kind(&self) -> Option<&'static str> {
        self.account().map(|a| a.kind)
    }

    /// The chosen account's size, muted beside its badge: "$50,000 start".
    fn account_facts(&self) -> Option<String> {
        let n = self.account()?.account_size?;
        Some(format!("${} start", schema::grouped(n)))
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
                    .child(div().w(crate::zoom::ui(90.0)).child("Stop"))
                    .child(div().w(crate::zoom::ui(90.0)).child("Target"))
                    .child(div().w(crate::zoom::ui(140.0)).child("Name")),
            );
        for (r, row) in ed.rows.iter().enumerate() {
            let cell = |text: &str, on: bool, w: f32| {
                div()
                    .w(crate::zoom::ui(w))
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
                .text_size(crate::zoom::ui(11.5))
                .text_color(if problem.is_some() { t.warn } else { t.text_dim })
                .child(problem.unwrap_or_else(|| {
                    "Enter saves · Esc cancels · Insert adds · Delete removes · Ctrl+↑↓ reorders · Tab moves".to_owned()
                })),
        );
        Some(v.into_any_element())
    }

    /// A setting's open list (under the row of `key`): what's typed, then the matches around
    /// the selection.
    fn list_view(&self, key: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = self.theme;
        let (list, p) = self.list_pick.as_ref()?;
        let list = *list;
        if list.key() != key {
            return None;
        }
        let mut v = div()
            .flex()
            .flex_col()
            .p_1()
            .min_w(crate::zoom::ui(320.0))
            .block_mouse_except_scroll()
            .bg(t.panel)
            .rounded_md()
            .border_1()
            .border_color(t.axis_line)
            .child(
                div()
                    .px_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(t.axis_line)
                    .child(format!("Search {}: {}▏", list.plural(), p.query)),
            );
        let top = p.top();
        for (r, &k) in p.hits.iter().enumerate().skip(top).take(search_list::ROWS) {
            let on = r == p.sel;
            let mut row = div()
                .id((list.plural(), k))
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(if on {
                    t.accent
                } else {
                    gpui::transparent_black()
                })
                .cursor_pointer()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(list.text(k));
            // A font's row ends with a sample in that font: its name stays in the UI font, so a
            // symbol font's row is still readable (GPUI falls back to the OS fonts for a family
            // it can't load).
            if let Some(family) = list.family(k) {
                row = row.child(
                    div()
                        .flex_1()
                        .flex()
                        .justify_end()
                        .text_color(t.text_dim)
                        .font_weight(FontWeight::NORMAL)
                        .font_family(family)
                        .child(fonts::SAMPLE),
                );
            }
            if on {
                row = row.font_weight(FontWeight::BOLD);
            }
            v = v.child(row.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, _, _, cx| {
                    v.choose(list, k, cx);
                    cx.notify();
                }),
            ));
        }
        let shown = p.hits.len().min(search_list::ROWS);
        // What's shown, then each key as caps beside its action (`docs/design.md` §6).
        let keys: &[(&[&str], &str)] = if p.hits.is_empty() {
            &[(&["Backspace"], "edits"), (&["Esc"], "closes")]
        } else {
            &[
                (&["↑", "↓"], "choose"),
                (&["Enter"], "sets"),
                (&["Esc"], "closes"),
            ]
        };
        let mut foot = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_2()
            .text_size(crate::zoom::ui(11.5))
            .text_color(t.text_dim)
            .child(if p.hits.is_empty() {
                format!("No {} matches", list.singular())
            } else {
                format!("{shown} of {}", p.hits.len())
            });
        for (caps, action) in keys {
            let caps: Vec<String> = caps.iter().map(|c| (*c).to_owned()).collect();
            foot = foot.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .children(crate::keycap::caps_of(&caps, &t))
                    .child(*action),
            );
        }
        v = v.child(foot);
        Some(v.into_any_element())
    }
}

/// A setting chosen from a searchable list (`crate::search_list`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum List {
    /// Appearance › Time zone (`crate::display_zone`).
    Zone,
    /// Appearance › UI font (`crate::fonts`).
    Font,
}

impl List {
    const ALL: [List; 2] = [List::Zone, List::Font];

    /// The list a setting chooses from, if it has one.
    fn of(key: &str) -> Option<List> {
        List::ALL.into_iter().find(|l| l.key() == key)
    }

    fn key(self) -> &'static str {
        match self {
            List::Zone => display_zone::KEY,
            List::Font => fonts::KEY,
        }
    }

    /// `--settings-set KEY=list:Q`: the list and Q.
    #[cfg(feature = "screenshot")]
    fn typed(s: &str) -> Option<(List, &str)> {
        let (key, q) = s.split_once(LIST_OPEN)?;
        Some((List::of(key)?, q))
    }

    fn singular(self) -> &'static str {
        match self {
            List::Zone => "zone",
            List::Font => "font",
        }
    }

    fn plural(self) -> &'static str {
        match self {
            List::Zone => "zones",
            List::Font => "fonts",
        }
    }

    /// Choice `k`'s words in the list.
    fn text(self, k: usize) -> String {
        match self {
            List::Zone => display_zone::option_text(&display_zone::options()[k]),
            List::Font => fonts::options()[k].describe().to_owned(),
        }
    }

    /// The font family choice `k`'s sample is drawn in (the font list previews each font).
    fn family(self, k: usize) -> Option<gpui::SharedString> {
        match self {
            List::Zone => None,
            List::Font => Some(fonts::options()[k].family().into()),
        }
    }

    /// Choice `k` as the setting's value.
    fn value(self, k: usize) -> String {
        match self {
            List::Zone => display_zone::options()[k].describe(),
            List::Font => fonts::options()[k].describe().to_owned(),
        }
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_rem_size(crate::zoom::rem());
        self.win_w = f32::from(window.viewport_size().width);
        let t = self.theme;
        // What the trading core reports (in force, waiting, lockouts) as of this frame; a
        // busy cache keeps the last frame's.
        if let Some(f) = facts_now(cx, false) {
            self.facts = f;
        }
        self.seen_revision = Self::trading_revision(cx);
        self.check_answers(cx);
        let prefs = current_prefs(cx);
        // Parts hidden while their guardrail is off (↑↓ skip them too).
        self.dormant = schema::all()
            .iter()
            .enumerate()
            .filter(|(_, st)| schema::dormant(st, &prefs, &self.target) == Some(None))
            .map(|(i, _)| i)
            .collect();
        let rows = self.rows();
        if self.row >= rows.len() {
            self.row = rows.len().saturating_sub(1);
        }
        let sel = rows.get(self.row).copied();
        // The Connections cards read the keychain off-thread, once they are first shown.
        if !self.conn.started()
            && let Some(l) = schema::index_of(schema::CONNECTIONS_LIST)
            && rows.contains(&l)
        {
            self.conn.reload(None);
        }
        let searching = !self.query.trim().is_empty();
        let mut main = div()
            .id("settings-main")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px_3()
            .pt_2()
            // Room under the last row: its controls never sit on the footer's rule.
            .pb_4();
        // The page's title; on Accounts, the one account picker beside it, what the account
        // is, and the tighter/looser rule once under them.
        let mut head = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_x_2()
            .gap_y_1()
            .pb_1()
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::BOLD)
                    .text_size(crate::zoom::ui(15.0))
                    .child(if searching {
                        format!("{} results", schema::line_count(&rows))
                    } else {
                        self.section.name().to_owned()
                    }),
            );
        // A search with per-account matches: which account (and product) they mean.
        let per_account = |scopes: &[Scope]| {
            rows.iter()
                .any(|&i| scopes.contains(&schema::all()[i].scope))
        };
        let accounts_page = !searching && self.section == Section::Accounts;
        if accounts_page || (searching && per_account(&[Scope::Account, Scope::AccountProduct])) {
            head = head.child(self.account_field(cx));
            if searching && per_account(&[Scope::AccountProduct]) {
                head = head.child(self.product_field(cx));
            }
            if let Some(kind) = self.account_kind() {
                head = head.child(crate::trade_bar::account_badge(kind, &t));
            }
            if let Some(facts) = self.account_facts() {
                head = head.child(
                    div()
                        .flex_none()
                        .text_size(crate::zoom::ui(12.0))
                        .text_color(t.text_dim)
                        .child(facts),
                );
            }
        }
        main = main.child(head);
        // Children before the rows: the selected line's index, for scrolling it into view.
        let mut child = 1;
        if accounts_page {
            main = main.child(
                div()
                    .pb_1()
                    .text_size(crate::zoom::ui(12.0))
                    .text_color(t.text_dim)
                    .child(RULE_NOTE),
            );
            child += 1;
        }
        let mut selected_child = None;
        let mut keys_first = None;
        // Blocks shown after a line: the preset editor, the time zone's and the UI font's
        // lists, the keys.
        let extras = |v: &Self, keys: &[usize], cx: &mut Context<Self>| -> Vec<AnyElement> {
            let mut out = Vec::new();
            for &i in keys {
                match schema::all()[i].key {
                    "trading.brackets" => out.extend(v.editor_view()),
                    "keys.list" => out.extend(crate::keys_view::list(&v.keys, &v.theme, cx)),
                    // Settings › Connections: a card per source (decision 0030).
                    k if k == schema::CONNECTIONS_LIST => {
                        out.push(crate::connections_view::list(&v.conn, &v.theme, cx))
                    }
                    // Settings › Plugins: a card per installed plugin.
                    k if k == schema::PLUGINS_LIST => {
                        out.push(crate::plugins_view::list(&v.plugins, &v.theme, cx))
                    }
                    _ => {}
                }
            }
            out
        };
        if searching {
            if rows.is_empty() {
                main = main.child(div().text_color(t.text_dim).child("No setting matches"));
            }
            // Each match under its section's header (and its group's), so none is read out
            // of context.
            let mut header = String::new();
            // The line last drawn (its first key): two matches on one line draw it once.
            let mut last: Option<&'static str> = None;
            for &i in &rows {
                let Some(place) = schema::places()[i] else {
                    continue;
                };
                let first = place.line.keys().next();
                if first == last {
                    if sel == Some(i) {
                        selected_child = Some(child - 1);
                    }
                    continue;
                }
                last = first;
                let h = schema::search_header(i);
                if h != header {
                    main = main.child(self.group_title(&h, false, cx));
                    child += 1;
                    header = h;
                }
                if sel == Some(i) {
                    selected_child = Some(child);
                }
                let label = schema::line_label(&place.line);
                main = main.child(self.line_view(label, place.line.parts, false, &prefs, cx));
                child += 1;
                for e in extras(self, &[i], cx) {
                    main = main.child(e);
                    child += 1;
                }
            }
        } else {
            for g in schema::page(self.section) {
                if !g.title.is_empty() {
                    main = main.child(self.group_title(g.title, g.product, cx));
                    child += 1;
                }
                for line in g.lines {
                    let keys: Vec<usize> = line
                        .keys()
                        .filter_map(schema::index_of)
                        .filter(|&i| self.on_page(i))
                        .collect();
                    if keys.is_empty() {
                        continue;
                    }
                    if sel.is_some_and(|s| keys.contains(&s)) {
                        selected_child = Some(child);
                    }
                    let label = schema::line_label(line);
                    main = main.child(self.line_view(label, line.parts, line.sub, &prefs, cx));
                    child += 1;
                    if line.keys().any(|k| k == "keys.list") {
                        keys_first = Some(child);
                    }
                    for e in extras(self, &keys, cx) {
                        main = main.child(e);
                        child += 1;
                    }
                }
            }
        }
        // Settings › Keys: the focused (or capturing) row in view once it moved.
        if self.keys.reveal
            && let (Some(first), Some(i)) = (keys_first, self.keys.at.get())
            && self.scroll.bounds().size.height > px(0.0)
        {
            self.scroll.scroll_to_item(first + i);
            self.keys.reveal = false;
            self.scrolled = Some((self.section, self.row, self.query.clone()));
        } else if self.keys.reveal && keys_first.is_some() && self.keys.at.get().is_some() {
            // Not laid out yet: the next frame.
            cx.notify();
        }
        let at = (self.section, self.row, self.query.clone());
        // Only once the list has been laid out (a first frame has no viewport yet, and
        // scrolling against none would jump past the row): the next frame does it then.
        if let Some(c) = selected_child
            && self.scrolled.as_ref() != Some(&at)
            && self.scroll.bounds().size.height > px(0.0)
        {
            // The first row: the top (its page's title and account line too).
            if self.row == 0 {
                self.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
            } else {
                self.scroll.scroll_to_item(c);
            }
            self.scrolled = Some(at);
            self.scroll_wait = None;
        } else if selected_child.is_some() && self.scrolled.as_ref() != Some(&at) {
            // Not laid out yet: scroll on a next frame (a row opened below the fold), a few
            // frames at most.
            let tries = match &self.scroll_wait {
                Some((w, n)) if *w == at => *n,
                _ => 0,
            };
            if tries < 4 {
                self.scroll_wait = Some((at, tries + 1));
                cx.notify();
            }
        }
        // An open dropdown, laid out once: its lit row near the middle.
        if let Some(d) = self.drop.as_mut()
            && let Some(n) = d.reveal_wait
        {
            if d.scroll.bounds().size.height > px(0.0) {
                drop_view::reveal(&d.scroll, &d.list, true);
                d.reveal_wait = None;
            } else if n < 4 {
                d.reveal_wait = Some(n + 1);
                cx.notify();
            }
        }
        let search = div()
            .flex_1()
            .ml_3()
            .px_2()
            .rounded_sm()
            .border_1()
            .border_color(if searching { t.accent } else { t.axis_line })
            .bg(t.panel)
            .text_color(if searching { t.text_main } else { t.text_dim })
            .child(if searching {
                format!("{}▏", self.query)
            } else {
                "Search settings".to_owned()
            });
        div()
            .id("settings")
            .size_full()
            // The UI font (`crate::fonts`): every text in the window inherits it.
            .font_family(crate::fonts::ui_family())
            .flex()
            .flex_col()
            .bg(t.bg)
            .text_color(t.text_main)
            .text_size(crate::zoom::ui(13.0))
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
                    .child(search),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(self.nav(&prefs, cx))
                    .child(
                        // A page longer than the window fades into the bottom bar, so a row
                        // cut there reads as more below, not as a broken control.
                        div().relative().flex().flex_1().min_w_0().child(main).when(
                            // More below the visible part (the offset is negative, down to
                            // -max_offset), not merely a page longer than the window.
                            self.scroll.max_offset().y > px(0.0)
                                && -self.scroll.offset().y < self.scroll.max_offset().y - px(1.0),
                            |d| {
                                d.child(
                                    div()
                                        .absolute()
                                        .left_0()
                                        .right_0()
                                        .bottom_0()
                                        .h(crate::zoom::ui(FADE_H))
                                        .bg(gpui::linear_gradient(
                                            180.0,
                                            // The background, fully transparent: a
                                            // scroll cue, not a UI state (no state token).
                                            gpui::linear_color_stop(
                                                gpui::Hsla { a: 0.0, ..t.bg },
                                                0.0,
                                            ),
                                            gpui::linear_color_stop(t.bg, 1.0),
                                        )),
                                )
                            },
                        ),
                    ),
            )
            .child(
                // Only the two keys a row doesn't show: undo and reset (Esc still closes).
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(t.axis_line)
                    .text_size(crate::zoom::ui(11.5))
                    .text_color(t.text_dim)
                    .child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .child("Ctrl+Z undo · Ctrl+R reset"),
                    ),
            )
    }
}

/// An Info value short enough to sit on its line's right ("Not locked out"); a longer one
/// (a path, the accounts list) goes under the label.
fn inline_text(s: &str) -> bool {
    !s.contains('\n') && s.chars().count() <= 40
}

/// A line's width outside its content (100 % logical px): the nav and its border, the main
/// column's padding (px_3 each side), the line's edge slot, its three gaps (gap_2), its right
/// padding (pr_2), ● and ↺. All of it scales with the UI zoom.
const LINE_CHROME_W: f32 = NAV_W + 1.0 + 2.0 * 12.0 + 2.0 + 3.0 * 8.0 + 8.0 + DOT_W + RESET_W;

/// The width a line's controls get on a line of their own (100 % px), in a window `win_w`
/// logical px wide at UI zoom `z` (0: not laid out yet, the default size).
fn content_line_w(win_w: f32, z: f32) -> f32 {
    let w = if win_w > 0.0 { win_w } else { SIZE.0 };
    w / z.max(0.1) - LINE_CHROME_W
}

/// The − / + pair and the field between them at its narrowest (100 % px: two 20 px buttons,
/// the gaps, the field).
const STEPPER_W: f32 = 2.0 * 20.0 + 2.0 * 4.0 + NUMBER_STEP_W;

/// Whether number setting `i` gets − / + in a window `win_w` wide at zoom `z`: it is its line's
/// only field (a line of two numbers and words is typed), and the pair fits beside the label's
/// narrowest column, so the row keeps its one line (at 150 % in the default window it doesn't:
/// the field alone, typed, as before).
fn steps_fit(i: usize, win_w: f32, z: f32) -> bool {
    let st = &schema::all()[i];
    let fields = |l: &schema::Line| {
        l.parts
            .iter()
            .filter(|p| matches!(p, schema::Part::Field(..)))
            .count()
    };
    let alone = schema::page(st.section)
        .iter()
        .flat_map(|g| g.lines.iter())
        .find(|l| {
            l.parts
                .iter()
                .any(|p| matches!(p, schema::Part::Field(k, _) if *k == st.key))
        })
        .is_none_or(|l| fields(l) == 1);
    alone && content_line_w(win_w, z) - LABEL_MIN_W - 8.0 >= STEPPER_W
}

/// A chip group's width estimated from its labels (100 % px): 9 px a character (the 13 px
/// UI text measures about 8 px a character in IBM Plex Sans, a bold chip and digits a little
/// more), each chip's padding (px_2) with 2 px to spare, and the group's border. Wide on
/// purpose: a group counted too wide only moves under its label a little early; one counted too
/// narrow could wrap where its row is measured on one line.
fn choice_w(chips: &[&str]) -> f32 {
    chips
        .iter()
        .map(|c| c.chars().count() as f32 * 9.0 + 18.0)
        .sum::<f32>()
        + 2.0
}

/// Whether a choice takes its own line under its label: wider than `line_w`, the line its
/// controls get on their own ([`content_line_w`]), so its chips would wrap. (Light palette's
/// first chip names the dark palette, "Same as dark (Standard)": the longest it can read is
/// counted.)
fn long_choice(chips: &[&str], line_w: f32) -> bool {
    let shown: Vec<&str> = chips
        .iter()
        .map(|c| {
            if *c == "Same as dark" {
                "Same as dark (Colorblind)"
            } else {
                c
            }
        })
        .collect();
    choice_w(&shown) > line_w
}

/// A product multi-select's trigger: "None", "ES, NQ", "ES, NQ, +3".
fn products_summary(l: &[String]) -> String {
    match l {
        [] => "None".to_owned(),
        [a] => a.clone(),
        [a, b] => format!("{a}, {b}"),
        [a, b, rest @ ..] => format!("{a}, {b}, +{}", rest.len()),
    }
}

/// A multi-select of a few named options: "All", "Orders, Bid", "3 of 4", "None".
fn picks_summary(l: &[String], of: usize) -> String {
    match l.len() {
        0 => "None".to_owned(),
        n if n == of => "All".to_owned(),
        1 | 2 => l.join(", "),
        n => format!("{n} of {of}"),
    }
}

/// Editors whose summary sits beside their button ("8/16, 12/24 ticks  Edit…"); the rest
/// (the data folder's path, the reset's account, the rollback) go under the label.
fn summary_beside(key: &str) -> bool {
    key == "trading.brackets" || key == schema::PROP_OVERRIDES_KEY
}

#[cfg(test)]
mod tests {
    /// A palette row not in use outlines its choice in accent: 3:1 against the row it sits on
    /// (plain, or the selected row's panel) in every theme (WCAG 1.4.11).
    #[test]
    fn idle_choice_outline_is_3_to_1() {
        use crate::theme::{Theme, contrast};
        for t in Theme::all() {
            for (what, bg) in [("bg", t.bg), ("panel", t.panel)] {
                let r = contrast(t.accent, bg);
                assert!(r >= 3.0, "accent on {what}: {r:.2} ({:?})", t.is_dark());
            }
        }
    }

    use super::*;

    /// Which rows' chips take their own line: every choice of buttons with more chips than fit
    /// beside a label at the narrowest window (a dropdown is one field, never long). Named
    /// here, so a new long choice is a decision.
    #[test]
    fn long_choices_take_their_own_line() {
        let long = |win: f32, z: f32| -> Vec<&str> {
            let line = content_line_w(win, z);
            schema::all()
                .iter()
                .filter(|s| {
                    matches!(s.kind, Kind::Choice(c) if !schema::drops_down(c) && long_choice(c, line))
                })
                .map(|s| s.key)
                .collect()
        };
        // At the default size and zoom, no buttons wrap (Bar period's 23 are a dropdown).
        assert!(long(SIZE.0, 1.0).is_empty(), "{:?}", long(SIZE.0, 1.0));
        // The narrowest window at 150 %: the button groups wider than their line.
        let line = content_line_w(MIN_SIZE.0, 1.5);
        assert!((150.0..200.0).contains(&line), "{line}");
        let narrow = long(MIN_SIZE.0, 1.5);
        for k in ["trading.pnl", "guard.news"] {
            assert!(narrow.contains(&k), "{k}");
        }
        for k in [
            "general.updates",
            "appearance.light_palette",
            "appearance.ui_zoom",
            "charts.timeframe",
            "prop.preset",
        ] {
            assert!(!narrow.contains(&k), "{k} is a dropdown");
        }
        // At the default size a short group keeps its place beside the label.
        assert!(!long_choice(
            &["Dark", "Light", "System"],
            content_line_w(SIZE.0, 1.0)
        ));
        // UI zoom's six chips (about 280 px drawn) are counted at least that wide.
        assert!(choice_w(&crate::zoom::LABELS) >= 280.0);
    }

    /// The audited rows open dropdowns, lit at the value in force: Bar period grouped with
    /// "Custom…" last; the depth products and the DOM's columns several at once.
    #[test]
    fn dropdowns_list_their_rows() {
        let prefs = Prefs::default();
        let at = |key: &str| schema::index_of(key).unwrap();
        let rows = |key: &str, v: Value| SettingsWindow::drop_rows(at(key), &v, &prefs).unwrap();
        let tf = rows("charts.timeframe", Value::Choice(3));
        assert_eq!(tf.lit, 3);
        assert!(tf.custom && !tf.multi);
        let groups: Vec<_> = tf.entries.iter().map(|e| e.group.unwrap()).collect();
        let mut seen = groups.clone();
        seen.dedup();
        assert_eq!(
            seen,
            [
                "Minutes", "Hours", "Days", "Ticks", "Seconds", "Volume", "Range"
            ],
            "each group once, in order"
        );
        // A typed timeframe lights "Custom…".
        assert_eq!(rows("charts.timeframe", Value::Text("750t".into())).lit, 23);
        assert!(rows("dom.grouping", Value::Choice(0)).custom);
        assert!(!rows("appearance.ui_zoom", Value::Choice(2)).custom);
        let cols = rows("dom.columns", Value::List(vec!["Bid".into(), "Ask".into()]));
        assert!(cols.multi);
        let on = Value::List(vec!["Bid".into(), "Ask".into()]);
        let keys = SettingsWindow::drop_keys(at("dom.columns"), &on);
        let checked: Vec<bool> = (0..4)
            .map(|k| SettingsWindow::drop_checked(&on, &keys, k))
            .collect();
        assert_eq!(checked, [false, true, true, false]);
        assert!(rows("data.depth", Value::List(vec!["ES".into()])).multi);
        // Rows keep their product when the saved set shrinks under the open list: unknown
        // symbols sit last, so removing one would move the rows after it.
        let saved = Value::List(vec!["ES".into(), "ZZ1".into(), "ZZ2".into()]);
        let keys = SettingsWindow::drop_keys(at("data.depth"), &saved);
        let (zz1, zz2) = (keys.len() - 2, keys.len() - 1);
        assert_eq!((&*keys[zz1], &*keys[zz2]), ("ZZ1", "ZZ2"));
        let after = Value::List(vec!["ES".into(), "ZZ2".into()]);
        assert!(!SettingsWindow::drop_checked(&after, &keys, zz1));
        assert!(SettingsWindow::drop_checked(&after, &keys, zz2));
        // Buttons stay buttons.
        for key in [
            "appearance.theme",
            "appearance.palette",
            "trading.pnl",
            "guard.news",
        ] {
            let v = (schema::all()[at(key)].get)(&prefs, &Target::default(), &Facts::default());
            assert!(
                SettingsWindow::drop_rows(at(key), &v, &prefs).is_none(),
                "{key}"
            );
        }
    }

    #[test]
    fn summaries_name_a_few_and_count_the_rest() {
        let l = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(products_summary(&[]), "None");
        assert_eq!(products_summary(&l(&["ES", "NQ"])), "ES, NQ");
        assert_eq!(
            products_summary(&l(&["ES", "NQ", "CL", "GC", "MES"])),
            "ES, NQ, +3"
        );
        assert_eq!(
            picks_summary(&l(&["Orders", "Bid", "Ask", "Volume"]), 4),
            "All"
        );
        assert_eq!(picks_summary(&l(&["Bid", "Ask", "Volume"]), 4), "3 of 4");
        assert_eq!(picks_summary(&l(&["Bid", "Ask"]), 4), "Bid, Ask");
        assert_eq!(picks_summary(&[], 4), "None");
    }

    /// − / + go beside a lone number when they fit by the label's narrowest column: at 100 %
    /// in every window size, never at 150 % in the default one (the field alone keeps the row
    /// on one line); never on a line of two numbers.
    #[test]
    fn steppers_only_where_they_fit() {
        let at = |key: &str| schema::index_of(key).unwrap();
        let daily = at("guard.max_daily_loss");
        assert!(steps_fit(daily, SIZE.0, 1.0));
        assert!(steps_fit(daily, MIN_SIZE.0, 1.0));
        assert!(!steps_fit(daily, SIZE.0, 1.5));
        assert!(
            !steps_fit(at("guard.news_before"), SIZE.0, 1.0),
            "two numbers"
        );
    }

    /// At its smallest the window leaves a row's label column its full width, and the widest
    /// line's controls (the news window: two numbers and their words) fit their own line
    /// below it with the ● and ↺ slots (they wrap there; the label never collapses).
    #[test]
    fn the_smallest_window_keeps_the_label_column() {
        // The nav and its border, the main column's padding (px_3), the line's edge slot, its
        // gap and right padding (pr_2).
        let body = MIN_SIZE.0 - NAV_W - 1.0 - 2.0 * 12.0 - 2.0 - 8.0 - 8.0;
        const { assert!(MIN_SIZE.0 <= SIZE.0 && MIN_SIZE.1 <= SIZE.1) };
        // Two number fields, "before," and "after" (about 7 px a letter), the gaps, ● and ↺.
        let words = 13.0 * 7.0;
        let group = 2.0 * NUMBER_W + words + 4.0 * 8.0 + DOT_W + RESET_W + 2.0 * 4.0;
        assert!(body >= LABEL_MIN_W, "{body}");
        assert!(
            body >= group,
            "the controls fit their own line: {body} < {group}"
        );
    }

    /// The window's search keeps every match under its own section's header: the headers,
    /// in order, never repeat, and each match's header is its place's.
    #[test]
    fn search_results_keep_their_headers() {
        let hits = schema::search("loss");
        assert!(hits.len() > 2);
        let headers: Vec<String> = hits.iter().map(|&i| schema::search_header(i)).collect();
        let mut seen: Vec<&String> = Vec::new();
        for h in &headers {
            if seen.last() != Some(&h) {
                assert!(!seen.contains(&h), "{h} shows twice: {headers:?}");
                seen.push(h);
            }
        }
        assert!(
            headers.iter().any(|h| h == "Accounts › Limits"),
            "{headers:?}"
        );
        assert!(headers.iter().all(|h| !h.is_empty()));
    }

    use std::collections::BTreeMap;

    /// A refused change goes back by the entry it wrote (name or id key), and not over a
    /// change made since.
    #[test]
    fn a_refusal_puts_back_only_what_it_wrote() {
        let saved: BTreeMap<String, i64> = [("paper-eval".to_owned(), 1_000)].into();
        let mut after = saved.clone();
        after.insert("paper-eval".into(), 100);
        after.insert("Other".into(), 5);
        let wrote = changed_entries(&saved, &after);
        assert_eq!(
            wrote,
            vec![
                ("Other".to_owned(), None, Some(5)),
                ("paper-eval".to_owned(), Some(1_000), Some(100)),
            ],
            "the entries changed, by their own keys"
        );
        let mut now = after.clone();
        put_back(&mut now, wrote.clone());
        assert_eq!(now, saved, "both entries back as they were");
        // Rewritten since: left to that change.
        let mut now = after.clone();
        now.insert("paper-eval".into(), 700);
        put_back(&mut now, wrote);
        assert_eq!(now.get("paper-eval"), Some(&700));
        assert_eq!(now.get("Other"), None);
    }
}
