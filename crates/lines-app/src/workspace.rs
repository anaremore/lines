//! The live workspace (decision 0016): windows → tabs → a [`Layout`] of independent panes.
//!
//! - **[`Workspace`]** (a GPUI global) holds what spans windows: the pane environment (source,
//!   theme), the workspace's name, crosshair sync, the open windows and autosave.
//! - **[`WindowView`]** is each OS window's root: a tab bar, the active tab's panes laid out
//!   from [`Layout::place`] (so drawing and hit-testing share one geometry), and overlays (the
//!   context menu, the save/open prompt).
//! - **Panes** are cached views. Only the active tab of a visible window is rendered, and every
//!   other pane is told it is hidden so its poll stops notifying: hidden tabs and minimised
//!   windows do no paint work.
//!
//! Panes never reference each other. A pane that switches symbol emits `FeedChanged`; the
//! workspace hands the new feed to every pane in the same link group, in every window. A chart
//! emits `CrosshairTime`; with crosshair sync on, every other chart marks that time.
//!
//! Keys go through one capture-phase handler on the window root: `crate::commands` maps them to
//! [`Command`]s, so the keyboard, the context menu and the tab bar all run the same code.
//! Everything that decides (layout surgery, focus movement, tab drops, window placement,
//! persistence) is pure and tested elsewhere (`layout`, `wsdoc`, `versioned`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui::{
    AnyElement, AnyView, App, AppContext, Bounds, Context, DisplayId, Entity, FocusHandle,
    FontWeight, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, StyleRefinement, Styled,
    Subscription as GpuiSubscription, TitlebarOptions, Window, WindowBounds, WindowHandle,
    WindowOptions, div, point, prelude::*, px, relative, size,
};
use lines_core::{Price, UtcMillis};
use lines_feed::FeedConfig;
use lines_feed::availability::Availability;

use crate::chain_view::ChainView;
use crate::chart_view::{self, ChartView, CrosshairTime, Fixture};
use crate::commands::{self, Command, StrategyCmd};
use crate::dom_view::{DomOptions, DomView};
use crate::feeds::{Feeds, PaneFeed};
use crate::journal_view::JournalView;
use crate::layout::{
    Axis, Direction, Layout, LinkGroup, PaneKind, Preset, Rect, next_link, trading_group,
};
use crate::menu::{Menu, MenuItem, MenuKey};
use crate::notice::Notices;
use crate::pane::{FeedChanged, Pane};
use crate::pane_columns::{ColumnCmd, ColumnMenu, Columns};
use crate::pane_window::{self, PaneWindow};
use crate::persist::{self, Writer};
use crate::strategy_view::StrategyView;
use crate::tabstate::{self, Leaf, PaneState};
use crate::tape_view::{TapeOptions, TapeView};
use crate::theme::Theme;
use crate::versioned;
use crate::wsdoc::StudyVisibility;
use crate::wsdoc::{
    self, DisplayInfo, PaneDoc, Placement, TabDoc, TabDrop, WindowDoc, WindowState, WorkspaceDoc,
};

const TAB_BAR_H_100: f32 = 28.0;

/// [`TAB_BAR_H_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn tab_bar_h() -> f32 {
    crate::zoom::f(TAB_BAR_H_100)
}
const HINT_H_100: f32 = 22.0;
/// [`HINT_H_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn hint_h() -> f32 {
    crate::zoom::f(HINT_H_100)
}
/// The shortest window a person can drag to: room for two DOMs stacked. Narrow enough for any
/// layout that works today.
const MIN_WINDOW_W: f32 = crate::trade_bar::MIN_DOM_WIDTH;
const MIN_WINDOW_H: f32 = TAB_BAR_H_100 + HINT_H_100 + 2.0 * crate::trade_bar::MIN_DOM_PANE_H;
const TAB_W_100: f32 = 168.0;
/// [`TAB_W_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn tab_w() -> f32 {
    crate::zoom::f(TAB_W_100)
}
const TAB_X0_100: f32 = 4.0;
/// [`TAB_X0_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn tab_x0() -> f32 {
    crate::zoom::f(TAB_X0_100)
}
/// Width of a divider's grab handle (the visible line is one pixel).
const GRAB: f32 = 6.0;
/// A tab drag starts after this much movement.
const DRAG_START: f32 = 6.0;
/// The top of a pane that is its header: dragging there pulls the pane out of the window.
const HEADER_H: f32 = 22.0;
/// Closed tabs kept for "Reopen closed tab".
const CLOSED_TABS: usize = 10;
/// A docked pane's neighbour keeps this share.
const DOCK_SHARE: f32 = 0.72;

// ---- environment and the app-wide state -----------------------------------------------------

/// What every pane is created from: the run's source and look.
pub struct PaneEnv {
    /// The run's source; panes set the target.
    pub base: FeedConfig,
    pub availability: Availability,
    /// The look every pane is created with; the Settings window changes it live
    /// ([`set_theme`]).
    pub theme: std::cell::Cell<Theme>,
    /// This run's look flags (`--theme`, `--palette`, `--hollow`): over Settings ›
    /// Appearance until the trader sets that field; never saved.
    pub look_flags: std::cell::Cell<crate::settings_schema::Appearance>,
    /// Screenshot mode: frozen clock, no pointer, no user files.
    pub frozen: bool,
    /// Follow window visibility (off for screenshots and the benchmark, whose windows are
    /// never shown but must paint).
    pub track_visibility: bool,
    /// Defaults for new panes (`--dom-grouping`, `--large-print`).
    pub dom: DomOptions,
    pub tape: TapeOptions,
    /// `--record`: given to the first pane that starts a feed.
    pub record: RefCell<Option<PathBuf>>,
    /// Screenshot fixture: given to the first chart that has data.
    pub fixture: RefCell<Option<Fixture>>,
    /// Window size for windows without a saved placement (`--size`), else a share of the
    /// primary display.
    pub default_size: Option<(f32, f32)>,
    /// What the source shows when a pane's own symbol can't be served (adopt / suggest).
    pub fallback: crate::feeds::Fallback,
}

struct Autosave {
    path: PathBuf,
    /// Bytes last written (unchanged snapshots aren't rewritten).
    last: Vec<u8>,
}

/// Everything that spans windows.
pub struct Workspace {
    pub env: Rc<PaneEnv>,
    pub name: String,
    pub crosshair_sync: bool,
    /// Crosshair sync includes the price between charts of the same product.
    pub crosshair_sync_price: bool,
    /// The key-hint bar is hidden in every window (workspace preference; shown by default).
    pub hide_hints: bool,
    pub windows: Vec<WindowHandle<WindowView>>,
    /// Windows holding one detached pane each (the pane stays in its tab's layout).
    pub pane_windows: Vec<WindowHandle<PaneWindow>>,
    /// Recently closed tabs with the index they had (in-memory only).
    closed_tabs: tabstate::ClosedTabs<TabDoc>,
    /// The chart that last had focus in a main window (where "Dock beside chart" goes).
    last_chart: Option<PaneId>,
    /// What's wrong with a trigger, if anything (`chain_lines::trigger_status`), kept fresh by
    /// `start_trigger_watch`: true when it can't fire.
    trigger_status: Option<(String, bool)>,
    /// Open positions on every listed account, for the footer's `N open` (kept fresh by the
    /// trigger watch, as a position changes with no window redrawing).
    open_positions: Option<usize>,
    /// Problems and refusals shown inline in the footer (`crate::notice`: repeats counted,
    /// refusals expire, problems that need action stay until dismissed).
    pub notices: Notices,
    /// Show the chart header's performance stats (feed rate, paints/s, paint time): a
    /// workspace preference, off by default.
    pub show_perf_stats: bool,
    /// Show the latency HUD in chart headers (roadmap 6b): off by default. Mirrored in
    /// `lines_core::latency::enabled` (nothing records while it is off).
    pub show_latency_hud: bool,
    autosave: Option<Autosave>,
    config_dir: Option<PathBuf>,
    writer: Option<Writer>,
}

impl gpui::Global for Workspace {}

impl Workspace {
    pub fn new(env: PaneEnv) -> Workspace {
        Workspace {
            env: Rc::new(env),
            name: "default".into(),
            crosshair_sync: false,
            crosshair_sync_price: false,
            hide_hints: false,
            windows: Vec::new(),
            pane_windows: Vec::new(),
            closed_tabs: tabstate::ClosedTabs::new(CLOSED_TABS),
            last_chart: None,
            trigger_status: None,
            open_positions: None,
            notices: Notices::default(),
            show_perf_stats: false,
            show_latency_hud: false,
            autosave: None,
            config_dir: None,
            writer: None,
        }
    }

    /// Save to `<dir>/workspace.json` every couple of seconds when it changed, and on quit.
    /// Named workspaces are saved into `<dir>/workspaces/`.
    pub fn with_files(mut self, dir: PathBuf, writer: Writer, autosave: bool) -> Workspace {
        if autosave {
            self.autosave = Some(Autosave {
                path: persist::workspace_path(&dir),
                last: Vec::new(),
            });
        }
        self.config_dir = Some(dir);
        self.writer = Some(writer);
        self
    }

    /// Where settings and workspace files live (diagnostics bundles read them from here).
    pub fn config_dir(&self) -> Option<&std::path::Path> {
        self.config_dir.as_deref()
    }

    /// A problem the trader needs to act on (e.g. a workspace file that couldn't be read):
    /// shown until dismissed. Refused actions go through [`notice`], which expires.
    pub fn add_report(&mut self, r: String) {
        tracing::warn!("{r}");
        self.notices.push(r, true, Instant::now());
    }
}

fn env(cx: &App) -> Rc<PaneEnv> {
    cx.global::<Workspace>().env.clone()
}

// ---- panes ----------------------------------------------------------------------------------

/// A pane's identity for the lifetime of the app (unique across windows, so tabs can move).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(pub(crate) u64);

static NEXT_PANE: AtomicU64 = AtomicU64::new(1);

impl PaneId {
    fn next() -> PaneId {
        PaneId(NEXT_PANE.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone)]
pub enum PaneHandle {
    Chart(Entity<ChartView>),
    Dom(Entity<DomView>),
    Tape(Entity<TapeView>),
    Journal(Entity<JournalView>),
    Strategies(Entity<StrategyView>),
    Chain(Entity<ChainView>),
    Positions(Entity<crate::positions_view::PositionsView>),
}

macro_rules! each {
    ($h:expr, $e:ident => $body:expr) => {
        match $h {
            PaneHandle::Chart($e) => $body,
            PaneHandle::Dom($e) => $body,
            PaneHandle::Tape($e) => $body,
            PaneHandle::Journal($e) => $body,
            PaneHandle::Strategies($e) => $body,
            PaneHandle::Chain($e) => $body,
            PaneHandle::Positions($e) => $body,
        }
    };
}

impl PaneHandle {
    /// A new look (the Settings window): the pane repaints with it.
    pub(crate) fn set_theme(&self, theme: Theme, cx: &mut App) {
        each!(self, e => e.update(cx, |v, cx| {
            v.set_theme(theme);
            cx.notify();
        }));
        if let PaneHandle::Chain(e) = self {
            e.update(cx, |v, cx| v.set_chart_theme(theme, cx));
        }
    }

    pub(crate) fn view(&self) -> AnyView {
        each!(self, e => e.clone().into())
    }

    pub fn kind(&self) -> PaneKind {
        match self {
            PaneHandle::Chart(_) => PaneKind::Chart,
            PaneHandle::Dom(_) => PaneKind::Dom,
            PaneHandle::Tape(_) => PaneKind::Tape,
            PaneHandle::Journal(_) => PaneKind::Journal,
            PaneHandle::Strategies(_) => PaneKind::Strategies,
            PaneHandle::Chain(_) => PaneKind::Chain,
            PaneHandle::Positions(_) => PaneKind::Positions,
        }
    }

    fn set_feed(&self, feed: PaneFeed, cx: &mut App) {
        each!(self, e => e.update(cx, |p, cx| p.set_feed(feed, cx)))
    }

    /// Taking typed text now (symbol search, a note, a timeframe being typed).
    pub(crate) fn takes_text(&self, cx: &App) -> bool {
        if let PaneHandle::Chain(c) = self
            && c.read(cx).chart_takes_text(cx)
        {
            return true;
        }
        each!(self, e => e.read(cx).takes_text())
    }

    /// Whether it is shown, frames painted, and (charts) smoothed paint time in µs.
    pub(crate) fn perf(&self, cx: &mut App) -> (bool, u64, Option<f64>) {
        let us = match self {
            PaneHandle::Chart(e) => Some(e.read(cx).paint_us()),
            _ => None,
        };
        each!(self, e => e.update(cx, |p, _| {
            let c = p.common();
            (c.visible, c.paints, us)
        }))
    }

    pub(crate) fn feed(&self, cx: &App) -> PaneFeed {
        each!(self, e => e.read(cx).feed().clone())
    }

    fn link(&self, cx: &mut App) -> LinkGroup {
        each!(self, e => e.update(cx, |p, _| p.common().link))
    }

    fn set_link(&self, link: LinkGroup, cx: &mut App) {
        each!(self, e => e.update(cx, |p, cx| {
            p.common().link = link;
            cx.notify();
        }))
    }

    pub(crate) fn set_visible(&self, visible: bool, cx: &mut App) {
        each!(self, e => e.update(cx, |p, _| p.common().visible = visible));
        // A chain's chart polls with it: a hidden pane does no work.
        if let PaneHandle::Chain(e) = self {
            e.update(cx, |v, cx| v.sync_chart_visible(cx));
        }
    }

    /// Whether the pane can't be served (and the source has a symbol to offer instead).
    fn switchable(&self, cx: &App) -> bool {
        let f = self.feed(cx);
        f.unavailable.is_some() && f.suggest.is_some()
    }

    fn set_offer(&self, offer: bool, cx: &mut App) {
        each!(self, e => e.update(cx, |p, cx| {
            if p.common().offer_switch != offer {
                p.common().offer_switch = offer;
                cx.notify();
            }
        }))
    }

    fn show_suggested(&self, cx: &mut App) {
        each!(self, e => e.update(cx, |p, cx| p.show_suggested(cx)))
    }

    pub fn paints(&self, cx: &mut App) -> u64 {
        each!(self, e => e.update(cx, |p, _| p.common().paints))
    }

    /// Call `f` whenever this pane notifies (asks for a redraw). The benchmark's frame demand.
    pub fn on_notify(&self, cx: &mut App, f: impl Fn() + 'static) -> gpui::Subscription {
        each!(self, e => cx.observe(e, move |_, _| f()))
    }

    pub(crate) fn focus(&self, window: &mut Window, cx: &mut App) {
        each!(self, e => {
            let h = e.read(cx).focus_handle().clone();
            h.focus(window, cx);
        })
    }

    /// A column command on a DOM or time & sales.
    fn column_command(&self, c: ColumnCmd, cx: &mut App) -> Result<(), &'static str> {
        match self {
            PaneHandle::Dom(e) => e.update(cx, |v, cx| v.column_command(c, cx)),
            PaneHandle::Tape(e) => e.update(cx, |v, cx| v.column_command(c, cx)),
            PaneHandle::Journal(e) => e.update(cx, |v, cx| v.column_command(c, cx)),
            PaneHandle::Chart(_)
            | PaneHandle::Strategies(_)
            | PaneHandle::Chain(_)
            | PaneHandle::Positions(_) => {
                Err("columns belong to a DOM, time & sales or the journal")
            }
        }
    }

    /// The pane's columns, if it has a table.
    fn columns(&self, cx: &App) -> Option<Columns> {
        match self {
            PaneHandle::Dom(e) => Some(e.read(cx).columns().clone()),
            PaneHandle::Tape(e) => Some(e.read(cx).columns().clone()),
            PaneHandle::Journal(e) => Some(e.read(cx).columns().clone()),
            PaneHandle::Chart(_)
            | PaneHandle::Strategies(_)
            | PaneHandle::Chain(_)
            | PaneHandle::Positions(_) => None,
        }
    }

    fn set_synced_time(&self, t: Option<UtcMillis>, price: Option<Price>, cx: &mut App) {
        if let PaneHandle::Chart(e) = self {
            e.update(cx, |c, cx| {
                if c.common().visible {
                    c.set_synced_time(t, price, cx);
                }
            });
        }
    }

    /// The pane's saved form.
    fn doc(&self, cx: &mut App) -> PaneDoc {
        each!(self, e => e.update(cx, |p, _| {
            let mut d = PaneDoc::new(self.kind(), p.common().link, p.feed().target_doc.clone());
            p.save_state(&mut d);
            d
        }))
    }

    /// The product symbol it shows (tab titles).
    fn symbol(&self, cx: &App) -> String {
        each!(self, e => {
            let p = e.read(cx);
            match &p.feed().unavailable {
                Some(u) => u.symbol.clone(),
                None => p.store().try_read().map_or_else(String::new, |d| d.product.symbol.to_owned()),
            }
        })
    }
}

/// A pane in a tab, with the workspace's subscriptions to its events.
struct PaneSlot {
    handle: PaneHandle,
    _subs: Vec<GpuiSubscription>,
}

pub struct Tab {
    layout: Layout<PaneId>,
    panes: HashMap<PaneId, PaneSlot>,
    /// Hidden and detached panes (absent = shown). Their leaves stay in `layout`.
    state: HashMap<PaneId, PaneState>,
    focused: PaneId,
    /// Detached panes whose windows open once the tab is in a window (restore, reopen).
    pending_windows: Vec<(PaneId, Option<Placement>)>,
}

impl Tab {
    pub fn state(&self, id: PaneId) -> PaneState {
        self.state.get(&id).copied().unwrap_or_default()
    }

    fn set_state(&mut self, id: PaneId, s: PaneState) {
        if s == PaneState::Shown {
            self.state.remove(&id);
        } else {
            self.state.insert(id, s);
        }
    }

    fn is_shown(&self, id: &PaneId) -> bool {
        self.state(*id).is_shown()
    }

