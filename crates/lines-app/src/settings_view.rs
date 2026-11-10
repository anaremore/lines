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
//! Ctrl+R resets, Ctrl+Z undoes; a loosening is confirmed with Y (Esc cancels).

use std::time::Instant;

use gpui::{
    AnyElement, App, AppContext, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Render, StatefulInteractiveElement, Styled,
    TitlebarOptions, Window, WindowHandle, WindowOptions, div, prelude::FluentBuilder, px,
};

use crate::brackets::{self, EditorKey};
use crate::display_zone;
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
    /// The time zone's searchable list, while open (`crate::display_zone`).
    zone_pick: Option<display_zone::Pick>,
    /// The settings list's scroll (keeps the selected row in view).
    scroll: gpui::ScrollHandle,
    /// Risk changes the trading core hasn't answered yet, oldest first.
    awaiting: Vec<Awaiting>,
    /// The trading accounts' revisions last shown (the watcher repaints when they move).
    seen_revision: u64,
    /// The row last scrolled into view (only a new selection or search scrolls).
    scrolled: Option<(Section, usize, String)>,
}

/// The window's size (logical px).
pub const SIZE: (f32, f32) = (860.0, 620.0);
/// The smallest the window can be made (logical px): every section's rows stay readable.
pub const MIN_SIZE: (f32, f32) = (640.0, 420.0);
/// A row's label and description never get narrower than this (logical px): a control that
/// doesn't fit beside them moves to its own line below, and a choice's chips wrap.
pub const LABEL_MIN_W: f32 = 240.0;
/// A scope chip's widest (an account's name and product: "…" past it).
const SCOPE_MAX_W: f32 = 160.0;
/// The section list's width (logical px).
const NAV_W: f32 = 150.0;