    /// The least height a split leaves this pane: a DOM keeps its header, account and exits.
    fn min_h(&self, id: &PaneId) -> f32 {
        self.handle(*id).map_or(0.0, |h| {
            crate::zoom::f(crate::trade_bar::min_pane_h(h.kind()))
        })
    }

    /// The least width a split leaves this pane. Above 100 % UI zoom a DOM keeps the width its
    /// trading panel is laid out for, at the zoom (`trade_bar::MIN_DOM_WIDTH`); at 100 % and
    /// below, nothing (layouts are as before).
    fn min_w(&self, id: &PaneId) -> f32 {
        if crate::zoom::get() <= 1.0 {
            return 0.0;
        }
        self.handle(*id)
            .filter(|h| h.kind() == PaneKind::Dom)
            .map_or(0.0, |_| crate::zoom::f(crate::trade_bar::MIN_DOM_WIDTH))
    }

    fn order(&self) -> Vec<PaneId> {
        self.layout.leaves().into_iter().copied().collect()
    }

    /// What `crate::tabstate` decides from.
    fn leaves(&self, cx: &mut App) -> HashMap<PaneId, Leaf> {
        self.panes
            .iter()
            .map(|(id, s)| {
                (
                    *id,
                    Leaf {
                        kind: s.handle.kind(),
                        link: s.handle.link(cx),
                        state: self.state(*id),
                    },
                )
            })
            .collect()
    }

    /// Panes of this tab in `state`, in layout order.
    fn in_state(&self, f: impl Fn(PaneState) -> bool) -> Vec<PaneId> {
        self.order()
            .into_iter()
            .filter(|id| f(self.state(*id)))
            .collect()
    }

    fn handle(&self, id: PaneId) -> Option<&PaneHandle> {
        self.panes.get(&id).map(|s| &s.handle)
    }

    fn focused_handle(&self) -> Option<&PaneHandle> {
        self.handle(self.focused)
    }

    pub fn handles(&self) -> impl Iterator<Item = &PaneHandle> {
        self.panes.values().map(|s| &s.handle)
    }

    fn title(&self, cx: &App) -> String {
        let mut seen: Vec<String> = Vec::new();
        for id in self.layout.leaves() {
            if let Some(h) = self.handle(*id) {
                let s = h.symbol(cx);
                if !s.is_empty() && !seen.contains(&s) {
                    seen.push(s);
                }
            }
        }
        if seen.is_empty() {
            "empty".into()
        } else {
            seen.join(" · ")
        }
    }

    fn doc(&self, cx: &mut App) -> TabDoc {
        let mut docs = HashMap::new();
        for (id, slot) in &self.panes {
            let mut d = slot.handle.doc(cx);
            d.state = self.state(*id);
            if d.state == PaneState::Detached {
                d.window = pane_window::placement_of(*id, cx);
            }
            docs.insert(*id, d);
        }
        let layout = self.layout.map(&mut |id| {
            docs.remove(id)
                .unwrap_or_else(|| PaneDoc::new(PaneKind::Chart, None, None))
        });
        TabDoc {
            focused: self.layout.index_of(&self.focused).unwrap_or(0),
            layout,
        }
    }
}

/// Create a pane from its saved form.
fn make_pane(doc: &PaneDoc, cx: &mut App) -> PaneHandle {
    let env = env(cx);
    let registry = Feeds::registry(cx);
    let mut base = env.base.clone();
    base.record = env.record.borrow().clone();
    let feed = PaneFeed::open(
        &registry,
        &base,
        &env.availability,
        doc.target.as_ref(),
        &env.fallback,
    );
    if feed.sub.is_some() {
        // The recording belongs to the first pane that started a feed (one contract per file).
        env.record.borrow_mut().take();
    }
    let available = feed.unavailable.is_none();
    let theme = env.theme.get();
    let handle = match doc.kind {
        PaneKind::Chart => {
            let fixture = if available {
                env.fixture.borrow_mut().take().unwrap_or_default()
            } else {
                Fixture::default()
            };
            let frozen = env.frozen;
            PaneHandle::Chart(cx.new(|cx| {
                let mut v = ChartView::new(feed, theme, frozen, fixture, cx);
                match &doc.chart {
                    Some(s) => v.restore(s),
                    // A new chart: the studies saved as the default for new charts.
                    None => v.apply_new_chart_defaults(cx),
                }
                v.restore_trade(doc.trade.as_ref(), cx);
                v
            }))
        }
        PaneKind::Dom => {
            let mut opts = env.dom.clone();
            // New DOMs follow Settings › DOM unless a flag was given (a saved DOM's own
            // state wins either way).
            let np = &crate::picker_view::settings(cx).new_panes;
            if !opts.grouping_from_cli {
                opts.grouping = np.dom_grouping.unwrap_or(opts.grouping);
            }
            if !opts.trail_from_cli {
                opts.trail |= np.dom_trail;
            }
            let frozen = env.frozen;
            PaneHandle::Dom(cx.new(|cx| {
                let mut v = DomView::new(feed, theme, frozen, opts, cx);
                if let Some(s) = &doc.dom {
                    v.restore(s);
                }
                v.restore_trade(doc.trade.as_ref(), cx);
                v
            }))
        }
        PaneKind::Tape => {
            let opts = env.tape.clone();
            let frozen = env.frozen;
            PaneHandle::Tape(cx.new(|cx| {
                let mut v = TapeView::new(feed, theme, frozen, opts, cx);
                if let Some(s) = &doc.tape {
                    v.restore(s);
                }
                v
            }))
        }
        PaneKind::Journal => {
            let frozen = env.frozen;
            PaneHandle::Journal(cx.new(|cx| {
                let mut v = JournalView::new(feed, theme, frozen, cx);
                if let Some(s) = &doc.journal {
                    v.restore(s);
                }
                v
            }))
        }
        PaneKind::Positions => PaneHandle::Positions(
            cx.new(|cx| crate::positions_view::PositionsView::new(feed, theme, cx)),
        ),
        PaneKind::Strategies => {
            let frozen = env.frozen;
            PaneHandle::Strategies(cx.new(|cx| StrategyView::new(feed, theme, frozen, cx)))
        }
        PaneKind::Chain => {
            let frozen = env.frozen;
            PaneHandle::Chain(cx.new(|cx| {
                let mut v = ChainView::new(feed, theme, frozen, cx);
                v.restore_trade(doc.trade.as_ref(), cx);
                if doc.chain_chart {
                    v.toggle_chart(cx);
                }
                v
            }))
        }
    };
    handle.set_link(doc.link, cx);
    handle
}

// ---- windows --------------------------------------------------------------------------------

struct TabDrag {
    index: usize,
    start: Point<Pixels>,
    now: Point<Pixels>,
    active: bool,
}

enum Prompt {
    SaveAs {
        text: String,
        error: Option<String>,
    },
    Open {
        items: Vec<String>,
        selected: usize,
        error: Option<String>,
    },
    /// Every command, filtered by what is typed (`commands::filter`).
    Palette {
        query: String,
        items: Vec<Command>,
        shown: Vec<Command>,
        selected: usize,
    },
}

/// Dragging a pane by its header (out of the window pops it out).
struct PaneDrag {
    id: PaneId,
    start: Point<Pixels>,
    now: Point<Pixels>,
    active: bool,
}

pub struct WindowView {
    tabs: Vec<Tab>,
    active: usize,
    theme: Theme,
    focus: FocusHandle,
    /// This window's id: its handle must never be read while this view updates.
    window_id: gpui::WindowId,
    /// Visible on screen (not minimised); always true without visibility tracking.
    window_visible: bool,
    window_active: bool,
    /// Last known screen bounds (for deciding where a dragged tab lands).
    screen: Rect,
    split_drag: Option<crate::layout::Divider>,
    tab_drag: Option<TabDrag>,
    pane_drag: Option<PaneDrag>,
    /// The pane and tab under the pointer, from geometry on mouse move: only they get hover
    /// controls, so a frame doesn't build (and lay out) controls for every pane.
    hover_pane: Option<PaneId>,
    /// Screenshots (`--hover-header`): the hovered pane is fixed, not the pointer's.
    hover_pinned: bool,
    /// Screenshot: a menu to open once its pane can show it (`--context-menu`).
    shot_menu: Option<crate::pane_menu::ShotMenu>,
    hover_tab: Option<usize>,
    menu: Option<Menu>,
    prompt: Option<Prompt>,
    _window_subs: Vec<GpuiSubscription>,
}

impl WindowView {
    fn new(tabs: Vec<Tab>, active: usize, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let env = env(cx);
        let mut subs = Vec::new();
        if env.track_visibility {
            subs.push(cx.observe_window_visibility(window, |this, vis, _, cx| {
                this.window_visible = vis.is_visible();
                this.sync_visibility(cx);
                if this.window_visible {
                    cx.notify();
                }
            }));
        }
        subs.push(cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            cx.notify();
        }));
        subs.push(cx.observe_window_bounds(window, |this, window, cx| {
            this.screen = screen_rect(window);
            // A move to a monitor with another DPI re-renders everything (GPUI refreshes on
            // bounds changes, bypassing view caches); panes work in logical px and snap to the
            // new device pixels on their next paint.
            cx.notify();
        }));
        let mut this = WindowView {
            tabs: Vec::new(),
            active: 0,
            theme: env.theme.get(),
            focus: cx.focus_handle(),
            window_id: window.window_handle().window_id(),
            window_visible: true,
            window_active: true,
            screen: screen_rect(window),
            split_drag: None,
            tab_drag: None,
            pane_drag: None,
            hover_pane: None,
            hover_pinned: false,
            shot_menu: None,
            hover_tab: None,
            menu: None,
            prompt: None,
            _window_subs: subs,
        };
        for tab in tabs {
            this.adopt(tab, cx);
        }
        this.active = active.min(this.tabs.len().saturating_sub(1));
        this.sync_visibility(cx);
        if let Some(h) = this.tabs.get(this.active).and_then(|t| t.focused_handle()) {
            h.focus(window, cx);
        } else {
            this.focus.focus(window, cx);
        }
        this
    }

    /// Build a tab from its saved form.
    fn build_tab(doc: &TabDoc, cx: &mut App) -> Tab {
        let mut panes = HashMap::new();
        let mut state = HashMap::new();
        let mut pending_windows = Vec::new();
        let mut ids = Vec::new();
        let layout = doc.layout.map(&mut |p| {
            let id = PaneId::next();
            ids.push((id, p.clone()));
            id
        });
        for (id, p) in ids {
            if p.state != PaneState::Shown {
                state.insert(id, p.state);
            }
            if p.state == PaneState::Detached {
                pending_windows.push((id, p.window.clone()));
            }
            panes.insert(
                id,
                PaneSlot {
                    handle: make_pane(&p, cx),
                    _subs: Vec::new(),
                },
            );
        }
        let mut tab = Tab {
            layout,
            panes,
            state,
            focused: PaneId(0),
            pending_windows,
        };
        let order = tab.order();
        let want = order[doc.focused.min(order.len() - 1)];
        // Focus is on a shown pane (`WorkspaceDoc::normalise` keeps one shown).
        tab.focused = order
            .iter()
            .copied()
            .find(|id| *id == want && tab.is_shown(id))
            .or_else(|| order.iter().copied().find(|id| tab.is_shown(id)))
            .unwrap_or(want);
        tab
    }

    /// Take ownership of a tab (new, or moved from another window): subscribe to its panes.
    fn adopt(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let i = self.tabs.len();
        self.adopt_at(i, tab, cx);
    }

    fn adopt_at(&mut self, i: usize, mut tab: Tab, cx: &mut Context<Self>) {
        for (id, slot) in tab.panes.iter_mut() {
            slot._subs = self.subscribe(*id, &slot.handle, cx);
        }
        let pending: Vec<_> = std::mem::take(&mut tab.pending_windows)
            .into_iter()
            .filter_map(|(id, p)| tab.handle(id).cloned().map(|h| (id, h, p)))
            .collect();
        self.tabs.insert(i.min(self.tabs.len()), tab);
        if !pending.is_empty() {
            cx.defer(move |cx| {
                for (id, h, p) in pending {
                    if let Err(e) = pane_window::open(id, h, p.as_ref(), cx) {
                        cx.global_mut::<Workspace>()
                            .add_report(format!("couldn't open a pane window: {e:#}"));
                    }
                }
            });
        }
    }

    /// A chart's exit from a position handle: sent only when a DOM linked to that chart, on the
    /// same account and contract, has trading enabled (a chart shows no switch of its own).
    fn chart_exit(
        &mut self,
        _id: PaneId,
        chart: gpui::Entity<crate::chart_view::ChartView>,
        ev: &crate::chart_view::ChartOrder,
        cx: &mut Context<Self>,
    ) {
        use crate::trade_bar::TradePane;
        // Link groups span every window (decision 0016): look at the DOMs in all of them.
        let windows = cx.global::<Workspace>().windows.clone();
        let mut doms: Vec<gpui::Entity<crate::dom_view::DomView>> = Vec::new();
        for w in &windows {
            // This view is being updated: reading its window through the handle would panic
            // (GPUI), so its own DOMs come from `self.tabs` below.
            if w.window_id() == self.window_id {
                continue;
            }
            if let Ok(wv) = w.read(cx) {
                doms.extend(
                    wv.tabs
                        .iter()
                        .flat_map(|t| t.handles())
                        .filter_map(|h| match h {
                            PaneHandle::Dom(d) => Some(d.clone()),
                            _ => None,
                        }),
                );
            }
        }
        // This window is being updated (it is not readable through its handle): its own DOMs.
        doms.extend(
            self.tabs
                .iter()
                .flat_map(|t| t.handles())
                .filter_map(|h| match h {
                    PaneHandle::Dom(d) => Some(d.clone()),
                    _ => None,
                }),
        );
        let mut seen = Vec::with_capacity(doms.len());
        for d in doms {
            let l = PaneHandle::Dom(d.clone()).link(cx);
            let v = d.read(cx);
            let p = v.trade_ref();
            seen.push((
                l,
                p.account.clone(),
                crate::trade_bar::contract_of(v.trade_store()),
                p.arming.is_armed(),
            ));
        }
        let ok = crate::chart_trade::linked_dom_enabled(
            ev.link,
            &ev.account,
            ev.contract,
            seen.iter().map(|(l, a, k, on)| (*l, a.as_ref(), *k, *on)),
        );
        match crate::trading::Trading::try_global(cx) {
            Some(t) if ok => {
                let id = t.submit(lines_trading::Source::Chart, ev.issued, ev.command.clone());
                if let Some((order, price, qty)) = ev.resize {
                    chart.update(cx, |c, cx| {
                        c.track_resize(order, price, qty, id);
                        cx.notify();
                    });
                }
            }
            _ => chart.update(cx, |c, cx| c.exit_refused(ev.refused, cx)),
        }
    }

    fn subscribe(
        &self,
        id: PaneId,
        handle: &PaneHandle,
        cx: &mut Context<Self>,
    ) -> Vec<GpuiSubscription> {
        let feed_changed =
            move |this: &mut WindowView, ev: &FeedChanged, cx: &mut Context<Self>| {
                this.forward_feed(id, ev, cx)
            };
        match handle {
            PaneHandle::Chart(e) => vec![
                cx.subscribe(e, move |this, _, ev: &FeedChanged, cx| {
                    feed_changed(this, ev, cx)
                }),
                cx.subscribe(e, move |_, _, ev: &CrosshairTime, cx| {
                    forward_crosshair(id, *ev, cx)
                }),
                cx.subscribe(
                    e,
                    move |this, chart, ev: &crate::chart_view::ChartOrder, cx| {
                        this.chart_exit(id, chart, ev, cx)
                    },
                ),
            ],
            PaneHandle::Dom(e) => vec![
                cx.subscribe(e, move |this, _, ev: &FeedChanged, cx| {
                    feed_changed(this, ev, cx)
                }),
                cx.subscribe(e, move |this, _, ev: &ColumnMenu, cx| {
                    this.open_column_menu(id, ev.0, cx)
                }),
            ],
            PaneHandle::Tape(e) => vec![
                cx.subscribe(e, move |this, _, ev: &FeedChanged, cx| {
                    feed_changed(this, ev, cx)
                }),
                cx.subscribe(e, move |this, _, ev: &ColumnMenu, cx| {
                    this.open_column_menu(id, ev.0, cx)
                }),
            ],
            PaneHandle::Journal(e) => vec![
                cx.subscribe(e, move |this, _, ev: &FeedChanged, cx| {
                    feed_changed(this, ev, cx)
                }),
                cx.subscribe(e, move |this, _, ev: &ColumnMenu, cx| {
                    this.open_column_menu(id, ev.0, cx)
                }),
            ],
            PaneHandle::Chain(e) => {
                vec![cx.subscribe(e, move |this, _, ev: &FeedChanged, cx| {
                    feed_changed(this, ev, cx)
                })]
            }
            PaneHandle::Strategies(e) => {
                vec![cx.subscribe(e, move |this, _, ev: &FeedChanged, cx| {
                    feed_changed(this, ev, cx)
                })]
            }
            PaneHandle::Positions(e) => {
                vec![cx.subscribe(e, move |this, _, ev: &FeedChanged, cx| {
                    feed_changed(this, ev, cx)
                })]
            }
        }
    }

    /// Every pane with its tab index and state (the benchmark's paint report).
    pub fn panes_by_state(&self) -> Vec<(usize, PaneState, PaneHandle)> {
        self.tabs
            .iter()
            .enumerate()
            .flat_map(|(i, t)| {
                t.panes
                    .iter()
                    .map(move |(id, s)| (i, t.state(*id), s.handle.clone()))
            })
            .collect()
    }

    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Only the shown panes of the active tab of a visible window are visible. Detached panes
    /// belong to their own windows, which set theirs.
    fn sync_visibility(&mut self, cx: &mut App) {
        for (i, tab) in self.tabs.iter().enumerate() {
            let active = self.window_visible && i == self.active;
            for (id, slot) in &tab.panes {
                match tab.state(*id) {
                    PaneState::Detached => {}
                    s => slot.handle.set_visible(active && s.is_shown(), cx),
                }
            }
        }
        self.sync_offers(cx);
    }

    /// Where each tab offers the switch to the source's symbol: on each unavailable pane, or
    /// once for the tab when every pane needs it (`tabstate::switch_offer`).
    fn sync_offers(&mut self, cx: &mut App) {
        for tab in &self.tabs {
            let order = tab.order();
            let flags: Vec<bool> = order
                .iter()
                .map(|id| tab.handle(*id).is_some_and(|h| h.switchable(cx)))
                .collect();
            let per_pane = tabstate::switch_offer(&flags) != tabstate::Offer::Tab;
            for h in tab.handles() {
                h.set_offer(per_pane, cx);
            }
        }
    }

    /// The active tab offers the switch once, for all its panes.
    fn tab_offer(&self, cx: &App) -> Option<(String, String)> {
        let tab = self.tabs.get(self.active)?;
        let order = tab.order();
        let flags: Vec<bool> = order
            .iter()
            .map(|id| tab.handle(*id).is_some_and(|h| h.switchable(cx)))
            .collect();
        if tabstate::switch_offer(&flags) != tabstate::Offer::Tab {
            return None;
        }
        let f = tab.handle(order[0])?.feed(cx);
        Some((f.unavailable?.reason, f.suggest?.label))
    }

    /// Apply state changes to the active tab: visibility, focus, and pane windows for
    /// newly detached or re-attached panes. `None`: where to put a new detached window.
    fn apply_states(
        &mut self,
        changes: tabstate::Changes<PaneId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (id, s) in changes {
            self.tab_mut().set_state(id, s);
        }
        let tab = self.tab();
        let leaves = tab.leaves(cx);
        let next = tabstate::refocus(&tab.order(), &leaves, tab.focused);
        self.sync_visibility(cx);
        if let Some(f) = next {
            self.focus_pane(f, window, cx);
        }
        cx.notify();
    }

    /// Report a refused action inline (docs/design.md: errors where they happened). It expires
    /// on its own; the same refusal again replaces it with a count.
    fn refuse(&mut self, why: &str, cx: &mut Context<Self>) {
        notice(why.to_owned(), cx);
        cx.notify();
    }

    /// Pane `from` switched feed: the rest of its link group follows, in every window.
    fn forward_feed(&mut self, from: PaneId, ev: &FeedChanged, cx: &mut Context<Self>) {
        let Some(link) = self
            .tabs
            .iter()
            .find_map(|t| t.handle(from))
            .cloned()
            .map(|h| h.link(cx))
        else {
            return;
        };
        cx.notify(); // the tab title
        // A pane that switched may no longer need the switch (or the tab banner).
        self.sync_offers(cx);
        let Some(group) = link else { return };
        let feed = ev.0.clone();
        let windows = cx.global::<Workspace>().windows.clone();
        cx.defer(move |cx| {
            for w in windows {
                let _ = w.update(cx, |wv, _, cx| {
                    wv.apply_group_feed(group, from, &feed, cx);
                    wv.sync_offers(cx);
                    cx.notify();
                });
            }
        });
    }

    fn apply_group_feed(&mut self, group: u8, except: PaneId, feed: &PaneFeed, cx: &mut App) {
        for tab in &self.tabs {
            for (id, slot) in &tab.panes {
                if *id != except && slot.handle.link(cx) == Some(group) {
                    slot.handle.set_feed(feed.clone(), cx);
                }
            }
        }
    }

    /// The tab bar's height: shown only with two tabs or more (one tab needs no bar; the
    /// workspace's status lives in the footer either way).
    fn bar_h(&self) -> f32 {
        if tabstate::shows_tab_bar(self.tabs.len()) {
            tab_bar_h()
        } else {
            0.0
        }
    }

    /// The footer (key hints and workspace status) is shown unless the key hints are hidden
    /// and the status has nothing to say beyond the workspace's name: hidden or popped-out
    /// panes, crosshair sync and reports always bring it back (docs/design.md §3, §7).
    fn footer_shown(&self, cx: &App) -> bool {
        let ws = cx.global::<Workspace>();
        let report = !ws.notices.is_empty() && !ws.windows.is_empty();
        let panes = self.tabs.get(self.active).is_some_and(|t| {
            !t.in_state(PaneState::is_hidden).is_empty()
                || !t.in_state(|s| s == PaneState::Detached).is_empty()
        });
        StudyVisibility::footer_shown(ws.hide_hints, ws.crosshair_sync || report || panes)
    }

    fn content_area(&self, window: &Window, cx: &App) -> Rect {
        let vs = window.viewport_size();
        let (w, h) = (f32::from(vs.width), f32::from(vs.height));
        let foot = if self.footer_shown(cx) { hint_h() } else { 0.0 };
        Rect::new(0.0, 0.0, w, (h - self.bar_h() - foot).max(0.0))
    }

    fn focus_pane(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        let tab = self.tab_mut();
        if tab.panes.contains_key(&id) && tab.focused != id {
            tab.focused = id;
            cx.notify();
        }
        if let Some(h) = self.tab().handle(id).cloned() {
            h.focus(window, cx);
            if h.kind() == PaneKind::Chart {
                cx.global_mut::<Workspace>().last_chart = Some(id);
            }
        }
    }

    fn activate_tab(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if i >= self.tabs.len() {
            return;
        }
        self.active = i;
        self.sync_visibility(cx);
        let f = self.tab().focused;
        self.focus_pane(f, window, cx);
        cx.notify();
    }

    // ---- commands -------------------------------------------------------------------------

    fn run(&mut self, cmd: Command, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        if self.tabs.is_empty() {
            return;
        }
        match cmd {
            Command::Focus(dir) => self.move_focus(dir, window, cx),
            Command::Split(axis) => self.split(axis, window, cx),
            Command::ClosePane => self.close_pane(window, cx),
            Command::SetKind(kind) => self.set_kind(kind, window, cx),
            Command::CycleLink => {
                let h = self.tab().focused_handle().cloned();
                if let Some(h) = h {
                    let l = next_link(h.link(cx));
                    self.set_link(l, window.window_handle().window_id(), cx);
                }
            }
            Command::SetLink(l) => self.set_link(l, window.window_handle().window_id(), cx),
            Command::Preset(p) => self.apply_preset(p, window, cx),
            Command::NewTab => {
                let tab = self.fresh_tab(cx);
                self.adopt(tab, cx);
                let i = self.tabs.len() - 1;
                self.activate_tab(i, window, cx);
            }
            Command::OpenJournal => self.open_panel(PaneKind::Journal, window, cx),
            Command::OpenChain => self.open_panel(PaneKind::Chain, window, cx),
            Command::OpenPositions => self.open_panel(PaneKind::Positions, window, cx),
            Command::ChainChart => match self.tab().focused_handle().cloned() {
                Some(PaneHandle::Chain(c)) => c.update(cx, |v, cx| v.toggle_chart(cx)),
                _ => self.refuse("The chart acts on the focused option chain", cx),
            },
            Command::ChainPnl => match self.tab().focused_handle().cloned() {
                Some(PaneHandle::Chain(c)) => c.update(cx, |v, cx| v.toggle_pnl(cx)),
                _ => self.refuse("P&L to close acts on the focused option chain", cx),
            },
            Command::ChainOneClick => match self.tab().focused_handle().cloned() {
                Some(PaneHandle::Chain(c)) => c.update(cx, |v, cx| v.toggle_one_click(cx)),
                _ => self.refuse("One-click acts on the focused option chain", cx),
            },
            Command::Strategy(StrategyCmd::Open) => {
                self.open_panel(PaneKind::Strategies, window, cx)
            }
            Command::Strategy(StrategyCmd::ToggleMarks) => {
                match self.tab().focused_handle().cloned() {
                    Some(PaneHandle::Chart(chart)) => {
                        chart.update(cx, |c, cx| c.toggle_strategy_marks(cx));
                    }
                    _ => self.refuse("focus a chart first", cx),
                }
            }
            Command::Strategy(c) => self.strategy_command(c, window, cx),
            Command::ExportJournal => export_journal(cx),
            Command::JournalEdit
            | Command::JournalAllDates
            | Command::JournalOpenChart
            | Command::JournalGroup(_) => {
                let Some(PaneHandle::Journal(j)) = self.tab().focused_handle().cloned() else {
                    self.refuse("focus the journal first (Open journal)", cx);
                    return;
                };
                if let Command::JournalGroup(g) = cmd {
                    j.update(cx, |v, cx| v.set_group(g, cx));
                } else if cmd == Command::JournalAllDates {
                    j.update(cx, |v, cx| v.toggle_all_dates(cx));
                } else if cmd == Command::JournalOpenChart {
                    if let Err(e) = j.update(cx, |v, cx| v.open_selected_chart(cx)) {
                        self.refuse(e, cx);
                    }
                } else if let Err(e) = j.update(cx, |v, cx| v.start_edit(cx)) {
                    self.refuse(e, cx);
                }
            }
            Command::CloseTab => self.close_tab(self.active, window, cx),
            Command::ReopenTab => self.reopen_tab(window, cx),
            Command::HidePane => {
                let tab = self.tab();
                match tabstate::hide(&tab.order(), &tab.leaves(cx), tab.focused) {
                    Ok(c) => self.apply_states(c, window, cx),
                    Err(e) => self.refuse(e, cx),
                }
            }
            Command::Toggle(kind) => {
                let tab = self.tab();
                match tabstate::toggle_kind(&tab.order(), &tab.leaves(cx), tab.focused, kind) {
                    Ok(c) => self.apply_states(c, window, cx),
                    Err(e) => self.refuse(e, cx),
                }
            }
            Command::ChartOnly => {
                let tab = self.tab();
                match tabstate::chart_only(&tab.order(), &tab.leaves(cx), tab.focused) {
                    Ok(c) => self.apply_states(c, window, cx),
                    Err(e) => self.refuse(&e, cx),
                }
            }
            Command::ShowHidden => {
                let c = self
                    .tab()
                    .in_state(PaneState::is_hidden)
                    .into_iter()
                    .map(|id| (id, PaneState::Shown))
                    .collect();
                self.apply_states(c, window, cx);
            }
            Command::PopOut => {
                let id = self.tab().focused;
                self.pop_out(id, None, window, cx);
            }
            Command::Reattach => {
                let ids = self.tab().in_state(|s| s == PaneState::Detached);
                if ids.is_empty() {
                    self.refuse("no pane of this tab is popped out", cx);
                }
                for id in ids {
                    self.reattach(id, window, cx);
                }
            }
            Command::ExportDiagnostics => crate::diagnostics::export(window, cx),
            Command::ShowSourceHere => match self.tab().focused_handle().cloned() {
                Some(h) if h.switchable(cx) => h.show_suggested(cx),
                _ => self.refuse("this pane already shows data", cx),
            },
            Command::ShowSourceInTab => {
                let hs: Vec<PaneHandle> = self.tab().handles().cloned().collect();
                let mut any = false;
                for h in hs {
                    if h.switchable(cx) {
                        h.show_suggested(cx);
                        any = true;
                    }
                }
                if !any {
                    self.refuse("every pane here already shows data", cx);
                }
                self.sync_offers(cx);
            }
            Command::Columns(c) => {
                if let Some(h) = self.tab().focused_handle().cloned()
                    && let Err(e) = h.column_command(c, cx)
                {
                    self.refuse(e, cx);
                }
            }
            Command::DockBesideChart => self.refuse(
                "dock beside chart works from a popped-out pane's window",
                cx,
            ),
            Command::Palette => {
                let items = commands::palette();
                self.prompt = Some(Prompt::Palette {
                    query: String::new(),
                    shown: items.clone(),
                    items,
                    selected: 0,
                });
            }
            Command::NextTab => {
                let i = (self.active + 1) % self.tabs.len();
                self.activate_tab(i, window, cx);
            }
            Command::PrevTab => {
                let i = (self.active + self.tabs.len() - 1) % self.tabs.len();
                self.activate_tab(i, window, cx);
            }
            Command::SelectTab(i) => self.activate_tab(i, window, cx),
            Command::TabToNewWindow => {
                if self.tabs.len() > 1 {
                    let at = self.screen;
                    self.tear_off(self.active, (at.x + 40.0, at.y + 40.0), window, cx);
                }
            }
            Command::ToggleCrosshairSync => {
                let ws = cx.global_mut::<Workspace>();
                ws.crosshair_sync = !ws.crosshair_sync;
                if !ws.crosshair_sync {
                    clear_synced_times(cx);
                }
            }
            Command::ToggleCrosshairSyncPrice => {
                let ws = cx.global_mut::<Workspace>();
                ws.crosshair_sync_price = !ws.crosshair_sync_price;
                // Price sync needs sync: turning it on turns sync on too.
                if ws.crosshair_sync_price {
                    ws.crosshair_sync = true;
                }
                clear_synced_times(cx);
            }
            Command::ToggleMagnet => {
                if let Some(PaneHandle::Chart(chart)) = self.tab().focused_handle().cloned() {
                    chart.update(cx, |c, cx| c.toggle_magnet(cx));
                }
            }
            Command::ToggleOrderStrip => {
                if let Some(PaneHandle::Chart(chart)) = self.tab().focused_handle().cloned() {
                    chart.update(cx, |c, cx| c.toggle_order_strip(cx));
                }
            }
            Command::ToggleStudyLabels | Command::ToggleStudies | Command::CleanChart => {
                let Some(PaneHandle::Chart(chart)) = self.tab().focused_handle().cloned() else {
                    return; // chart commands; other panes have no studies
                };
                let v = chart.read(cx).visibility();
                let hints = cx.global::<Workspace>().hide_hints;
                let (v, hints) = match cmd {
                    Command::ToggleStudyLabels => (
                        StudyVisibility {
                            hide_study_labels: !v.hide_study_labels,
                            ..v
                        },
                        hints,
                    ),
                    Command::ToggleStudies => (
                        StudyVisibility {
                            hide_studies: !v.hide_studies,
                            ..v
                        },
                        hints,
                    ),
                    _ => v.clean_chart(hints),
                };
                chart.update(cx, |c, cx| c.set_visibility(v, cx));
                set_hide_hints(hints, cx);
            }
            Command::ToggleHints => {
                let hide = !cx.global::<Workspace>().hide_hints;
                set_hide_hints(hide, cx);
            }
            Command::Studies => {
                if let Some(PaneHandle::Chart(chart)) = self.tab().focused_handle().cloned() {
                    chart.update(cx, |c, cx| c.toggle_studies_menu(cx));
                }
            }
            // Per chart: linked charts keep their own timeframes (links share the market).
            Command::Timeframe(tf) => {
                if let Some(PaneHandle::Chart(chart)) = self.tab().focused_handle().cloned() {
                    chart.update(cx, |c, cx| c.set_timeframe(tf, cx));
                }
            }
            Command::TimeframeMenu => {
                if let Some(PaneHandle::Chart(chart)) = self.tab().focused_handle().cloned() {
                    chart.update(cx, |c, cx| c.toggle_timeframe_menu(cx));
                }
            }
            Command::TickBarsContinuous => {
                if let Some(PaneHandle::Chart(chart)) = self.tab().focused_handle().cloned() {
                    let (on, ticks, tf) = chart.update(cx, |c, cx| {
                        let on = !c.tick_continuous();
                        c.set_tick_continuous(on, cx);
                        (on, c.on_ticks(), c.timeframe())
                    });
                    // A tick chart shows the mode in its chip; a time chart keeps it for its
                    // next tick timeframe, so say so (else the toggle would change nothing
                    // visible).
                    if !ticks {
                        notice(tick_mode_notice(on, tf), cx);
                    }
                } else {
                    notice("tick bars: focus a chart first".into(), cx);
                }
            }
            // An order tag's menu: that order, on the focused chart (where it was opened).
            Command::Order(o) => {
                if let Some(PaneHandle::Chart(chart)) = self.tab().focused_handle().cloned() {
                    chart.update(cx, |c, cx| c.order_command(o, cx));
                }
            }
            // Drawings are on charts; they belong to the chart's product.
            Command::Draw(d) => match self.tab().focused_handle().cloned() {
                Some(PaneHandle::Chart(chart)) => {
                    chart.update(cx, |c, cx| c.drawing_command(d, cx))
                }
                _ => self.refuse("Drawings are on charts: focus a chart first", cx),
            },
            // A source's own switches and its types are in the studies menu.
            Command::ToggleLevels | Command::ToggleLevelLabels => {
                let Some(PaneHandle::Chart(chart)) = self.tab().focused_handle().cloned() else {
                    return; // levels are on charts only
                };
                let mut c = chart.read(cx).levels_config();
                if cmd == Command::ToggleLevels {
                    c.hidden = !c.hidden;
                } else {
                    c.hide_labels = !c.hide_labels;
                }
                chart.update(cx, |v, cx| v.set_levels_config(c, cx));
            }
            Command::TogglePerfStats => {
                let show = !cx.global::<Workspace>().show_perf_stats;
                set_show_perf_stats(show, cx);
            }
            Command::ToggleLatencyHud => {
                let show = !cx.global::<Workspace>().show_latency_hud;
                set_show_latency_hud(show, cx);
            }
            Command::DomTrail | Command::DomTrailHalfLife => {
                let Some(PaneHandle::Dom(dom)) = self.tab().focused_handle().cloned() else {
                    self.refuse("the price trail is on a DOM: focus one first", cx);
                    return;
                };
                if cmd == Command::DomTrailHalfLife {
                    let ms = dom.update(cx, |v, cx| v.cycle_trail_half_life(cx));
                    notice(
                        format!("trail half-life {:.1} s", f64::from(ms) / 1000.0),
                        cx,
                    );
                    cx.notify();
                } else {
                    dom.update(cx, |v, cx| v.toggle_trail(cx));
                }
            }
            Command::Settings => crate::settings_view::open(None, cx),
            Command::Setting(c) => match crate::settings_view::run(c, cx) {
                Ok(Some(text)) => notice(text, cx),
                Ok(None) => {}
                Err(e) => self.refuse(e, cx),
            },
            Command::ZoomIn | Command::ZoomOut | Command::ZoomReset => zoom_command(cmd, cx),
            Command::CyclePnl | Command::SetPnl(_) => {
                let s = cx.global_mut::<crate::picker_view::AppSettings>();
                let m = match cmd {
                    Command::SetPnl(m) => m,
                    _ => s.settings.pnl_display.next(),
                };
                s.set_pnl_display(m);
                cx.notify();
            }
            Command::Trade(tc) => {
                self.focus_for_size(tc, window, cx);
                self.run_trade(tc, false, cx)
            }
            Command::Menu => {
                let area = self.content_area(window, cx);
                let bar = self.bar_h();
                let tab = self.tab();
                let at = tab
                    .layout
                    .index_of(&tab.focused)
                    .and_then(|i| {
                        tab.layout
                            .place_where(area, &|id| tab.is_shown(id), &|id| tab.min_h(id), &|id| {
                                tab.min_w(id)
                            })
                            .into_iter()
                            .find(|p| p.index == i)
                            .map(|p| p.rect)
                    })
                    .map_or(point(crate::zoom::ui(40.0), px(bar + 30.0)), |r| {
                        point(px(r.x + 24.0), px(bar + r.y + 24.0))
                    });
                self.open_menu(at, cx);
            }
            Command::SaveAs => {
                let name = cx.global::<Workspace>().name.clone();
                self.prompt = Some(Prompt::SaveAs {
                    text: if wsdoc::valid_name(&name).is_ok() {
                        name
                    } else {
                        String::new()
                    },
                    error: None,
                });
            }
            Command::ResetPanels => {
                crate::popover::forget_drag();
                crate::picker_view::update_settings(cx, |s| s.panel_places.clear());
            }
            Command::Open => {
                let ws = cx.global::<Workspace>();
                let mut items = ws
                    .config_dir
                    .as_deref()
                    .map(persist::list_named)
                    .unwrap_or_default();
                items.extend(["pair", "trading", "1x2", "2x2", "1+2", "chart"].map(String::from));
                self.prompt = Some(Prompt::Open {
                    items,
                    selected: 0,
                    error: None,
                });
            }
        }
        cx.notify();
    }

    /// Strategy controls. Stop all runs from any pane (through a strategy panel if one
    /// exists, so a refusal shows there; else straight to the core). Start and Stop act on
    /// the focused panel's selected strategy.
    fn strategy_command(&mut self, c: StrategyCmd, window: &mut Window, cx: &mut Context<Self>) {
        let focused = match self.tab().focused_handle() {
            Some(PaneHandle::Strategies(p)) => Some(p.clone()),
            _ => None,
        };
        let panel = focused.clone().or_else(|| {
            self.tabs.iter().find_map(|t| {
                t.panes.values().find_map(|s| match &s.handle {
                    PaneHandle::Strategies(p) => Some(p.clone()),
                    _ => None,
                })
            })
        });
        let r = match (c, panel, focused) {
            (StrategyCmd::Start | StrategyCmd::Stop, _, None) => {
                Err("focus the strategy panel first (Open strategies)")
            }
            (_, Some(p), _) => p.update(cx, |v, cx| v.run(c, lines_trading::Source::Hotkey, cx)),
            (_, None, _) => match crate::trading::Trading::try_global(cx) {
                Some(t) => {
                    crate::strategy_panel::submit_stop_all(
                        t,
                        c == StrategyCmd::StopAll,
                        lines_trading::Source::Hotkey,
                    );
                    Ok(())
                }
                None => Err("trading isn't running"),
            },
        };
        if let Err(e) = r {
            self.refuse(e, cx);
        }
        let _ = window;
    }

    /// Show the journal or the strategy panel: such a pane of this window (shown again if it
    /// was hidden; a popped-out one is said to be in its own window), else a new tab with one.
    fn open_panel(&mut self, kind: PaneKind, window: &mut Window, cx: &mut Context<Self>) {
        let found = self.tabs.iter().enumerate().find_map(|(i, t)| {
            t.panes
                .iter()
                .find(|(_, s)| s.handle.kind() == kind)
                .map(|(id, _)| (i, *id, t.state(*id)))
        });
        if let Some((i, id, state)) = found {
            if state == PaneState::Detached {
                self.refuse(
                    match kind {
                        PaneKind::Journal => "the journal is popped out: it is in its own window",
                        PaneKind::Chain => {
                            "the option chain is popped out: it is in its own window"
                        }
                        PaneKind::Positions => "positions are popped out: in their own window",
                        _ => "the strategy panel is popped out: it is in its own window",
                    },
                    cx,
                );
                return;
            }
            self.activate_tab(i, window, cx);
            if state.is_hidden() {
                self.apply_states(vec![(id, PaneState::Shown)], window, cx);
            }
            self.focus_pane(id, window, cx);
            return;
        }
        // An option chain opens on the focused pane's market, in its link group (the
        // journal and the strategy panel aren't per market).
        let (target, link) = match (
            kind,
            self.tabs.get(self.active).and_then(|t| t.focused_handle()),
        ) {
            (PaneKind::Chain, Some(h)) => (h.feed(cx).target_doc, h.link(cx)),
            // Positions in the focused pane's link group: a row's click shows it there.
            (PaneKind::Positions, Some(h)) => (None, h.link(cx)),
            _ => (None, None),
        };
        let tab = Self::build_tab(
            &TabDoc {
                focused: 0,
                layout: Layout::Pane(PaneDoc::new(kind, link, target)),
            },
            cx,
        );
        self.adopt(tab, cx);
        let i = self.tabs.len() - 1;
        self.activate_tab(i, window, cx);
    }

    /// A fresh trading tab on the focused pane's market, in its link group.
    fn fresh_tab(&self, cx: &mut App) -> Tab {
        let (target, link) = match self.tabs.get(self.active).and_then(|t| t.focused_handle()) {
            Some(h) => (h.feed(cx).target_doc, h.link(cx)),
            None => (None, Some(0)),
        };
        let layout = trading_group(link, &mut |k, l| PaneDoc::new(k, l, target.clone()));
        Self::build_tab(&TabDoc { focused: 0, layout }, cx)
    }

    /// Reopen the most recently closed tab where it was (layout, panes and states as saved).
    fn reopen_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.tabs.len();
        let Some((doc, i)) = cx.global_mut::<Workspace>().closed_tabs.pop(len) else {
            self.refuse("no closed tab to reopen", cx);
            return;
        };
        let tab = Self::build_tab(&doc, cx);
        let i = i.min(self.tabs.len());
        self.adopt_at(i, tab, cx);
        self.activate_tab(i, window, cx);
    }

    /// Put pane `id` of the active tab into its own window, at screen position `at` (default:
    /// where the pane was), keeping its leaf here for re-attaching.
    fn pop_out(
        &mut self,
        id: PaneId,
        at: Option<(f32, f32)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = self.tab();
        if let Err(e) = tabstate::can_detach(&tab.order(), &tab.leaves(cx), id) {
            self.refuse(e, cx);
            return;
        }
        let Some(h) = tab.handle(id).cloned() else {
            return;
        };
        let area = self.content_area(window, cx);
        let index = tab.layout.index_of(&id);
        let r = tab
            .layout
            .place_where(area, &|p| tab.is_shown(p), &|p| tab.min_h(p), &|p| {
                tab.min_w(p)
            })
            .into_iter()
            .find(|p| Some(p.index) == index)
            .map_or(Rect::new(0.0, 0.0, 360.0, 600.0), |p| p.rect);
        let (x, y) = at.unwrap_or((
            self.screen.x + r.x + 24.0,
            self.screen.y + self.bar_h() + r.y + 24.0,
        ));
        let place = Placement {
            display: None,
            state: WindowState::Windowed,
            x,
            y,
            w: r.w.max(240.0),
            h: r.h.max(200.0),
        };
        self.apply_states(vec![(id, PaneState::Detached)], window, cx);
        cx.defer(move |cx| {
            if let Err(e) = pane_window::open(id, h, Some(&place), cx) {
                cx.global_mut::<Workspace>()
                    .add_report(format!("couldn't open a pane window: {e:#}"));
                reattach_pane(id, cx);
            }
        });
    }

    /// Whether pane `id` lives in one of this window's tabs.
    pub(crate) fn has_pane(&self, id: PaneId) -> bool {
        self.tabs.iter().any(|t| t.panes.contains_key(&id))
    }

    /// A detached pane comes back to its leaf (its previous spot); its window closes.
    pub(crate) fn reattach(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(i) = self.tabs.iter().position(|t| t.panes.contains_key(&id)) else {
            return;
        };
        self.tabs[i].set_state(id, PaneState::Shown);
        if i == self.active {
            self.sync_visibility(cx);
            self.focus_pane(id, window, cx);
        } else {
            self.sync_visibility(cx);
        }
        cx.defer(move |cx| pane_window::close(id, cx));
        cx.notify();
    }

    fn focused_target(&self, cx: &App) -> Option<wsdoc::TargetDoc> {
        self.tab()
            .focused_handle()
            .and_then(|h| h.feed(cx).target_doc)
    }

    fn move_focus(&mut self, dir: Direction, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.tab();
        let area = self.content_area(window, cx);
        let Some(i) = tab.layout.index_of(&tab.focused) else {
            return;
        };
        if let Some(j) = tab.layout.neighbour_where(
            i,
            dir,
            area,
            &|id| tab.is_shown(id),
            &|id| tab.min_h(id),
            &|id| tab.min_w(id),
        ) {
            let id = *tab.layout.leaves()[j];
            self.focus_pane(id, window, cx);
        }
    }

    fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        let Some(h) = self.tab().focused_handle().cloned() else {
            return;
        };
        // Same kind, symbol, link and view state as the source; the trading switch is not
        // copied (new panes start disabled).
        let mut doc = h.doc(cx);
        doc.trade = doc.trade.take().map(crate::trade_bar::TradeDoc::copied);
        let new_id = PaneId::next();
        let handle = make_pane(&doc, cx);
        let subs = self.subscribe(new_id, &handle, cx);
        let focused = self.tab().focused;
        let tab = self.tab_mut();
        let layout = std::mem::replace(&mut tab.layout, Layout::Pane(focused));
        tab.layout = match layout.split_leaf(&focused, axis, new_id) {
            Ok(l) => l,
            Err((l, _)) => l,
        };
        tab.panes.insert(
            new_id,
            PaneSlot {
                handle,
                _subs: subs,
            },
        );
        self.sync_visibility(cx);
        self.focus_pane(new_id, window, cx);
    }

    fn close_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        let focused = tab.focused;
        let Some(i) = tab.layout.index_of(&focused) else {
            return;
        };
        let layout = std::mem::replace(&mut tab.layout, Layout::Pane(focused));
        match layout.remove_leaf(&focused) {
            Ok(Some(rest)) => {
                tab.layout = rest;
                tab.panes.remove(&focused);
                tab.state.remove(&focused);
                // Never leave a tab with nothing on screen: bring back what was hidden, else
                // what was popped out.
                if tab.in_state(PaneState::is_shown).is_empty() {
                    let hidden = tab.in_state(PaneState::is_hidden);
                    let back = if hidden.is_empty() {
                        tab.in_state(|s| s == PaneState::Detached)
                    } else {
                        hidden
                    };
                    for id in back {
                        tab.set_state(id, PaneState::Shown);
                        cx.defer(move |cx| pane_window::close(id, cx));
                    }
                }
                let shown = tab.in_state(PaneState::is_shown);
                let order = tab.order();
                // The shown pane nearest where the closed one was.
                let next = order[i.min(order.len() - 1)..]
                    .iter()
                    .chain(order[..i.min(order.len())].iter().rev())
                    .find(|id| shown.contains(id))
                    .copied();
                self.sync_visibility(cx);
                if let Some(n) = next {
                    self.focus_pane(n, window, cx);
                }
            }
            Ok(None) => {
                tab.layout = Layout::Pane(focused);
                self.close_tab(self.active, window, cx);
            }
            Err(l) => tab.layout = l,
        }
    }

    /// Close tab `i`; Reopen closed tab brings it back (so no confirmation). The last tab of
    /// another window closes that window; the last tab of the last window is replaced with a
    /// fresh one, so closing always does something visible.
    fn close_tab(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if i >= self.tabs.len() {
            return;
        }
        let doc = self.tabs[i].doc(cx);
        let windows = cx.global::<Workspace>().windows.len();
        cx.global_mut::<Workspace>().closed_tabs.push(doc, i);
        // Detached panes of the closed tab lose their windows (kept in the closed tab's doc).
        cx.defer(prune_pane_windows);
        // Dropping a tab drops its panes; the registry stops feeds nobody else watches.
        match tabstate::close_tab(self.tabs.len(), windows, self.active, i) {
            tabstate::CloseTab::CloseWindow => window.remove_window(),
            tabstate::CloseTab::ReplaceWithFresh => {
                let fresh = self.fresh_tab(cx);
                self.tabs.clear();
                self.adopt(fresh, cx);
                self.activate_tab(0, window, cx);
            }
            tabstate::CloseTab::Remove { active } => {
                self.tabs.remove(i);
                self.activate_tab(active, window, cx);
            }
        }
    }

    fn set_kind(&mut self, kind: PaneKind, window: &mut Window, cx: &mut Context<Self>) {
        let Some(h) = self.tab().focused_handle().cloned() else {
            return;
        };
        if h.kind() == kind {
            return;
        }
        let old = h.doc(cx);
        let doc = PaneDoc::new(kind, old.link, old.target);
        let id = self.tab().focused;
        let handle = make_pane(&doc, cx);
        let subs = self.subscribe(id, &handle, cx);
        self.tab_mut().panes.insert(
            id,
            PaneSlot {
                handle,
                _subs: subs,
            },
        );
        self.sync_visibility(cx);
        self.focus_pane(id, window, cx);
    }

    fn set_link(&mut self, link: LinkGroup, my_window: gpui::WindowId, cx: &mut Context<Self>) {
        let Some(h) = self.tab().focused_handle().cloned() else {
            return;
        };
        let me = self.tab().focused;
        h.set_link(link, cx);
        // Joining a group adopts the group's symbol, so a group always shows one market.
        let Some(g) = link else { return };
        let mut others: Vec<PaneHandle> = self
            .tabs
            .iter()
            .flat_map(|t| t.panes.iter())
            .filter(|(id, _)| **id != me)
            .map(|(_, s)| s.handle.clone())
            .collect();
        for w in cx.global::<Workspace>().windows.clone() {
            // This window's view is being updated right now: its panes are `self.tabs`.
            if w.window_id() == my_window {
                continue;
            }
            if let Ok(wv) = w.read(cx) {
                others.extend(wv.tabs.iter().flat_map(|t| t.handles().cloned()));
            }
        }
        if let Some(f) = others
            .into_iter()
            .find(|o| o.link(cx) == Some(g))
            .map(|o| o.feed(cx))
        {
            h.set_feed(f, cx);
        }
    }

    /// Each link group's market: the target of some pane in it, anywhere in the workspace.
    fn group_targets(
        &self,
        my_window: gpui::WindowId,
        cx: &mut App,
    ) -> HashMap<u8, Option<wsdoc::TargetDoc>> {
        let mut handles: Vec<PaneHandle> = self
            .tabs
            .iter()
            .flat_map(|t| t.handles().cloned())
            .collect();
        for w in cx.global::<Workspace>().windows.clone() {
            if w.window_id() != my_window
                && let Ok(wv) = w.read(cx)
            {
                handles.extend(wv.tabs.iter().flat_map(|t| t.handles().cloned()));
            }
        }
        let mut out = HashMap::new();
        for h in handles {
            if let Some(g) = h.link(cx) {
                out.entry(g).or_insert_with(|| h.feed(cx).target_doc);
            }
        }
        out
    }

    fn apply_preset(&mut self, preset: Preset, window: &mut Window, cx: &mut Context<Self>) {
        // Panes join the market their link group already shows; otherwise the focused pane's.
        let focused = self.focused_target(cx);
        let groups = self.group_targets(window.window_handle().window_id(), cx);
        let layout = preset.build(|k, l| {
            let t = l
                .and_then(|g| groups.get(&g).cloned())
                .unwrap_or_else(|| focused.clone());
            PaneDoc::new(k, l, t)
        });
        // New panes first (they share the running feeds), then the old ones go.
        let mut tab = Self::build_tab(&TabDoc { focused: 0, layout }, cx);
        for (id, slot) in tab.panes.iter_mut() {
            slot._subs = self.subscribe(*id, &slot.handle, cx);
        }
        let i = self.active;
        self.tabs[i] = tab;
        self.activate_tab(i, window, cx);
        // The old tab's popped-out panes went with it.
        cx.defer(prune_pane_windows);
    }

    /// Take pane `id` out of its tab (for docking elsewhere); `None` if it is its tab's only
    /// pane.
    fn take_pane(&mut self, id: PaneId) -> Option<PaneSlot> {
        let tab = self.tabs.iter_mut().find(|t| t.panes.contains_key(&id))?;
        if tab.layout.leaf_count() < 2 {
            return None;
        }
        let layout = std::mem::replace(&mut tab.layout, Layout::Pane(id));
        tab.layout = match layout.remove_leaf(&id) {
            Ok(Some(rest)) => rest,
            Ok(None) => unreachable!("two leaves or more"),
            Err(l) => l,
        };
        tab.state.remove(&id);
        tab.panes.remove(&id)
    }

    /// Put `slot` (pane `id`) beside chart `chart`, which keeps [`DOCK_SHARE`] of the width.
    fn insert_beside(
        &mut self,
        chart: PaneId,
        id: PaneId,
        mut slot: PaneSlot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(i) = self.tabs.iter().position(|t| t.panes.contains_key(&chart)) else {
            return;
        };
        slot._subs = self.subscribe(id, &slot.handle, cx);
        let tab = &mut self.tabs[i];
        let layout = std::mem::replace(&mut tab.layout, Layout::Pane(chart));
        tab.layout = match layout.split_leaf_at(&chart, Axis::Row, id, DOCK_SHARE) {
            Ok(l) => l,
            Err((l, _)) => l,
        };
        tab.panes.insert(id, slot);
        self.activate_tab(i, window, cx);
        self.focus_pane(id, window, cx);
    }

    // ---- tab moves --------------------------------------------------------------------------

    /// Move tab `i` into a new window at screen position `at`.
    fn tear_off(&mut self, i: usize, at: (f32, f32), window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        let tab = self.tabs.remove(i);
        if self.active >= self.tabs.len() || self.active > i {
            self.active = self.active.saturating_sub(1).min(self.tabs.len() - 1);
        }
        let a = self.active;
        self.activate_tab(a, window, cx);
        let size = (
            self.screen.w.max(640.0) * 0.8,
            self.screen.h.max(480.0) * 0.8,
        );
        let bounds = Bounds {
            origin: point(px(at.0 - tab_w() * 0.5), px(at.1 - 12.0)),
            size: gpui::size(px(size.0), px(size.1)),
        };
        cx.defer(move |cx| {
            if let Err(e) =
                open_window(vec![tab], 0, Some(WindowBounds::Windowed(bounds)), None, cx)
            {
                cx.global_mut::<Workspace>()
                    .add_report(format!("couldn't open a window: {e:#}"));
            }
        });
    }

    /// Move tab `i` into another window.
    fn move_tab_to(
        &mut self,
        i: usize,
        to: WindowHandle<WindowView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = self.tabs.remove(i);
        let emptied = self.tabs.is_empty();
        if !emptied {
            if self.active >= self.tabs.len() || self.active > i {
                self.active = self.active.saturating_sub(1).min(self.tabs.len() - 1);
            }
            let a = self.active;
            self.activate_tab(a, window, cx);
        }
        cx.defer(move |cx| {
            let _ = to.update(cx, |wv, window, cx| {
                wv.adopt(tab, cx);
                let i = wv.tabs.len() - 1;
                wv.activate_tab(i, window, cx);
                window.activate_window();
            });
        });
        if emptied {
            window.remove_window();
        }
    }

    fn end_tab_drag(&mut self, pos: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.tab_drag.take() else {
            return;
        };
        let d = pos - drag.start;
        if !drag.active && f32::from(d.x).abs() + f32::from(d.y).abs() <= DRAG_START {
            return;
        }
        let own = screen_rect(window);
        let vs = window.viewport_size();
        let own = Rect::new(own.x, own.y, f32::from(vs.width), f32::from(vs.height));
        let bar = Rect::new(own.x, own.y, own.w, tab_bar_h());
        let rights: Vec<f32> = (0..self.tabs.len())
            .map(|i| own.x + tab_x0() + (i as f32 + 1.0) * tab_w())
            .collect();
        let me = window.window_handle().window_id();
        let others: Vec<(WindowHandle<WindowView>, Rect)> = cx
            .global::<Workspace>()
            .windows
            .iter()
            .filter(|w| w.window_id() != me)
            .filter_map(|w| w.read(cx).ok().map(|wv| (*w, wv.screen)))
            .collect();
        let pointer = (own.x + f32::from(pos.x), own.y + f32::from(pos.y));
        match wsdoc::tab_drop(pointer, own, bar, &rights, &others) {
            TabDrop::Reorder(j) => {
                let j = j.min(self.tabs.len() - 1);
                if j != drag.index {
                    let tab = self.tabs.remove(drag.index);
                    self.tabs.insert(j, tab);
                    self.activate_tab(j, window, cx);
                }
            }
            TabDrop::ToWindow(w) => self.move_tab_to(drag.index, w, window, cx),
            TabDrop::NewWindow { x, y } => self.tear_off(drag.index, (x, y), window, cx),
            TabDrop::Cancel => {}
        }
        cx.notify();
    }

    // ---- menu and prompt --------------------------------------------------------------------

    /// Open a pane, tab or column menu: any other popover closes (`crate::popover`).
    fn show_menu(&mut self, menu: Menu, cx: &mut Context<Self>) {
        self.menu = Some(menu);
        crate::popover::opened(cx, crate::popover::Kind::Menu, |this: &mut Self, cx| {
            if this.menu.take().is_some() {
                cx.notify();
            }
        });
        cx.notify();
    }

    /// The focused pane's menu (`crate::pane_menu`); after a right click on an order's tag or a
    /// drawing, that object's menu instead. Layout and workspace actions are on the tab's.
    fn open_menu(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(h) = self.tab().focused_handle().cloned() else {
            return;
        };
        if let PaneHandle::Chart(chart) = &h
            && let Some(o) = chart.update(cx, |c, _| c.take_menu_object())
        {
            self.show_menu(
                Menu::new(at, crate::pane_menu::object_items(&o)).unselected(),
                cx,
            );
            return;
        }
        let m = self.pane_menu(&h, cx);
        self.show_menu(Menu::new(at, crate::pane_menu::items(&m)), cx);
    }

    /// The focused pane's state, as its menu shows it.
    fn pane_menu(&self, h: &PaneHandle, cx: &mut Context<Self>) -> crate::pane_menu::PaneMenu {
        use crate::pane_menu::{ChartMenu, DomMenu, JournalMenu, PaneMenu};
        let kind = h.kind();
        let tab = self.tab();
        let leaves = tab.leaves(cx);
        let link = h.link(cx);
        let (hide_hints, closed_tabs) = {
            let ws = cx.global::<Workspace>();
            (ws.hide_hints, !ws.closed_tabs.is_empty())
        };
        let chart = match h {
            PaneHandle::Chart(chart) => {
                let c = chart.read(cx);
                let v = c.visibility();
                let lc = c.levels_config();
                Some(ChartMenu {
                    timeframe: c.timeframe(),
                    on_ticks: c.on_ticks(),
                    tick_continuous: c.tick_continuous(),
                    levels: (!c.level_sources().is_empty())
                        .then_some((!lc.hidden, !lc.hide_labels)),
                    studies_shown: !v.hide_studies,
                    study_labels_shown: !v.hide_study_labels,
                    strategy_marks_shown: !c.strategy_marks_hidden(),
                    clean: v.hide_study_labels && hide_hints,
                    order_strip: c.order_strip(),
                    magnet: c.magnet(),
                    drawings_shown: !c.drawings_hidden(),
                    drawing_selected: c.drawing_selected(),
                    symbol: c.product_symbol().to_string(),
                    chart_only: tabstate::is_chart_only(&leaves, tab.focused),
                })
            }
            _ => None,
        };
        let dom = match h {
            PaneHandle::Dom(d) => {
                let tr = d.read(cx).trail();
                Some(DomMenu {
                    trail: tr.enabled(),
                    half_life_ms: tr.half_life_ms(),
                })
            }
            _ => None,
        };
        let journal = match h {
            PaneHandle::Journal(j) => Some(JournalMenu {
                group: j.read(cx).group(),
                all_dates: j.read(cx).all_dates(),
            }),
            _ => None,
        };
        PaneMenu {
            kind,
            link,
            chart,
            dom,
            journal,
            pnl: pnl_mode(cx),
            linked_dom: tabstate::kind_shown(&leaves, tab.focused, PaneKind::Dom),
            linked_tape: tabstate::kind_shown(&leaves, tab.focused, PaneKind::Tape),
            hidden_panes: !tab.in_state(PaneState::is_hidden).is_empty(),
            detached_panes: !tab.in_state(|s| s == PaneState::Detached).is_empty(),
            closed_tabs,
        }
    }

    /// A DOM's or tape's column menu (right-click on its header row): show or hide each
    /// column (the price column always stays), fit them, reset them.
    fn open_column_menu(&mut self, id: PaneId, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(cols) = self
            .tabs
            .iter()
            .find_map(|t| t.handle(id))
            .and_then(|h| h.columns(cx))
        else {
            return;
        };
        let mut items = Vec::new();
        for i in 0..cols.set.len() {
            let Some(col) = cols.column(i) else { continue };
            let label = if cols.specs()[i].hideable {
                cols.label(i).to_owned()
            } else {
                format!("{} (always shown)", cols.label(i))
            };
            items.push(MenuItem::named(
                &label,
                Command::Columns(ColumnCmd::Toggle(col)),
                cols.set.is_shown(i),
            ));
        }
        items.push(MenuItem::sep());
        items.push(MenuItem::of(Command::Columns(ColumnCmd::AutoFitAll), false));
        items.push(MenuItem::of(Command::Columns(ColumnCmd::Reset), false));
        self.show_menu(Menu::new(at, items), cx);
    }

    /// A tab's menu (`pane_menu::tab_items`): tab, layout and workspace actions.
    fn open_tab_menu(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let ws = cx.global::<Workspace>();
        let m = crate::pane_menu::TabMenu {
            tabs: self.tabs.len(),
            closed_tabs: !ws.closed_tabs.is_empty(),
            crosshair_sync: ws.crosshair_sync,
            crosshair_sync_price: ws.crosshair_sync_price,
            key_hints: !ws.hide_hints,
            perf_stats: ws.show_perf_stats,
            latency_hud: ws.show_latency_hud,
        };
        self.show_menu(Menu::new(at, crate::pane_menu::tab_items(&m)), cx);
    }

    fn menu_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        match menu.key(ev.keystroke.key.as_str()) {
            MenuKey::Close => self.menu = None,
            MenuKey::Run(cmd) => {
                self.menu = None;
                self.run(cmd, window, cx);
            }
            MenuKey::Moved | MenuKey::Ignored => {}
        }
        cx.notify();
    }

    /// Enter in the open prompt (or a click on a row): save, open the selected workspace, run
    /// the selected command.
    fn prompt_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.prompt.as_mut() {
            Some(Prompt::SaveAs { text, error }) => match wsdoc::valid_name(text) {
                Ok(name) => {
                    save_named(&name, cx);
                    self.prompt = None;
                }
                Err(e) => *error = Some(e),
            },
            Some(Prompt::Open {
                items,
                selected,
                error,
            }) => {
                if let Some(name) = items.get(*selected).cloned() {
                    match load_named(&name, cx) {
                        Ok(doc) => {
                            self.prompt = None;
                            let me = window.window_handle();
                            cx.defer(move |cx| switch_workspace(doc, me, cx));
                        }
                        Err(e) => *error = Some(e),
                    }
                }
            }
            Some(Prompt::Palette {
                shown, selected, ..
            }) => {
                if let Some(cmd) = shown.get(*selected).copied() {
                    self.prompt = None;
                    self.run(cmd, window, cx);
                }
            }
            None => {}
        }
    }

    /// A click on row `i` of the open list: as Enter on it.
    fn prompt_pick(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        crate::popover::mark_moved_under_pointer();
        match self.prompt.as_mut() {
            Some(Prompt::Open { selected, .. } | Prompt::Palette { selected, .. }) => *selected = i,
            _ => return,
        }
        self.prompt_enter(window, cx);
        cx.notify();
    }

    /// The wheel over the open list: the selection moves `rows` (the list follows it).
    fn prompt_wheel(&mut self, rows: isize) {
        let (selected, n) = match self.prompt.as_mut() {
            Some(Prompt::Open {
                items, selected, ..
            }) => (selected, items.len()),
            Some(Prompt::Palette {
                shown, selected, ..
            }) => (selected, shown.len()),
            _ => return,
        };
        *selected = selected
            .saturating_add_signed(rows)
            .min(n.saturating_sub(1));
    }

    fn prompt_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let key = k.key.as_str();
        if key == "escape" {
            self.prompt = None;
            cx.notify();
            return;
        }
        if key == "enter" {
            self.prompt_enter(window, cx);
            cx.notify();
            return;
        }
        match self.prompt.as_mut() {
            Some(Prompt::SaveAs { text, error }) => match key {
                "backspace" => {
                    text.pop();
                    *error = None;
                }
                _ => {
                    if let Some(c) = k.key_char.as_deref().filter(|_| {
                        !k.modifiers.control && !k.modifiers.alt && !k.modifiers.platform
                    }) && text.chars().count() < 40
                    {
                        text.push_str(c);
                        *error = None;
                    }
                }
            },
            Some(Prompt::Open {
                items, selected, ..
            }) => match key {
                "down" => *selected = (*selected + 1) % items.len().max(1),
                "up" => *selected = (*selected + items.len().max(1) - 1) % items.len().max(1),
                _ => {}
            },
            Some(Prompt::Palette {
                query,
                items,
                shown,
                selected,
            }) => {
                let n = shown.len().max(1);
                match key {
                    "down" => *selected = (*selected + 1) % n,
                    "up" => *selected = (*selected + n - 1) % n,
                    "backspace" => {
                        query.pop();
                        *shown = commands::filter(query, items);
                        *selected = 0;
                    }
                    _ => {
                        if let Some(c) = k.key_char.as_deref().filter(|_| {
                            !k.modifiers.control && !k.modifiers.alt && !k.modifiers.platform
                        }) {
                            query.push_str(c);
                            *shown = commands::filter(query, items);
                            *selected = 0;
                        }
                    }
                }
            }
            None => {}
        }
        cx.notify();
    }

    /// A trading command on the focused pane's account and contract (decision 0016). `key`:
    /// from its key, run in the key-down handler (order keys need the DOM's order hotkeys).
    /// A size being typed needs its keys: focus may sit on this window's root.
    fn focus_for_size(
        &mut self,
        tc: crate::commands::TradeCmd,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if tc == crate::commands::TradeCmd::TypeQty && !self.tabs.is_empty() {
            let id = self.tab().focused;
            self.focus_pane(id, window, cx);
        }
    }

    fn run_trade(&mut self, tc: crate::commands::TradeCmd, key: bool, cx: &mut Context<Self>) {
        let issued = std::time::Instant::now();
        let run = if key {
            crate::trade_bar::run_key::<crate::dom_view::DomView>
        } else {
            crate::trade_bar::run::<crate::dom_view::DomView>
        };
        let run_chart = if key {
            crate::trade_bar::run_key::<crate::chart_view::ChartView>
        } else {
            crate::trade_bar::run::<crate::chart_view::ChartView>
        };
        match self.tab().focused_handle().cloned() {
            Some(PaneHandle::Dom(d)) => {
                d.update(cx, |v, cx| run(v, tc, issued, cx));
            }
            Some(PaneHandle::Chart(c)) if crate::trade_bar::chart_runs(tc) => {
                c.update(cx, |v, cx| run_chart(v, tc, issued, cx));
            }
            Some(PaneHandle::Chart(_)) => {
                self.refuse("arming and orders act on a DOM: focus one first", cx);
            }
            // The chain's exits, as on a DOM: for the underlying's options (their labels say so).
            Some(PaneHandle::Chain(c))
                if matches!(
                    tc,
                    crate::commands::TradeCmd::Flatten | crate::commands::TradeCmd::CancelAll
                ) =>
            {
                let source = if key {
                    lines_trading::Source::Hotkey
                } else {
                    lines_trading::Source::Palette
                };
                let flatten = tc == crate::commands::TradeCmd::Flatten;
                c.update(cx, |v, cx| v.run_exit(flatten, source, cx));
            }
            Some(PaneHandle::Chain(c)) if tc == crate::commands::TradeCmd::NextAccount => {
                c.update(cx, |v, cx| v.next_account(cx));
            }
            _ => self.refuse("trading commands act on the focused DOM or chart", cx),
        }
    }

    // ---- input ------------------------------------------------------------------------------

    fn on_key_capture(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_some() {
            self.menu_key(ev, window, cx);
            cx.stop_propagation();
        } else if self.prompt.is_some() {
            self.prompt_key(ev, window, cx);
            cx.stop_propagation();
        } else if let Some(cmd) = commands::command_for(&ev.keystroke) {
            // A pane taking typed text (symbol search, a note) keeps its plain and Alt keys.
            let typing = self
                .tabs
                .get(self.active)
                .and_then(|t| t.focused_handle())
                .is_some_and(|h| h.takes_text(cx));
            if typing && !commands::runs_while_typing(&ev.keystroke, cmd) {
                return;
            }
            // A held key repeats only commands that may repeat: never an order or arming.
            if !ev.is_held || commands::repeats(cmd) {
                match cmd {
                    Command::Trade(tc) => {
                        self.focus_for_size(tc, window, cx);
                        self.run_trade(tc, true, cx)
                    }
                    _ => self.run(cmd, window, cx),
                }
            }
            cx.stop_propagation();
        }
    }

    /// Which pane and tab are under the pointer (window coordinates); redraw on change.
    fn update_hover(&mut self, pos: Point<Pixels>, window: &Window, cx: &mut Context<Self>) {
        let (x, y) = (f32::from(pos.x), f32::from(pos.y));
        let bar = self.bar_h();
        let area = self.content_area(window, cx);
        let tab = (y < bar && x >= tab_x0())
            .then(|| ((x - tab_x0()) / tab_w()) as usize)
            .filter(|i| *i < self.tabs.len());
        let pane = self.tabs.get(self.active).and_then(|t| {
            let order = t.order();
            t.layout
                .place_where(area, &|id| t.is_shown(id), &|id| t.min_h(id), &|id| {
                    t.min_w(id)
                })
                .into_iter()
                .find(|p| p.rect.contains(x, y - bar))
                .map(|p| order[p.index])
        });
        if (tab, pane) != (self.hover_tab, self.hover_pane) {
            self.hover_tab = tab;
            self.hover_pane = pane;
            cx.notify();
        }
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.update_hover(ev.position, window, cx);
        if let Some(drag) = self.pane_drag.as_mut() {
            if ev.pressed_button != Some(MouseButton::Left) {
                self.pane_drag = None;
                cx.notify();
                return;
            }
            drag.now = ev.position;
            let d = drag.now - drag.start;
            if !drag.active && (f32::from(d.x).abs() + f32::from(d.y).abs()) > DRAG_START {
                drag.active = true;
            }
            if drag.active {
                cx.notify();
            }
            return;
        }
        if let Some(d) = &self.split_drag {
            if ev.pressed_button != Some(MouseButton::Left) {
                self.split_drag = None;
                return;
            }
            let r = d.ratio_at(
                f32::from(ev.position.x),
                f32::from(ev.position.y) - self.bar_h(),
            );
            let path = d.path.clone();
            self.tab_mut().layout.set_ratio(&path, r);
            cx.notify();
            return;
        }
        if let Some(drag) = self.tab_drag.as_mut() {
            if ev.pressed_button != Some(MouseButton::Left) {
                // The release happened where we didn't see it: drop nothing.
                self.tab_drag = None;
                cx.notify();
                return;
            }
            drag.now = ev.position;
            let d = drag.now - drag.start;
            if !drag.active && (f32::from(d.x).abs() + f32::from(d.y).abs()) > DRAG_START {
                drag.active = true;
            }
            if drag.active {
                cx.notify();
            }
        }
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.split_drag = None;
        self.end_tab_drag(ev.position, window, cx);
        self.end_pane_drag(ev.position, window, cx);
    }

    /// A pane dragged by its header and released outside the window pops out right there.
    fn end_pane_drag(&mut self, pos: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.pane_drag.take() else {
            return;
        };
        cx.notify();
        let vs = window.viewport_size();
        let (x, y) = (f32::from(pos.x), f32::from(pos.y));
        let inside = x >= 0.0 && y >= 0.0 && x < f32::from(vs.width) && y < f32::from(vs.height);
        if !drag.active || inside {
            return;
        }
        let at = (self.screen.x + x - 40.0, self.screen.y + y - 12.0);
        self.pop_out(drag.id, Some(at), window, cx);
    }

    // ---- saved form -------------------------------------------------------------------------

    fn doc(&self, window: &Window, cx: &mut App) -> WindowDoc {
        WindowDoc {
            placement: placement(window, cx),
            active_tab: self.active,
            tabs: self.tabs.iter().map(|t| t.doc(cx)).collect(),
        }
    }

    // ---- rendering --------------------------------------------------------------------------

    fn tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let mut bar = div()
            .h(px(tab_bar_h()))
            .flex_none()
            .flex()
            .flex_row()
            .items_end()
            .pl(px(tab_x0()))
            .bg(t.panel)
            .border_b_1()
            .border_color(t.axis_line)
            .text_size(crate::zoom::ui(12.0));
        let hovered = self.hover_tab;
        for (i, tab) in self.tabs.iter().enumerate() {
            let active = i == self.active;
            let title = tab.title(cx);
            // Close: on the active tab, and on any tab while hovered.
            let close = (active || hovered == Some(i)).then(|| {
                // Named per tab for assistive technology; the key closes the active tab only.
                crate::icons::button(
                    ("tab-close", i),
                    crate::icons::Icon::Close,
                    &format!("Close {title}"),
                    active.then_some(Command::CloseTab),
                    false,
                    &t,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.close_tab(i, window, cx);
                    }),
                )
            });
            bar = bar.child(
                div()
                    .id(("tab", i))
                    .w(px(tab_w()))
                    .h_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .pl_3()
                    .pr_1()
                    .overflow_hidden()
                    .border_b_2()
                    .border_color(if active {
                        t.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(if active { t.text_main } else { t.text_dim })
                    .when(active, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .child(div().flex_1().min_w_0().truncate().child(title))
                    .children(close)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.activate_tab(i, window, cx);
                            this.tab_drag = Some(TabDrag {
                                index: i,
                                start: ev.position,
                                now: ev.position,
                                active: false,
                            });
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.close_tab(i, window, cx);
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.activate_tab(i, window, cx);
                            this.open_tab_menu(ev.position, cx);
                        }),
                    ),
            );
        }
        bar = bar.child(
            div()
                .id("new-tab")
                .h_full()
                .px_2()
                .flex()
                .items_center()
                .text_color(t.text_dim)
                .hover(|s| s.text_color(t.text_main))
                .child("+")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.run(Command::NewTab, window, cx);
                    }),
                ),
        );
        bar
    }

    /// The workspace's status for the footer (always there, with or without a tab bar):
    /// an inline report, what this tab isn't showing (click to bring it back), crosshair
    /// sync, and the workspace's name (click to open another).
    fn status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let ws = cx.global::<Workspace>();
        let (name, sync, report) = (ws.name.clone(), ws.crosshair_sync, ws.notices.text());
        let trigger_status = ws.trigger_status.clone();
        let open = ws.open_positions.and_then(crate::positions::open_label);
        let is_first = ws.windows.first().map(|w| w.window_id());
        // What this tab isn't showing, as text that brings it back.
        let (hidden, detached) = match self.tabs.get(self.active) {
            Some(tab) => {
                let kinds = |ids: Vec<PaneId>| -> Vec<&'static str> {
                    let mut v: Vec<&'static str> = Vec::new();
                    for id in ids {
                        if let Some(h) = tab.handle(id) {
                            let l = match h.kind() {
                                PaneKind::Chart => "chart",
                                PaneKind::Dom => "DOM",
                                PaneKind::Tape => "T&S",
                                PaneKind::Journal => "journal",
                                PaneKind::Strategies => "strategy panel",
                                PaneKind::Chain => "option chain",
                                PaneKind::Positions => "positions",
                            };
                            if !v.contains(&l) {
                                v.push(l);
                            }
                        }
                    }
                    v
                };
                (
                    kinds(tab.in_state(PaneState::is_hidden)),
                    tab.in_state(|s| s == PaneState::Detached).len(),
                )
            }
            None => (Vec::new(), 0),
        };
        let link = |id: &'static str, text: String, cmd: Command| {
            div()
                .id(id)
                .text_color(t.text_dim)
                .hover(|s| s.text_color(t.text_main))
                .child(text)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.run(cmd, window, cx);
                    }),
                )
        };
        div()
            .flex_none()
            .max_w(relative(0.7))
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .gap_3()
            .whitespace_nowrap()
            .overflow_hidden()
            .text_color(t.text_dim)
            .when_some(report.filter(|_| is_first.is_some()), |d, r| {
                d.child(
                    div()
                        .id("report")
                        .min_w_0()
                        .truncate()
                        .text_color(t.text_main)
                        .child(format!("{r}  (click to dismiss)"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| {
                                cx.global_mut::<Workspace>().notices.clear();
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        ),
                )
            })
            .when_some(open, |d, text| {
                d.child(link("positions-open", text, Command::OpenPositions))
            })
            .when_some(trigger_status, |d, (text, loud)| {
                d.child(
                    div()
                        .id("trigger-status")
                        // Gives way in a narrow window before the links after it; the loud part
                        // is first.
                        .min_w_0()
                        .truncate()
                        .px_1()
                        .when(loud, |d| d.bg(t.warn).text_color(t.on_warn))
                        .when(!loud, |d| d.text_color(t.text_main))
                        .child(text),
                )
            })
            .when(!hidden.is_empty(), |d| {
                d.child(link(
                    "show-hidden",
                    format!("{} hidden · show", hidden.join(", ")),
                    Command::ShowHidden,
                ))
            })
            .when(detached > 0, |d| {
                d.child(link(
                    "reattach",
                    format!("{detached} popped out · re-attach"),
                    Command::Reattach,
                ))
            })
            .when(sync, |d| d.child("crosshair sync"))
            .child(
                div()
                    .id("workspace-name")
                    .text_color(t.text_main)
                    .hover(|s| s.text_color(t.accent))
                    .child(name)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.run(Command::Open, window, cx);
                        }),
                    ),
            )
    }

    /// Screenshots: open a prompt (`--prompt palette[:QUERY]|open|save`).
    #[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
    pub(crate) fn screenshot_prompt(
        &mut self,
        which: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let (kind, query) = which.split_once(':').unwrap_or((which, ""));
        let cmd = match kind {
            "palette" => Command::Palette,
            "open" => Command::Open,
            "save" => Command::SaveAs,
            _ => anyhow::bail!("--prompt: expected palette[:QUERY], open or save"),
        };
        self.run(cmd, window, cx);
        if let Some(Prompt::Palette {
            query: q,
            items,
            shown,
            selected,
        }) = self.prompt.as_mut()
        {
            *q = query.to_owned();
            *shown = commands::filter(q, items);
            *selected = 0;
        }
        cx.notify();
        Ok(())
    }

    /// Screenshot: open this menu once its pane can show it (an order's: once the chart has
    /// one).
    #[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
    pub(crate) fn screenshot_menu(
        &mut self,
        m: crate::pane_menu::ShotMenu,
        cx: &mut Context<Self>,
    ) {
        self.shot_menu = Some(m);
        cx.notify();
    }

    /// The screenshot's menu, if its pane can show it now: whether it's done.
    fn open_shot_menu(&mut self, m: &crate::pane_menu::ShotMenu, cx: &mut Context<Self>) -> bool {
        let at = point(px(m.at.0), px(m.at.1));
        if m.order {
            let Some(PaneHandle::Chart(chart)) = self.tab().focused_handle().cloned() else {
                shot_menu_failed("`order` needs a focused chart");
            };
            let mode = pnl_mode(cx);
            let Some(o) = chart.read(cx).first_order_object(mode) else {
                return false;
            };
            self.show_menu(
                Menu::new(at, crate::pane_menu::object_items(&o)).unselected(),
                cx,
            );
        } else if m.measure {
            let Some(PaneHandle::Chart(chart)) = self.tab().focused_handle().cloned() else {
                shot_menu_failed("`measure` needs a focused chart");
            };
            if !chart.read(cx).measure_shown() {
                return false;
            }
            let show = crate::picker_view::settings(cx).measure;
            let o = crate::pane_menu::MenuObject::Measure { show };
            self.show_menu(
                Menu::new(at, crate::pane_menu::object_items(&o)).unselected(),
                cx,
            );
        } else if m.tab {
            self.open_tab_menu(at, cx);
        } else {
            self.open_menu(at, cx);
        }
        if let (Some(sub), Some(menu)) = (&m.sub, self.menu.as_mut()) {
            let i = menu.items.iter().position(|i| i.label == *sub);
            if !i.is_some_and(|i| menu.open_sub(i, true)) {
                shot_menu_failed(&format!("no row \"{sub}\" opens a submenu"));
            }
        }
        true
    }

    /// Screenshots (`--hover-header`): show the focused pane's header controls as if the
    /// pointer were over it.
    #[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
    pub(crate) fn hover_focused_pane(&mut self, cx: &mut Context<Self>) {
        self.hover_pane = Some(self.tab().focused);
        self.hover_pinned = true;
        cx.notify();
    }

    fn panes(&self, area: Rect, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = self.theme;
        let tab = self.tab();
        let shown = |id: &PaneId| tab.is_shown(id);
        let mut out = Vec::new();
        let leaves = tab.layout.leaves();
        let min_h = |id: &PaneId| tab.min_h(id);
        let min_w = |id: &PaneId| tab.min_w(id);
        let placed = tab.layout.place_where(area, &shown, &min_h, &min_w);
        let hovered = self.hover_pane;
        let bar = self.bar_h();
        for p in &placed {
            let id = *leaves[p.index];
            let Some(h) = tab.handle(id) else { continue };
            let r = p.rect;
            let kind = h.kind();
            let controls = (hovered == Some(id)).then(|| {
                // From `pane::controls` (the gutter headers keep free is sized for them).
                let chart_only =
                    kind == PaneKind::Chart && tabstate::is_chart_only(&tab.leaves(cx), id);
                header_controls(
                    &t,
                    kind,
                    false,
                    chart_only,
                    |this: &mut Self, cmd, id, at, window, cx| {
                        this.focus_pane(id, window, cx);
                        match (cmd, at) {
                            (Command::Menu, Some(at)) => this.open_menu(at, cx),
                            _ => this.run(cmd, window, cx),
                        }
                    },
                    id,
                    cx,
                )
            });
            out.push(
                div()
                    .absolute()
                    .left(px(r.x))
                    .top(px(r.y))
                    .w(px(r.w))
                    .h(px(r.h))
                    .overflow_hidden()
                    .capture_any_mouse_down(cx.listener(
                        move |this, ev: &MouseDownEvent, window, cx| {
                            // An open menu's backdrop (`crate::popover`) takes presses while
                            // it is open: this pane sees none of them.
                            this.focus_pane(id, window, cx);
                            // The header is a handle: drag it out of the window to pop out.
                            let y = f32::from(ev.position.y) - bar - r.y;
                            if ev.button == MouseButton::Left && (0.0..HEADER_H).contains(&y) {
                                this.pane_drag = Some(PaneDrag {
                                    id,
                                    start: ev.position,
                                    now: ev.position,
                                    active: false,
                                });
                            }
                        },
                    ))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            this.focus_pane(id, window, cx);
                            let at = point(ev.position.x, ev.position.y);
                            this.open_menu(at, cx);
                        }),
                    )
                    .child(h.view().cached(StyleRefinement::default().size_full()))
                    .children(controls)
                    .into_any_element(),
            );
        }
        // Separators and their grab handles (on top of the panes).
        for (i, d) in tab
            .layout
            .dividers_where(area, &shown, &min_h, &min_w)
            .into_iter()
            .enumerate()
        {
            let line = match d.axis {
                Axis::Row => Rect::new(d.at, d.area.y, 1.0, d.area.h),
                Axis::Column => Rect::new(d.area.x, d.at, d.area.w, 1.0),
            };
            out.push(
                div()
                    .absolute()
                    .left(px(line.x))
                    .top(px(line.y))
                    .w(px(line.w))
                    .h(px(line.h))
                    .bg(t.axis_line)
                    .into_any_element(),
            );
            let g = d.handle(GRAB);
            let row = d.axis == Axis::Row;
            out.push(
                div()
                    .id(("divider", i))
                    .absolute()
                    .left(px(g.x))
                    .top(px(g.y))
                    .w(px(g.w))
                    .h(px(g.h))
                    .when(row, |d| d.cursor_col_resize())
                    .when(!row, |d| d.cursor_row_resize())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.split_drag = Some(d.clone());
                        }),
                    )
                    .into_any_element(),
            );
        }
        // Every pane is unavailable for the same reason: one switch for the whole tab.
        if let Some((reason, label)) = self.tab_offer(cx) {
            out.push(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom(crate::zoom::ui(16.0))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .id("switch-all")
                            .occlude()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .py_2()
                            .bg(t.panel)
                            .border_1()
                            .border_color(t.axis_line)
                            .rounded_md()
                            .text_size(crate::zoom::ui(12.0))
                            .text_color(t.text_dim)
                            .child(format!("Every pane: {reason}"))
                            .child(
                                div()
                                    .id("switch-all-button")
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(t.accent)
                                    .text_color(t.text_main)
                                    .hover(|d| d.bg(t.accent.opacity(0.16)))
                                    .child(format!("Switch all panes to {label}"))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                                            cx.stop_propagation();
                                            this.run(Command::ShowSourceInTab, window, cx);
                                        }),
                                    ),
                            )
                            .child("Ctrl+Shift+P: commands"),
                    )
                    .into_any_element(),
            );
            // Above any open header dropdown, which would otherwise cover it.
            let banner = out.pop().expect("just pushed");
            out.push(
                gpui::deferred(banner)
                    .with_priority(crate::dropdown::OVERLAY_PRIORITY)
                    .into_any_element(),
            );
        }
        // Focus: always visible on the focused pane (docs/design.md: violet is focus).
        if let Some(i) = tab.layout.index_of(&tab.focused)
            && let Some(p) = placed.iter().find(|p| p.index == i)
        {
            let r = p.rect;
            let (color, width) = if self.window_active {
                (t.accent, crate::zoom::ui(2.0))
            } else {
                (t.text_dim, px(1.0))
            };
            out.push(
                div()
                    .absolute()
                    .left(px(r.x))
                    .top(px(r.y))
                    .w(px(r.w))
                    .h(px(r.h))
                    .border(width)
                    .border_color(color)
                    .into_any_element(),
            );
        }
        out
    }

    fn menu_overlay(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu.as_ref()?;
        Some(menu.render(
            &self.theme,
            window.viewport_size(),
            |this: &mut Self| &mut this.menu,
            |this, cmd, window, cx| this.run(cmd, window, cx),
            cx,
        ))
    }

    fn prompt_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.prompt.as_ref()?;
        let t = self.theme;
        let row = |id: &'static str, i: usize, sel: bool, cx: &mut Context<Self>| {
            div()
                .id((id, i))
                .px_2()
                .py_1()
                .flex()
                .flex_row()
                .justify_between()
                .gap_2()
                .border_l_2()
                .border_color(if sel {
                    t.accent
                } else {
                    gpui::transparent_black()
                })
                .when(sel, |d| d.bg(t.accent.opacity(0.16)))
                .when(!sel, |d| d.hover(|s| s.bg(t.accent.opacity(0.08))))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.prompt_pick(i, window, cx);
                    }),
                )
        };
        let (panel, title, body, error) = match p {
            Prompt::SaveAs { text, error } => (
                crate::popover::Panel::SaveAs,
                "Save workspace as",
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .child(if text.is_empty() {
                        div().text_color(t.text_dim).child("name")
                    } else {
                        div().text_color(t.text_main).child(text.clone())
                    })
                    .child(
                        div()
                            .w(crate::zoom::ui(2.0))
                            .h(crate::zoom::ui(15.0))
                            .bg(t.accent),
                    ),
                error.clone(),
            ),
            Prompt::Open {
                items,
                selected,
                error,
            } => {
                let mut list = div().flex().flex_col().py_1();
                // A window of rows around the selection (the wheel and keys move it).
                let first = selected.saturating_sub(13);
                for (i, name) in items.iter().enumerate().skip(first).take(14) {
                    let sel = i == *selected;
                    let builtin = wsdoc::is_builtin(name);
                    list = list.child(
                        row("open-row", i, sel, cx)
                            .child(div().text_color(t.text_main).child(name.clone()))
                            .child(div().text_color(t.text_dim).child(if builtin {
                                "built-in"
                            } else {
                                "saved"
                            })),
                    );
                }
                (
                    crate::popover::Panel::OpenWorkspace,
                    "Open workspace",
                    list,
                    error.clone(),
                )
            }
            Prompt::Palette {
                query,
                shown,
                selected,
                ..
            } => {
                let mut list = div().flex().flex_col().py_1();
                // A window of rows around the selection.
                let first = selected.saturating_sub(13);
                for (i, cmd) in shown.iter().enumerate().skip(first).take(14) {
                    let sel = i == *selected;
                    list = list.child(
                        row("palette-row", i, sel, cx)
                            .child(div().text_color(t.text_main).child(commands::label(*cmd)))
                            .child(div().text_color(t.text_dim).child(commands::hint(*cmd))),
                    );
                }
                if shown.is_empty() {
                    list = list.child(
                        div()
                            .px_3()
                            .py_1()
                            .text_color(t.text_dim)
                            .child("no matching command"),
                    );
                }
                let input = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(t.axis_line)
                    .child(if query.is_empty() {
                        div().text_color(t.text_dim).child("Type a command")
                    } else {
                        div().text_color(t.text_main).child(query.clone())
                    })
                    .child(
                        div()
                            .w(crate::zoom::ui(2.0))
                            .h(crate::zoom::ui(15.0))
                            .bg(t.accent),
                    );
                (
                    crate::popover::Panel::Palette,
                    "Commands",
                    div().flex().flex_col().child(input).child(list),
                    None,
                )
            }
        };
        Some(
            div()
                .absolute()
                .top(px(self.bar_h() + 24.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    crate::popover::floating(
                        panel,
                        title,
                        &t,
                        div()
                            .id("prompt")
                            .occlude()
                            .w(crate::zoom::ui(460.0))
                            .flex()
                            .flex_col()
                            .bg(t.panel)
                            .border_1()
                            .border_color(t.axis_line)
                            .rounded_b_md()
                            .text_size(crate::zoom::ui(13.0))
                            .on_scroll(cx.listener(|this, ev: &gpui::ScrollEvent, window, cx| {
                                let n = crate::popover::wheel_rows(ev, window);
                                if n != 0 {
                                    this.prompt_wheel(n);
                                    cx.notify();
                                }
                            }))
                            .child(body)
                            .when_some(error, |d, e| {
                                d.child(
                                    div()
                                        .px_3()
                                        .py_1()
                                        .border_t_1()
                                        .border_color(t.axis_line)
                                        .text_size(crate::zoom::ui(11.0))
                                        .text_color(t.text_main)
                                        .child(e),
                                )
                            }),
                        Box::new(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            this.prompt = None;
                            cx.notify();
                        })),
                    )
                    // Above any open header dropdown, which would otherwise cover it.
                    .priority(crate::dropdown::OVERLAY_PRIORITY),
                )
                .into_any_element(),
        )
    }

    fn drag_ghost(&self) -> Option<AnyElement> {
        let (now, label) = match (&self.tab_drag, &self.pane_drag) {
            (Some(d), _) if d.active => (d.now, "move tab"),
            (_, Some(d)) if d.active => (d.now, "drop outside the window to pop out"),
            _ => return None,
        };
        let t = self.theme;
        Some(
            div()
                .absolute()
                .left(now.x - px(tab_w() * 0.5))
                .top(now.y - crate::zoom::ui(10.0))
                .min_w(px(tab_w()))
                .h(px(tab_bar_h() - 6.0))
                .px_3()
                .flex()
                .items_center()
                .bg(t.panel.opacity(0.9))
                .border_1()
                .border_color(t.accent)
                .rounded_md()
                .text_size(crate::zoom::ui(12.0))
                .text_color(t.text_main)
                .child(label)
                .into_any_element(),
        )
    }
}