#[cfg(feature = "screenshot")]
fn section_named(name: &str) -> Option<Section> {
    Section::ALL
        .into_iter()
        .find(|s| s.name().eq_ignore_ascii_case(name))
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
        (Kind::Choice(_), n) => Value::Choice(
            n.parse()
                .map_err(|_| format!("{s:?}: a choice's position"))?,
        ),
        (Kind::Number { .. }, n) => {
            Value::Number(schema::parse_number(n).map_err(|e| format!("{s:?}: {e}"))?)
        }
        _ => return Err(format!("--settings-set {s:?}: not settable this way")),
    };
    Ok((i, value, sure))
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
    // `account=NAME` picks the window's account (by name or id), `appearance.time_zone=list:Q`
    // opens the time zone's list with Q typed; the rest are settings.
    for s in set
        .iter()
        .filter(|s| !s.starts_with("account=") && !s.starts_with(ZONE_LIST))
    {
        parse_set(s).map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

/// `--settings-set` prefix: the time zone's list, open, with the rest typed in it.
#[cfg(feature = "screenshot")]
const ZONE_LIST: &str = "appearance.time_zone=list:";

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
        for s in &set {
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
            if let Some(q) = s.strip_prefix(ZONE_LIST) {
                if let Some(i) = schema::index_of(display_zone::KEY) {
                    v.select(i);
                    v.open_zone_pick(cx);
                    if let Some(p) = v.zone_pick.as_mut() {
                        for ch in q.chars() {
                            p.key("", Some(ch.encode_utf8(&mut [0; 4])));
                        }
                    }
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
            let _ = commit(&before, &after, &target, cx);
            Ok(Some(format!("{}: {}", st.name, st.value_text(&v))))
        }
        None => {
            let i = c.index as usize;
            open(Some(i), cx);
            if st.key == crate::display_zone::KEY
                && let Some(h) = cx.try_global::<SettingsWindowGlobal>().and_then(|g| g.0)
            {
                // The time zone's palette command opens its list, ready to type in.
                let _ = h.update(cx, |w, _, cx| {
                    w.open_zone_pick(cx);
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
            hide_hints: ws.is_some_and(|w| w.hide_hints),
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
    if after.app.time_zone != before.app.time_zone {
        let problem = crate::display_zone::apply(&lines_core::display_tz::ZoneChoice::from_saved(
            after.app.time_zone.as_deref(),
        ));
        if let Some(p) = problem {
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
    if after.ws.hide_hints != before.ws.hide_hints {
        crate::workspace::set_hide_hints(after.ws.hide_hints, cx);
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
    let mut f = Facts::default();
    if let Some(ws) = cx.try_global::<Workspace>()
        && let Some(st) = &ws.env.base.store
    {
        f.store_path = st.root.display().to_string();
        f.store_budget_gb =
            Some((st.budget_bytes as f64 / (1u64 << 30) as f64 * 10.0).round() / 10.0);
        f.depth_products = st.depth_products.clone();
    } else {
        f.store_path = "Not recording (this source has no store)".to_owned();
    }
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
            zone_pick: None,
            scroll: gpui::ScrollHandle::new(),
            scrolled: None,
            awaiting: Vec::new(),
            seen_revision: 0,
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
                    let r = Self::trading_revision(cx);
                    if r != v.seen_revision {
                        v.seen_revision = r;
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
        self.edit = None;
        self.cancel_pending();
        self.editor = None;
        self.editor_msg = None;
        self.zone_pick = None;
    }

    /// Show setting `i`: its section, selected, the search cleared.
    fn show(&mut self, i: usize) {
        if let Some(st) = schema::all().get(i) {
            self.drop_unfinished();
            self.query.clear();
            self.section = st.section;
            self.row = self.rows().iter().position(|&r| r == i).unwrap_or(0);
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
        let (g0, g1) = (
            self.target.guardrails(&before),
            self.target.guardrails(&after),
        );
        let prop = st.key.starts_with("prop.");
        let loosens = schema::loosening(st, &old, &v, &g0, &g1)
            || (prop && schema::prop_loosens(&before, &after, &self.target, &self.facts));
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
            let (from, to) = (st.value_text(&old), st.value_text(&v));
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
        let text = if guard {
            if loosens {
                format!(
                    "Loosened {} → {}: takes effect at the next session.",
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

    /// Open the time zone's list, on the zone in force.
    fn open_zone_pick(&mut self, cx: &App) {
        let saved = settings(cx).time_zone.clone();
        self.zone_pick = Some(display_zone::Pick::new(
            &lines_core::display_tz::ZoneChoice::from_saved(saved.as_deref()),
        ));
    }

    /// A key while the time zone's list is open: it filters, moves, chooses or closes.
    fn zone_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let Some(p) = self.zone_pick.as_mut() else {
            return;
        };
        match p.key(k.key.as_str(), k.key_char.as_deref()) {
            display_zone::PickKey::Typing | display_zone::PickKey::Ignored => {}
            display_zone::PickKey::Cancel => self.zone_pick = None,
            display_zone::PickKey::Choose(c) => self.choose_zone(&c, cx),
        }
        cx.notify();
    }

    fn choose_zone(&mut self, c: &lines_core::display_tz::ZoneChoice, cx: &mut Context<Self>) {
        self.zone_pick = None;
        if let Some(i) = schema::index_of(display_zone::KEY) {
            self.set(i, Value::Text(c.describe()), false, cx);
        }
    }

    /// The editor a setting of kind `Editor` opens.
    fn open_editor_for(&mut self, i: usize, cx: &App) {
        if schema::all()[i].key == display_zone::KEY {
            self.open_zone_pick(cx);
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
        if self.editor.is_some() {
            self.editor_key(ev, cx);
            return;
        }
        if self.zone_pick.is_some() {
            self.zone_key(ev, cx);
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
            (Kind::Editor, _) if st.key == schema::RESET_KEY => self.reset_press(i, false, cx),
            (Kind::Editor, _) if st.key == schema::ROLLBACK_KEY => self.rollback_press(cx),
            (Kind::Editor, _) => self.open_editor_for(i, cx),
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
            .w(crate::zoom::ui(NAV_W))
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
                    .children(
                        count.map(|n| div().text_size(crate::zoom::ui(11.0)).child(n.to_string())),
                    )
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
                // Wraps onto more lines when the row is narrower than its chips.
                let mut seg = div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .min_w_0()
                    .border_1()
                    .border_color(t.axis_line)
                    .rounded_sm();
                for (j, name) in c.iter().enumerate() {
                    let on = j == *k;
                    seg = seg.child(
                        div()
                            .id(("choice", i * 64 + j))
                            .flex_none()
                            .whitespace_nowrap()
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
                // Off | On, the current one filled: a state, never a lone button.
                let mut seg = div()
                    .flex()
                    .flex_row()
                    .flex_none()
                    .border_1()
                    .border_color(t.axis_line)
                    .rounded_sm();
                for (j, (name, val)) in [("Off", false), ("On", true)].into_iter().enumerate() {
                    let on = val == *b;
                    seg = seg.child(
                        div()
                            .id(("toggle", i * 2 + j))
                            .px_2()
                            .cursor_pointer()
                            .when(on, |d| d.bg(t.accent).text_color(t.bg))
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
            (Kind::Number { unit, .. }, Value::Number(n)) => {
                let editing = selected && self.edit.is_some();
                let text = match (&self.edit, editing) {
                    (Some(e), true) => format!("{e}▏"),
                    _ => n.map_or_else(|| "None".to_owned(), schema::grouped),
                };
                let unit = if n.is_some() || editing { unit } else { "" };
                let mut d = div()
                    .id(("number", i))
                    .flex_none()
                    .min_w(crate::zoom::ui(72.0))
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
            (Kind::Editor, _) if schema::all()[i].key == schema::RESET_KEY => {
                self.reset_buttons(i, cx)
            }
            (Kind::Editor, _) if schema::all()[i].key == schema::ROLLBACK_KEY => {
                match self.facts.rollback.clone() {
                    None => div().into_any_element(),
                    Some(v) => div()
                        .id(("rollback", i))
                        .flex_none()
                        .whitespace_nowrap()
                        .px_2()
                        .rounded_sm()
                        .border_1()
                        .border_color(t.axis_line)
                        .cursor_pointer()
                        .child(format!("Roll back to v{v}"))
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
            (Kind::Editor, _) => div()
                .id(("editor", i))
                .flex_none()
                .px_2()
                .rounded_sm()
                .border_1()
                .border_color(t.axis_line)
                .cursor_pointer()
                .child(if schema::all()[i].key == display_zone::KEY {
                    "Change…"
                } else {
                    "Edit…"
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v, _, _, cx| {
                        v.select(i);
                        v.open_editor_for(i, cx);
                        cx.notify();
                    }),
                )
                .into_any_element(),
            _ => div().into_any_element(),
        }
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
        // Wraps in the label column (a search's section prefix, the name, the TRADING chip).
        let mut name = div().flex().flex_row().flex_wrap().items_center().gap_x_2();
        if searching {
            name = name.child(
                div()
                    .text_size(crate::zoom::ui(11.0))
                    .text_color(t.text_dim)
                    .child(format!("{} ›", st.section.name())),
            );
        }
        name = name.child(div().text_color(t.text_main).child(st.name));
        if st.safety == Safety::Trading {
            name = name.child(
                div()
                    .flex_none()
                    .px_1()
                    .text_size(crate::zoom::ui(10.0))
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
            .flex_grow_1()
            .flex_basis(crate::zoom::ui(LABEL_MIN_W))
            .min_w(crate::zoom::ui(LABEL_MIN_W))
            .child(name)
            .child(
                div()
                    .text_size(crate::zoom::ui(11.5))
                    .text_color(t.text_dim)
                    .child(st.description),
            );
        if let (Kind::Info, Value::Text(s)) = (st.kind, &value) {
            // A lockout or breach is the row's warning (by its words and the lock, not color
            // alone).
            let warn = s.starts_with('🔒');
            left = left.child(
                div()
                    .text_color(if warn { t.warn } else { t.text_main })
                    .child(s.clone()),
            );
        }
        // What is in force now, when the core holds a looser value back.
        if let Some(line) = schema::status(st, prefs, &self.target, &self.facts) {
            left = left.child(
                div()
                    .text_size(crate::zoom::ui(11.5))
                    .text_color(t.warn)
                    .child(line),
            );
        }
        if let (Kind::Editor, Value::Text(s)) = (st.kind, &value) {
            left = left.child(
                div()
                    .text_size(crate::zoom::ui(11.5))
                    .text_color(t.text_main)
                    .child(s.clone()),
            );
        }
        // A reset waiting for its second press says what that press will do.
        if st.key == schema::RESET_KEY
            && let Some(text) = self.reset_confirm(cx)
        {
            left = left.child(
                div()
                    .text_size(px(11.5))
                    .text_color(t.warn)
                    // Armed shows by weight too, not only by color.
                    .font_weight(FontWeight::BOLD)
                    .child(text),
            );
        }
        if let Some((n, text, tone)) = self.note.as_ref().filter(|n| n.0 == i) {
            let _ = n;
            left = left.child(
                div()
                    .text_size(crate::zoom::ui(11.5))
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
            .when(selected, |d| {
                d.bg(t.panel).border_l_2().border_color(t.accent)
            })
            .when(st.safety == Safety::Trading, |d| {
                d.border_l_2().border_color(t.warn)
            })
            // A trading row keeps its warn bar; its selection shows as an accent bar beside
            // it, in a slot every trading row has (selecting never moves the text).
            .when(st.safety == Safety::Trading, |d| {
                d.child(
                    div()
                        .w(crate::zoom::ui(3.0))
                        .self_stretch()
                        .flex_none()
                        .when(selected, |b| b.bg(t.accent)),
                )
            })
            .child(
                // Label and description, then the control group: beside them when it fits,
                // else on its own line below (right-aligned). Nothing squeezes the label.
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap_x_2()
                    .gap_y_1()
                    .child(left)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_start()
                            .gap_2()
                            .ml_auto()
                            .min_w_0()
                            .max_w_full()
                            .child(self.control(i, &value, cx))
                            // An account's name can be long: "…" past its share of the row.
                            .child(
                                div()
                                    // Never narrower than its words; "…" only past the cap.
                                    .flex_none()
                                    .max_w(crate::zoom::ui(SCOPE_MAX_W))
                                    .truncate()
                                    .px_1()
                                    .text_size(crate::zoom::ui(10.5))
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(t.axis_line)
                                    .text_color(t.text_dim)
                                    .child(scope),
                            )
                            .child(
                                div()
                                    .id(("reset", i))
                                    // A 20 px hit, the minimum.
                                    .w(crate::zoom::hit(20.0))
                                    .flex()
                                    .justify_center()
                                    .flex_none()
                                    .text_color(t.text_dim)
                                    .when(changed, |d| {
                                        crate::tooltip::tip_div(
                                            d,
                                            "Reset to default (Ctrl+R)",
                                            None,
                                            &t,
                                        )
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
                            ),
                    ),
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
                    .child(div().w(crate::zoom::ui(90.0)).child("Stop"))
                    .child(div().w(crate::zoom::ui(90.0)).child("Target"))
                    .child(div().w(crate::zoom::ui(140.0)).child("Name")),
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
                .text_size(crate::zoom::ui(11.5))
                .text_color(if problem.is_some() { t.warn } else { t.text_dim })
                .child(problem.unwrap_or_else(|| {
                    "Enter saves · Esc cancels · Insert adds · Delete removes · Ctrl+↑↓ reorders · Tab moves".to_owned()
                })),
        );
        Some(v.into_any_element())
    }

    /// The time zone's list: what's typed, then the matching zones around the selection.
    fn zone_view(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = self.theme;
        let p = self.zone_pick.as_ref()?;
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
            .child(
                div()
                    .px_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(t.axis_line)
                    .child(format!("Search zones: {}▏", p.query)),
            );
        let top = p.top();
        for (r, &k) in p.hits.iter().enumerate().skip(top).take(display_zone::ROWS) {
            let on = r == p.sel;
            let choice = &display_zone::options()[k];
            let mut row = div()
                .id(("zone", k))
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(if on {
                    t.accent
                } else {
                    gpui::transparent_black()
                })
                .cursor_pointer()
                .child(display_zone::option_text(choice));
            if on {
                row = row.font_weight(FontWeight::BOLD);
            }
            v = v.child(row.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, _, _, cx| {
                    v.choose_zone(&display_zone::options()[k], cx);
                    cx.notify();
                }),
            ));
        }
        let shown = p.hits.len().min(display_zone::ROWS);
        v = v.child(
            div()
                .text_size(crate::zoom::ui(11.5))
                .text_color(t.text_dim)
                .child(if p.hits.is_empty() {
                    "No zone matches · Backspace edits · Esc closes".to_owned()
                } else {
                    format!(
                        "{shown} of {} · ↑↓ choose · Enter sets · Esc closes",
                        p.hits.len()
                    )
                }),
        );
        Some(v.into_any_element())
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_rem_size(crate::zoom::rem());
        let t = self.theme;
        // What the trading core reports (in force, waiting, lockouts) as of this frame; a
        // busy cache keeps the last frame's.
        if let Some(f) = facts_now(cx, false) {
            self.facts = f;
        }
        self.seen_revision = Self::trading_revision(cx);
        self.check_answers(cx);
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
            .pt_2()
            // Room under the last row: its controls never sit on the footer's rule.
            .pb_4()
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
                // Wraps in a narrow window; a long account name ends in "…" (the window's width).
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_x_2()
                    .gap_y_1()
                    .pb_1()
                    .text_size(crate::zoom::ui(12.0))
                    .text_color(t.text_dim)
                    .child(
                        div()
                            .id("account-step")
                            .min_w_0()
                            .max_w_full()
                            .truncate()
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
            if schema::all()[i].key == display_zone::KEY
                && let Some(e) = self.zone_view(cx)
            {
                main = main.child(e);
                child += 1;
            }
            if schema::all()[i].key == "keys.list" {
                main = main.child(self.keys_list());
                child += 1;
            }
        }
        let at = (self.section, self.row, self.query.clone());
        if let Some(c) = selected_child
            && self.scrolled.as_ref() != Some(&at)
        {
            // The first row: the top (its section's title and account line too).
            if self.row == 0 {
                self.scroll.set_offset(gpui::point(px(0.0), px(0.0)));
            } else {
                self.scroll.scroll_to_item(c);
            }
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
                    .child(search)
                    .child(div().text_color(t.text_dim).text_size(crate::zoom::ui(12.0)).child("Esc closes")),
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
                // Two whole texts: the second wraps below the first in a narrow window.
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .justify_between()
                    .gap_x_4()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(t.axis_line)
                    .text_size(crate::zoom::ui(11.5))
                    .text_color(t.text_dim)
                    .child(div().flex_none().whitespace_nowrap().child(
                        "↑↓ settings · ←→ change · Enter edit · Tab sections · Ctrl+Z undo · Ctrl+R reset",
                    ))
                    .child(div().flex_none().whitespace_nowrap().child(
                        if self.section == Section::Risk && self.query.trim().is_empty() {
                            "Tighter applies at once · looser waits for the next session"
                        } else {
                            "Changes apply at once · every setting is also a palette command"
                        },
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// At its smallest the window leaves a row's label column its full width beside the
    /// widest scope chip and the reset slot (the control may wrap below; the label never
    /// collapses to letters).
    #[test]
    fn the_smallest_window_keeps_the_label_column() {
        // The nav and its border, the main column's padding (px_3), the row's (px_2), a trading
        // row's bar, its gap and the row's border.
        let body = MIN_SIZE.0 - NAV_W - 1.0 - 2.0 * 12.0 - 2.0 * 8.0 - 3.0 - 8.0 - 2.0;
        const { assert!(MIN_SIZE.0 <= SIZE.0 && MIN_SIZE.1 <= SIZE.1) };
        // A number control, the widest scope chip, the reset slot and the gaps.
        let group = 72.0 + SCOPE_MAX_W + 20.0 + 2.0 * 8.0;
        assert!(body >= LABEL_MIN_W, "{body}");
        assert!(body >= group, "the control group fits its own line: {body}");
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