impl Render for WindowView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        // The UI zoom (`crate::zoom`): every rem-based size in the window follows it.
        window.set_rem_size(crate::zoom::rem());
        self.screen = screen_rect(window);
        if let Some(m) = self.shot_menu.clone()
            && self.open_shot_menu(&m, cx)
        {
            self.shot_menu = None;
        }
        if !window.is_window_hovered() && !self.hover_pinned {
            // GPUI on Windows sends no exit event: the pointer left, so nothing is hovered.
            self.hover_pane = None;
            self.hover_tab = None;
        }
        let area = self.content_area(window, cx);
        let hints = match self.tabs.get(self.active).and_then(|t| t.focused_handle()) {
            Some(PaneHandle::Dom(_)) => {
                "DOM: wheel scroll, C center, G / Shift+G grouping, Ctrl+K symbol"
            }
            Some(PaneHandle::Tape(_)) => "Time & sales: type a symbol or Ctrl+K: switch",
            Some(PaneHandle::Journal(_)) => {
                "Journal: click or Up/Down to select, Enter: tags and note, Snapshot column: view, click the stats: today / all dates"
            }
            Some(PaneHandle::Positions(_)) => {
                "Positions: click a row to show it in the linked chart and DOM"
            }
            Some(PaneHandle::Chain(_)) => {
                "Option chain: click an ask to buy or a bid to sell, or Enter on the selected cell; Up/Down strike, Left/Right expiry, Tab calls / puts, Shift+Tab bid / ask, type a symbol or Ctrl+K: switch"
            }
            _ => chart_view::HINTS_SHORT,
        };
        let content = if self.tabs.is_empty() {
            Vec::new()
        } else {
            self.panes(area, cx)
        };
        let menu = self.menu_overlay(window, cx);
        let bar =
            tabstate::shows_tab_bar(self.tabs.len()).then(|| self.tab_bar(cx).into_any_element());
        let status = self.status(cx).into_any_element();
        // F1 / Clean chart hide the key hints; the footer stays while it has state to show.
        let (footer, hide_hints) = (self.footer_shown(cx), cx.global::<Workspace>().hide_hints);
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(t.bg)
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_key_capture))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .children(bar)
            .child(
                div()
                    .relative()
                    .flex_none()
                    .w(px(area.w))
                    .h(px(area.h))
                    .overflow_hidden()
                    .children(content),
            )
            .children(footer.then(|| {
                div()
                    .h(px(hint_h()))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px_3()
                    .gap_4()
                    .overflow_hidden()
                    .text_size(crate::zoom::ui(11.0))
                    .text_color(t.text_dim)
                    .bg(t.panel)
                    .border_t_1()
                    .border_color(t.axis_line)
                    .child(div().flex_1().min_w_0().truncate().when(!hide_hints, |d| {
                        d.child(format!("{hints}  ·  {}", commands::workspace_hints()))
                    }))
                    .child(status)
            }))
            .children(menu)
            .children(self.prompt_overlay(cx))
            .children(self.drag_ghost())
    }
}

/// Icon controls over the right end of a pane's header (`pane::controls`). Callers build
/// them only for the hovered pane (docs/design.md: no permanent chrome; and no per-frame cost
/// for the others). `chart_only` draws the chart-only icon in the accent color while active.
/// `run` issues the command on pane `id`.
/// Runs a header control's command on pane `id`; `at`: where it was clicked (the menu opens
/// there).
pub(crate) type ControlRun<V> =
    fn(&mut V, Command, PaneId, Option<Point<Pixels>>, &mut Window, &mut Context<V>);

pub(crate) fn header_controls<V: 'static>(
    t: &Theme,
    kind: PaneKind,
    detached: bool,
    chart_only: bool,
    run: ControlRun<V>,
    id: PaneId,
    cx: &mut Context<V>,
) -> AnyElement {
    // Exactly the gutter every pane header keeps free (`pane::controls_gutter`): the controls
    // never cover header content.
    let mut row = div()
        .absolute()
        .top_0()
        .right_0()
        // The header's own height, so the icons sit on its text's center line.
        .h(crate::zoom::ui(crate::pane::header_height(kind) - 1.0))
        .w(px(crate::pane::controls_gutter(kind)))
        .overflow_hidden()
        .justify_end()
        .px(px(crate::pane::CONTROL_PAD))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(crate::pane::CONTROL_GAP))
        .bg(t.panel);
    for (i, c) in crate::pane::controls(kind, detached).iter().enumerate() {
        let cmd = c.cmd;
        let on = chart_only && cmd == Command::ChartOnly;
        row = row.child(
            crate::icons::button(("pane-control", i), c.icon, c.name, Some(cmd), on, t)
                .relative()
                .children(crate::layout_rec::marker(
                    lines_core::layout_lint::Kind::Button,
                    c.name,
                    0,
                    "hover-controls",
                ))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        // The menu opens where it was asked for (under the ⋯).
                        run(this, cmd, id, Some(ev.position), window, cx);
                        cx.notify();
                    }),
                ),
        );
    }
    row.children(crate::layout_rec::marker(
        lines_core::layout_lint::Kind::HoverSlot,
        "hover controls",
        0,
        "",
    ))
    .into_any_element()
}

/// The window's screen rectangle (logical px).
fn screen_rect(window: &Window) -> Rect {
    let b = window.bounds();
    Rect::new(
        f32::from(b.origin.x),
        f32::from(b.origin.y),
        f32::from(b.size.width),
        f32::from(b.size.height),
    )
}

/// Where a window is, for saving.
pub(crate) fn placement(window: &Window, cx: &App) -> Option<Placement> {
    let (state, b) = match window.window_bounds() {
        WindowBounds::Windowed(b) => (WindowState::Windowed, b),
        WindowBounds::Maximized(b) => (WindowState::Maximized, b),
        WindowBounds::Fullscreen(b) => (WindowState::Fullscreen, b),
    };
    let display = window
        .display(cx)
        .and_then(|d| d.uuid().ok())
        .map(|u| u.to_string());
    Some(Placement {
        display,
        state,
        x: f32::from(b.origin.x),
        y: f32::from(b.origin.y),
        w: f32::from(b.size.width),
        h: f32::from(b.size.height),
    })
}

/// Window options for a saved placement (or the default size on the primary display).
pub(crate) fn window_options(
    placement: Option<&Placement>,
    cx: &App,
) -> (Option<WindowBounds>, Option<DisplayId>) {
    let displays = cx.displays();
    let infos: Vec<DisplayInfo> = displays
        .iter()
        .map(|d| {
            let b = d.bounds();
            DisplayInfo {
                uuid: d.uuid().ok().map(|u| u.to_string()),
                bounds: Rect::new(
                    f32::from(b.origin.x),
                    f32::from(b.origin.y),
                    f32::from(b.size.width),
                    f32::from(b.size.height),
                ),
            }
        })
        .collect();
    let Some(p) = placement else {
        let env = env(cx);
        let primary = cx.primary_display();
        let (w, h) = env.default_size.unwrap_or_else(|| {
            primary.as_ref().map_or((1280.0, 800.0), |d| {
                let b = d.bounds();
                (
                    f32::from(b.size.width) * 0.85,
                    f32::from(b.size.height) * 0.85,
                )
            })
        });
        let bounds = Bounds::centered(primary.map(|d| d.id()), size(px(w), px(h)), cx);
        return (Some(WindowBounds::Windowed(bounds)), None);
    };
    let (i, r) = wsdoc::restore_placement(p, &infos);
    let b = Bounds {
        origin: point(px(r.x), px(r.y)),
        size: size(px(r.w), px(r.h)),
    };
    let wb = match p.state {
        WindowState::Windowed => WindowBounds::Windowed(b),
        WindowState::Maximized => WindowBounds::Maximized(b),
        WindowState::Fullscreen => WindowBounds::Fullscreen(b),
    };
    (Some(wb), i.map(|i| displays[i].id()))
}

/// Open a window holding `tabs`. `options` overrides everything for screenshots/benchmarks.
pub fn open_window(
    tabs: Vec<Tab>,
    active: usize,
    bounds: Option<WindowBounds>,
    options: Option<WindowOptions>,
    cx: &mut App,
) -> Result<WindowHandle<WindowView>> {
    let opts = options.unwrap_or_else(|| WindowOptions {
        window_bounds: bounds,
        titlebar: Some(TitlebarOptions {
            title: Some("Lines".into()),
            ..Default::default()
        }),
        icon: crate::window_icon(),
        window_min_size: Some(size(px(MIN_WINDOW_W), px(MIN_WINDOW_H))),
        ..Default::default()
    });
    let handle = cx.open_window(opts, move |window, cx| {
        cx.new(|cx| WindowView::new(tabs, active, window, cx))
    })?;
    cx.global_mut::<Workspace>().windows.push(handle);
    let _ = handle.update(cx, |_, window, cx| {
        let root = cx.entity();
        window.on_window_should_close(cx, move |window, cx| {
            on_should_close(handle, &root, window, cx)
        });
    });
    Ok(handle)
}

/// A hidden window at an exact size (screenshots and the benchmark).
pub fn hidden_window(title: &str, (w, h): (f32, f32)) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(w), px(h)),
        })),
        titlebar: Some(TitlebarOptions {
            title: Some(title.to_owned().into()),
            ..Default::default()
        }),
        show: false,
        focus: false,
        ..Default::default()
    }
}

/// Open a window from its saved form.
pub fn open_doc_window(
    doc: &WindowDoc,
    options: Option<WindowOptions>,
    cx: &mut App,
) -> Result<WindowHandle<WindowView>> {
    let tabs: Vec<Tab> = doc
        .tabs
        .iter()
        .map(|t| WindowView::build_tab(t, cx))
        .collect();
    let (bounds, display) = window_options(doc.placement.as_ref(), cx);
    let options = options.or_else(|| {
        display.map(|id| WindowOptions {
            window_bounds: bounds,
            display_id: Some(id),
            titlebar: Some(TitlebarOptions {
                title: Some("Lines".into()),
                ..Default::default()
            }),
            icon: crate::window_icon(),
            ..Default::default()
        })
    });
    open_window(tabs, doc.active_tab, bounds, options, cx)
}

/// What switching the tick mode on a time chart did: it shows on the chart's next tick
/// timeframe ("1000t ∞" in the chip), not now.
fn tick_mode_notice(on: bool, tf: lines_core::Timeframe) -> String {
    let mode = if on {
        "run continuously"
    } else {
        "restart at each session open"
    };
    format!("tick bars on this chart will {mode} (shown on a tick timeframe; now {tf})")
}

#[cfg(test)]
#[test]
fn tick_mode_notice_text() {
    assert_eq!(
        tick_mode_notice(true, lines_core::Timeframe::Minutes(5)),
        "tick bars on this chart will run continuously (shown on a tick timeframe; now 5m)"
    );
    assert_eq!(
        tick_mode_notice(false, lines_core::Timeframe::Hours(1)),
        "tick bars on this chart will restart at each session open (shown on a tick \
         timeframe; now 1h)"
    );
}

/// Show a notice that needs no action: it replaces an identical one (counted) and goes away
/// after `notice::NOTICE_TTL`. One timer per notice, nothing ticking in between, so an idle
/// window stays idle.
pub(crate) fn notice(text: String, cx: &mut App) {
    tracing::info!("{text}");
    cx.global_mut::<Workspace>()
        .notices
        .push(text, false, Instant::now());
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(crate::notice::NOTICE_TTL + Duration::from_millis(20))
            .await;
        cx.update(|cx| {
            if cx.global_mut::<Workspace>().notices.expire(Instant::now()) {
                notify_windows(cx);
            }
        });
    })
    .detach();
}

/// Redraw every main window's own chrome (footer, tab bar); panes repaint only if they changed.
fn notify_windows(cx: &mut App) {
    for w in cx.global::<Workspace>().windows.clone() {
        let _ = w.update(cx, |_, _, cx| cx.notify());
    }
}

/// Write the journal's closed trades as CSV to `<config dir>/journal/export-<time>.csv` (on
/// the journal's thread; a failure is logged) and say where.
fn export_journal(cx: &mut App) {
    let Some(t) = crate::trading::Trading::try_global(cx) else {
        notice("trading isn't running: no journal to export".into(), cx);
        return;
    };
    let journal = t.journal.clone();
    let Some(dir) = cx.global::<Workspace>().config_dir.clone() else {
        notice(
            "no config directory: the journal can't be exported here".into(),
            cx,
        );
        return;
    };
    let now = lines_feed::now_utc();
    let d = lines_core::CivilDate::from_utc(now);
    let s = now.0.rem_euclid(86_400_000) / 1000;
    let path = dir.join("journal").join(format!(
        "export-{}{:02}{:02}-{:02}{:02}{:02}-{:03}.csv",
        d.year,
        d.month,
        d.day,
        s / 3600,
        s / 60 % 60,
        s % 60,
        now.0.rem_euclid(1000)
    ));
    journal.export(path.clone());
    notice(format!("exporting the journal to {}", path.display()), cx);
}

/// Show or hide the chart headers' performance stats (a workspace preference, saved with it).
fn set_show_perf_stats(show: bool, cx: &mut App) {
    let ws = cx.global_mut::<Workspace>();
    if ws.show_perf_stats != show {
        ws.show_perf_stats = show;
        cx.refresh_windows();
    }
}

/// Show or hide the latency HUD (a workspace preference, saved with it). Recording follows
/// it: nothing is measured while it is hidden.
pub fn set_show_latency_hud(show: bool, cx: &mut App) {
    lines_core::latency::set_enabled(show);
    let ws = cx.global_mut::<Workspace>();
    if ws.show_latency_hud != show {
        ws.show_latency_hud = show;
        cx.refresh_windows();
    }
}

/// Change the look of every window and pane now (the Settings window's appearance), and of
/// panes created from now on.
pub fn set_theme(theme: Theme, cx: &mut App) {
    crate::journal_shots::set_theme(theme);
    let Some(ws) = cx.try_global::<Workspace>() else {
        return;
    };
    ws.env.theme.set(theme);
    let windows = ws.windows.clone();
    let pane_windows = ws.pane_windows.clone();
    for w in windows {
        let _ = w.update(cx, |v, _, cx| {
            v.theme = theme;
            let handles: Vec<PaneHandle> = v
                .tabs
                .iter()
                .flat_map(|t| t.handles().cloned().collect::<Vec<_>>())
                .collect();
            for h in handles {
                h.set_theme(theme, cx);
            }
            cx.notify();
        });
    }
    for w in pane_windows {
        let _ = w.update(cx, |v, _, cx| v.set_theme(theme, cx));
    }
    cx.refresh_windows();
}

/// Zoom in, out, or back to 100 % (`crate::zoom`), from any window.
pub(crate) fn zoom_command(cmd: Command, cx: &mut App) {
    let dir = match cmd {
        Command::ZoomIn => 1,
        Command::ZoomOut => -1,
        _ => 0,
    };
    set_ui_zoom(crate::zoom::step(crate::zoom::percent(), dir), cx);
}

/// Every window at this zoom now, saved; a brief notice says the new size ("110%").
pub(crate) fn set_ui_zoom(pct: u16, cx: &mut App) {
    if !crate::zoom::set(pct) {
        return;
    }
    let pct = crate::zoom::percent();
    crate::picker_view::update_settings(cx, |s| s.ui_zoom = crate::zoom::saved(pct));
    applied_ui_zoom(cx);
}

/// After the zoom changed (a key, the palette, Settings): say so and redraw every window.
pub(crate) fn applied_ui_zoom(cx: &mut App) {
    if cx.has_global::<Workspace>() {
        notice(format!("{}%", crate::zoom::percent()), cx);
    }
    cx.refresh_windows();
}

/// Show or hide the key-hint bar in every window (a workspace preference, saved with it).
pub(crate) fn set_hide_hints(hide: bool, cx: &mut App) {
    let ws = cx.global_mut::<Workspace>();
    if ws.hide_hints != hide {
        ws.hide_hints = hide;
        cx.refresh_windows();
    }
}

/// Take a workspace's name and settings (not its windows).
pub fn set_meta(doc: &WorkspaceDoc, cx: &mut App) {
    let ws = cx.global_mut::<Workspace>();
    ws.name = doc.name.clone();
    ws.crosshair_sync = doc.crosshair_sync;
    ws.hide_hints = doc.hide_hints;
    ws.show_perf_stats = doc.show_perf_stats;
    set_show_latency_hud(doc.show_latency_hud, cx);
}

/// Open a whole workspace (startup).
pub fn open_workspace(doc: &WorkspaceDoc, cx: &mut App) -> Result<()> {
    set_meta(doc, cx);
    for w in &doc.windows {
        open_doc_window(w, None, cx)?;
    }
    Ok(())
}

/// Replace the open workspace with `doc` (Open workspace…). The new panes are created before
/// the old ones are dropped, so feeds both show keep running.
fn switch_workspace(doc: WorkspaceDoc, keep: gpui::AnyWindowHandle, cx: &mut App) {
    let old: Vec<_> = cx.global::<Workspace>().windows.clone();
    let mut doc = doc;
    if doc.normalise().is_err() {
        return;
    }
    let first = doc.windows.remove(0);
    let tabs: Vec<Tab> = first
        .tabs
        .iter()
        .map(|t| WindowView::build_tab(t, cx))
        .collect();
    {
        let ws = cx.global_mut::<Workspace>();
        ws.name = doc.name.clone();
        ws.crosshair_sync = doc.crosshair_sync;
        ws.crosshair_sync_price = doc.crosshair_sync_price;
        ws.hide_hints = doc.hide_hints;
        ws.show_perf_stats = doc.show_perf_stats;
    }
    set_show_latency_hud(doc.show_latency_hud, cx);
    for w in &doc.windows {
        if let Err(e) = open_doc_window(w, None, cx) {
            cx.global_mut::<Workspace>()
                .add_report(format!("couldn't open a window: {e:#}"));
        }
    }
    let keep_id = keep.window_id();
    for w in old {
        if w.window_id() != keep_id {
            let _ = w.update(cx, |_, window, _| window.remove_window());
        }
    }
    if let Some(h) = cx
        .global::<Workspace>()
        .windows
        .iter()
        .find(|w| w.window_id() == keep_id)
        .copied()
    {
        let _ = h.update(cx, |wv, window, cx| {
            let old_tabs = std::mem::take(&mut wv.tabs);
            for t in tabs {
                wv.adopt(t, cx);
            }
            drop(old_tabs);
            let a = first.active_tab.min(wv.tabs.len() - 1);
            wv.activate_tab(a, window, cx);
        });
    }
    cx.defer(prune_pane_windows);
}

/// Close pane windows whose pane is no longer detached in some tab (its tab closed, its window
/// closed, the workspace was replaced).
fn prune_pane_windows(cx: &mut App) {
    let windows = cx.global::<Workspace>().windows.clone();
    let panes: Vec<_> = cx.global::<Workspace>().pane_windows.clone();
    for p in panes {
        let Ok(id) = p.read(cx).map(|pw| pw.id) else {
            continue;
        };
        let alive = windows.iter().any(|w| {
            w.read(cx).is_ok_and(|wv| {
                wv.tabs
                    .iter()
                    .any(|t| t.panes.contains_key(&id) && t.state(id) == PaneState::Detached)
            })
        });
        if !alive {
            pane_window::close(id, cx);
        }
    }
}

/// The main window holding pane `id`.
fn window_with(id: PaneId, cx: &App) -> Option<WindowHandle<WindowView>> {
    cx.global::<Workspace>()
        .windows
        .iter()
        .find(|w| w.read(cx).is_ok_and(|wv| wv.has_pane(id)))
        .copied()
}

/// A detached pane back to its previous spot (its leaf never left the tab).
pub(crate) fn reattach_pane(id: PaneId, cx: &mut App) {
    match window_with(id, cx) {
        Some(w) => {
            let _ = w.update(cx, |wv, window, cx| {
                wv.reattach(id, window, cx);
                window.activate_window();
            });
        }
        // Its tab is gone: nothing to return to.
        None => pane_window::close(id, cx),
    }
}

/// A detached pane docked beside the chart that last had focus (right of it), wherever that
/// is; it leaves its old spot. Falls back to re-attaching.
pub(crate) fn dock_beside_chart(id: PaneId, cx: &mut App) {
    let chart = cx.global::<Workspace>().last_chart;
    let (Some(chart), Some(from)) = (chart, window_with(id, cx)) else {
        notice(
            "no chart to dock beside yet: re-attached instead".into(),
            cx,
        );
        reattach_pane(id, cx);
        return;
    };
    let Some(to) = window_with(chart, cx) else {
        reattach_pane(id, cx);
        return;
    };
    let slot = from
        .update(cx, |wv, _, cx| {
            let s = wv.take_pane(id);
            wv.sync_visibility(cx);
            cx.notify();
            s
        })
        .ok()
        .flatten();
    let Some(slot) = slot else {
        // Its tab's only pane: it can only go back there.
        reattach_pane(id, cx);
        return;
    };
    let _ = to.update(cx, |wv, window, cx| {
        wv.insert_beside(chart, id, slot, window, cx);
        window.activate_window();
    });
    pane_window::close(id, cx);
}

/// App-wide hooks for the interactive app: forget closed windows (also those closed by
/// moving their last tab away), quit when none is left, and save on quit.
pub fn install(cx: &mut App) {
    cx.on_window_closed(|cx, id| {
        if cx.has_global::<Workspace>() {
            let ws = cx.global_mut::<Workspace>();
            ws.windows.retain(|w| w.window_id() != id);
            ws.pane_windows.retain(|w| w.window_id() != id);
            if ws.windows.is_empty() {
                // The last main window went: popped-out panes go with it (already saved).
                for p in ws.pane_windows.clone() {
                    let _ = p.update(cx, |_, window, _| window.remove_window());
                }
            } else {
                prune_pane_windows(cx);
            }
        }
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
    cx.on_app_quit(|cx| {
        // Quitting with windows open (e.g. Cmd+Q): save them. After the last window closed
        // there is nothing left to snapshot, and `on_should_close` already saved.
        if !cx.global::<Workspace>().windows.is_empty() {
            let doc = snapshot(cx);
            autosave_write(&doc, cx);
        }
        let writer = cx.global::<Workspace>().writer.clone();
        // The journal's last trades too (macOS exits here, before `run` returns).
        let journal = crate::trading::Trading::try_global(cx).map(|t| t.journal.clone());
        async move {
            if let Some(w) = writer {
                w.flush(Duration::from_secs(3));
            }
            if let Some(j) = journal {
                j.flush(Duration::from_secs(3));
            }
        }
    })
    .detach();
}

/// The last window closing saves everything first (including itself); other windows just go.
fn on_should_close(
    me: WindowHandle<WindowView>,
    root: &Entity<WindowView>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let last = cx.global::<Workspace>().windows.len() <= 1;
    if last {
        if let Some(doc) = snapshot_with(me, root, window, cx) {
            autosave_write(&doc, cx);
        }
        if let Some(w) = cx.global::<Workspace>().writer.clone() {
            w.flush(Duration::from_secs(3));
        }
    }
    cx.global_mut::<Workspace>()
        .windows
        .retain(|w| w.window_id() != me.window_id());
    true
}

/// The whole workspace as a document. `current` is the window whose update we are inside, if
/// any (it can't be updated through its handle).
fn snapshot_with(
    current: WindowHandle<WindowView>,
    root: &Entity<WindowView>,
    window: &mut Window,
    cx: &mut App,
) -> Option<WorkspaceDoc> {
    let ws = cx.global::<Workspace>();
    let (name, sync, sync_price, hide_hints, show_perf_stats, show_latency_hud, windows) = (
        ws.name.clone(),
        ws.crosshair_sync,
        ws.crosshair_sync_price,
        ws.hide_hints,
        ws.show_perf_stats,
        ws.show_latency_hud,
        ws.windows.clone(),
    );
    let mut docs = Vec::new();
    for w in windows {
        let doc = if w.window_id() == current.window_id() {
            root.update(cx, |wv, cx| wv.doc(window, cx))
        } else {
            w.update(cx, |wv, window, cx| wv.doc(window, cx)).ok()?
        };
        docs.push(doc);
    }
    Some(WorkspaceDoc {
        name,
        crosshair_sync: sync,
        crosshair_sync_price: sync_price,
        hide_hints,
        show_perf_stats,
        show_latency_hud,
        windows: docs,
    })
}

/// The whole workspace as a document (from outside any window update).
/// Every pane in every window, with its id (diagnostics).
pub fn all_panes(cx: &App) -> Vec<(PaneId, PaneHandle)> {
    cx.global::<Workspace>()
        .windows
        .iter()
        .filter_map(|w| w.read(cx).ok())
        .flat_map(|wv| {
            wv.tabs
                .iter()
                .flat_map(|t| t.panes.iter().map(|(id, s)| (*id, s.handle.clone())))
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn snapshot(cx: &mut App) -> WorkspaceDoc {
    let ws = cx.global::<Workspace>();
    let (name, sync, sync_price, hide_hints, show_perf_stats, show_latency_hud, windows) = (
        ws.name.clone(),
        ws.crosshair_sync,
        ws.crosshair_sync_price,
        ws.hide_hints,
        ws.show_perf_stats,
        ws.show_latency_hud,
        ws.windows.clone(),
    );
    let windows = windows
        .into_iter()
        .filter_map(|w| w.update(cx, |wv, window, cx| wv.doc(window, cx)).ok())
        .collect();
    WorkspaceDoc {
        name,
        crosshair_sync: sync,
        crosshair_sync_price: sync_price,
        hide_hints,
        show_perf_stats,
        show_latency_hud,
        windows,
    }
}

fn autosave_write(doc: &WorkspaceDoc, cx: &mut App) {
    if doc.windows.is_empty() {
        return;
    }
    let Ok(bytes) = versioned::encode(&wsdoc::FORMAT, doc) else {
        return;
    };
    let ws = cx.global_mut::<Workspace>();
    let (Some(a), Some(w)) = (ws.autosave.as_mut(), ws.writer.as_ref()) else {
        return;
    };
    if a.last != bytes {
        a.last = bytes.clone();
        w.write(a.path.clone(), bytes);
    }
}

/// Look again at what's wrong with a trigger. Busy this instant: next time.
pub(crate) fn refresh_trigger_status(cx: &mut App) {
    let Some(trading) = crate::trading::Trading::try_global(cx) else {
        return;
    };
    // The chains a person can see: the active tab's shown panes in a window on screen, and
    // anything popped out.
    let mut shown: Vec<(lines_trading::AccountId, lines_core::ProductId)> = Vec::new();
    for w in cx.global::<Workspace>().windows.clone() {
        let _ = w.read_with(cx, |wv, cx| {
            for (i, tab) in wv.tabs.iter().enumerate() {
                let front = i == wv.active && wv.window_visible;
                for (id, pane) in &tab.panes {
                    let state = tab.state(*id);
                    let visible = state == PaneState::Detached || (front && state.is_shown());
                    if let (true, PaneHandle::Chain(c)) = (visible, &pane.handle)
                        && let Some(s) = c.read(cx).shows_triggers()
                    {
                        shown.push(s);
                    }
                }
            }
        });
    }
    let Some(st) = trading.cache.try_read() else {
        return;
    };
    let status = crate::chain_lines::trigger_status(
        st.triggers(),
        st.trigger_feeds_missing(),
        |account, product| shown.iter().any(|(a, p)| a == account && *p == product),
    );
    drop(st);
    let open = crate::positions_view::open_count(cx);
    let ws = cx.global_mut::<Workspace>();
    // Busy this instant: last time's count stands.
    let open = open.or(ws.open_positions);
    if ws.trigger_status != status || ws.open_positions != open {
        ws.trigger_status = status;
        ws.open_positions = open;
        for w in ws.windows.clone() {
            let _ = w.update(cx, |_, _, cx| cx.notify());
        }
    }
}

/// Keep the workspace's word on triggers fresh: a feed can go stale with nothing else changing.
pub fn start_trigger_watch(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.update(refresh_trigger_status);
            cx.background_executor().timer(Duration::from_secs(1)).await;
        }
    })
    .detach();
}

/// Snapshot the workspace every 2 s and save it when it changed (off the UI thread).
pub fn start_autosave(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            cx.update(|cx| {
                if cx.global::<Workspace>().windows.is_empty() {
                    return;
                }
                let doc = snapshot(cx);
                autosave_write(&doc, cx);
            });
        }
    })
    .detach();
}

fn save_named(name: &str, cx: &mut App) {
    let mut doc = snapshot(cx);
    doc.name = name.to_owned();
    let ws = cx.global_mut::<Workspace>();
    ws.name = name.to_owned();
    let (Some(dir), Some(w)) = (ws.config_dir.clone(), ws.writer.clone()) else {
        ws.add_report("no config directory: workspace not saved".into());
        return;
    };
    match versioned::encode(&wsdoc::FORMAT, &doc) {
        Ok(bytes) => w.write(persist::named_path(&dir, name), bytes),
        Err(e) => ws.add_report(format!("couldn't save workspace {name}: {e}")),
    }
}

/// A named or built-in workspace.
fn load_named(name: &str, cx: &mut App) -> Result<WorkspaceDoc, String> {
    if let Some(dir) = cx.global::<Workspace>().config_dir.clone() {
        let path = persist::named_path(&dir, name);
        if path.exists() {
            let l = versioned::load::<WorkspaceDoc>(&wsdoc::FORMAT, &path);
            if let Some(r) = l.report {
                cx.global_mut::<Workspace>().add_report(r.clone());
            }
            return l.value.ok_or_else(|| format!("couldn't read {name}"));
        }
    }
    wsdoc::builtin(name).ok_or_else(|| format!("no workspace named {name:?}"))
}

/// Crosshair sync: chart `from` has `ev` under its pointer. Only the charts of its link group
/// are told (`tabstate::crosshair_peers`), and the price only those of the same product with
/// price sync on (`tabstate::syncs_price`).
fn forward_crosshair(from: PaneId, ev: CrosshairTime, cx: &mut App) {
    let ws = cx.global::<Workspace>();
    if !ws.crosshair_sync {
        return;
    }
    let (with_price, windows) = (ws.crosshair_sync_price, ws.windows.clone());
    cx.defer(move |cx| {
        let mut charts: Vec<(PaneId, PaneHandle)> = Vec::new();
        for w in &windows {
            if let Ok(wv) = w.read(cx) {
                charts.extend(
                    wv.tabs
                        .iter()
                        .flat_map(|t| t.panes.iter())
                        .filter(|(_, s)| s.handle.kind() == PaneKind::Chart)
                        .map(|(id, s)| (*id, s.handle.clone())),
                );
            }
        }
        let Some(link) = charts
            .iter()
            .find(|(id, _)| *id == from)
            .map(|(_, h)| h.link(cx))
        else {
            return;
        };
        for (id, h) in charts {
            if id == from || !tabstate::crosshair_peers(link, h.link(cx)) {
                continue;
            }
            let PaneHandle::Chart(e) = &h else { continue };
            let product = e.read(cx).product();
            let price = ev
                .price
                .filter(|_| tabstate::syncs_price(with_price, ev.product, product));
            h.set_synced_time(ev.time, price, cx);
        }
    });
}

fn clear_synced_times(cx: &mut App) {
    let windows = cx.global::<Workspace>().windows.clone();
    cx.defer(move |cx| {
        for w in windows {
            let charts: Vec<PaneHandle> = w
                .read(cx)
                .map(|wv| wv.tabs.iter().flat_map(|t| t.handles().cloned()).collect())
                .unwrap_or_default();
            for h in charts {
                h.set_synced_time(None, None, cx);
            }
        }
    });
}

/// The app's P&L display (menus show and set it).
fn pnl_mode(cx: &App) -> crate::pnl::PnlDisplay {
    cx.try_global::<crate::picker_view::AppSettings>()
        .map(|s| s.settings.pnl_display)
        .unwrap_or_default()
}

/// `--context-menu` asked for what can't be shown: no image (a screenshot's failures exit
/// non-zero with no file).
fn shot_menu_failed(why: &str) -> ! {
    eprintln!("screenshot failed: --context-menu: {why}");
    std::process::exit(1)
}
