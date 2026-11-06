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
use std::time::Duration;

use anyhow::Result;
use gpui::{
    AnyElement, AnyView, App, AppContext, Bounds, Context, DisplayId, Entity, FocusHandle,
    FontWeight, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, StyleRefinement, Styled,
    Subscription as GpuiSubscription, TitlebarOptions, Window, WindowBounds, WindowHandle,
    WindowOptions, div, point, prelude::*, px, size,
};
use lines_core::UtcMillis;
use lines_feed::FeedConfig;
use lines_feed::availability::Availability;

use crate::chart_view::{self, ChartView, CrosshairTime, Fixture};
use crate::commands::{self, Command};
use crate::dom_view::{DomOptions, DomView};
use crate::feeds::{Feeds, PaneFeed};
use crate::layout::{
    Axis, Direction, Layout, LinkGroup, PaneKind, Preset, Rect, link_letter, next_link,
    trading_group,
};
use crate::pane::{FeedChanged, Pane};
use crate::persist::{self, Writer};
use crate::tape_view::{TapeOptions, TapeView};
use crate::theme::Theme;
use crate::versioned;
use crate::wsdoc::{
    self, DisplayInfo, PaneDoc, Placement, TabDoc, TabDrop, WindowDoc, WindowState, WorkspaceDoc,
};

const TAB_BAR_H: f32 = 28.0;
const HINT_H: f32 = 22.0;
const TAB_W: f32 = 168.0;
const TAB_X0: f32 = 4.0;
/// Width of a divider's grab handle (the visible line is one pixel).
const GRAB: f32 = 6.0;
/// A tab drag starts after this much movement.
const DRAG_START: f32 = 6.0;
const MENU_W: f32 = 260.0;
const MENU_ROW_H: f32 = 22.0;

// ---- environment and the app-wide state -----------------------------------------------------

/// What every pane is created from: the run's source and look.
pub struct PaneEnv {
    /// The run's source; panes set the target.
    pub base: FeedConfig,
    pub availability: Availability,
    pub theme: Theme,
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
    pub windows: Vec<WindowHandle<WindowView>>,
    /// A problem to show inline (e.g. a workspace file that couldn't be read), until dismissed.
    pub report: Option<String>,
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
            windows: Vec::new(),
            report: None,
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

    pub fn add_report(&mut self, r: String) {
        tracing::warn!("{r}");
        self.report = Some(match self.report.take() {
            Some(old) => format!("{old}  ·  {r}"),
            None => r,
        });
    }
}

fn env(cx: &App) -> Rc<PaneEnv> {
    cx.global::<Workspace>().env.clone()
}

// ---- panes ----------------------------------------------------------------------------------

/// A pane's identity for the lifetime of the app (unique across windows, so tabs can move).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(u64);

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
}

macro_rules! each {
    ($h:expr, $e:ident => $body:expr) => {
        match $h {
            PaneHandle::Chart($e) => $body,
            PaneHandle::Dom($e) => $body,
            PaneHandle::Tape($e) => $body,
        }
    };
}

impl PaneHandle {
    fn view(&self) -> AnyView {
        each!(self, e => e.clone().into())
    }

    pub fn kind(&self) -> PaneKind {
        match self {
            PaneHandle::Chart(_) => PaneKind::Chart,
            PaneHandle::Dom(_) => PaneKind::Dom,
            PaneHandle::Tape(_) => PaneKind::Tape,
        }
    }

    fn set_feed(&self, feed: PaneFeed, cx: &mut App) {
        each!(self, e => e.update(cx, |p, cx| p.set_feed(feed, cx)))
    }

    fn feed(&self, cx: &App) -> PaneFeed {
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

    fn set_visible(&self, visible: bool, cx: &mut App) {
        each!(self, e => e.update(cx, |p, _| p.common().visible = visible))
    }

    pub fn paints(&self, cx: &mut App) -> u64 {
        each!(self, e => e.update(cx, |p, _| p.common().paints))
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        each!(self, e => {
            let h = e.read(cx).focus_handle().clone();
            h.focus(window, cx);
        })
    }

    fn set_synced_time(&self, t: Option<UtcMillis>, cx: &mut App) {
        if let PaneHandle::Chart(e) = self {
            e.update(cx, |c, cx| {
                if c.common().visible {
                    c.set_synced_time(t, cx);
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
    focused: PaneId,
}

impl Tab {
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
            docs.insert(*id, slot.handle.doc(cx));
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
    let feed = PaneFeed::open(&registry, &base, &env.availability, doc.target.as_ref());
    if feed.sub.is_some() {
        // The recording belongs to the first pane that started a feed (one contract per file).
        env.record.borrow_mut().take();
    }
    let available = feed.unavailable.is_none();
    let theme = env.theme;
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
                if let Some(s) = &doc.chart {
                    v.restore(s);
                }
                v
            }))
        }
        PaneKind::Dom => {
            let opts = env.dom.clone();
            let frozen = env.frozen;
            PaneHandle::Dom(cx.new(|cx| {
                let mut v = DomView::new(feed, theme, frozen, opts, cx);
                if let Some(s) = &doc.dom {
                    v.restore(s);
                }
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

struct MenuItem {
    label: String,
    hint: &'static str,
    /// `None`: a separator.
    cmd: Option<Command>,
    checked: bool,
}

struct Menu {
    at: Point<Pixels>,
    items: Vec<MenuItem>,
    selected: usize,
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
}

pub struct WindowView {
    tabs: Vec<Tab>,
    active: usize,
    theme: Theme,
    focus: FocusHandle,
    /// Visible on screen (not minimised); always true without visibility tracking.
    window_visible: bool,
    window_active: bool,
    /// Last known screen bounds (for deciding where a dragged tab lands).
    screen: Rect,
    split_drag: Option<crate::layout::Divider>,
    tab_drag: Option<TabDrag>,
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
            theme: env.theme,
            focus: cx.focus_handle(),
            window_visible: true,
            window_active: true,
            screen: screen_rect(window),
            split_drag: None,
            tab_drag: None,
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
        let mut ids = Vec::new();
        let layout = doc.layout.map(&mut |p| {
            let id = PaneId::next();
            ids.push((id, p.clone()));
            id
        });
        for (id, p) in ids {
            panes.insert(
                id,
                PaneSlot {
                    handle: make_pane(&p, cx),
                    _subs: Vec::new(),
                },
            );
        }
        let leaves = layout.leaves();
        let focused = *leaves[doc.focused.min(leaves.len() - 1)];
        Tab {
            layout,
            panes,
            focused,
        }
    }

    /// Take ownership of a tab (new, or moved from another window): subscribe to its panes.
    fn adopt(&mut self, mut tab: Tab, cx: &mut Context<Self>) {
        for (id, slot) in tab.panes.iter_mut() {
            slot._subs = self.subscribe(*id, &slot.handle, cx);
        }
        self.tabs.push(tab);
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
                    forward_crosshair(id, ev.0, cx)
                }),
            ],
            PaneHandle::Dom(e) => {
                vec![cx.subscribe(e, move |this, _, ev: &FeedChanged, cx| {
                    feed_changed(this, ev, cx)
                })]
            }
            PaneHandle::Tape(e) => {
                vec![cx.subscribe(e, move |this, _, ev: &FeedChanged, cx| {
                    feed_changed(this, ev, cx)
                })]
            }
        }
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Only the active tab of a visible window is visible.
    fn sync_visibility(&mut self, cx: &mut App) {
        for (i, tab) in self.tabs.iter().enumerate() {
            let visible = self.window_visible && i == self.active;
            for h in tab.handles() {
                h.set_visible(visible, cx);
            }
        }
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
        let Some(group) = link else { return };
        let feed = ev.0.clone();
        let windows = cx.global::<Workspace>().windows.clone();
        cx.defer(move |cx| {
            for w in windows {
                let _ = w.update(cx, |wv, _, cx| {
                    wv.apply_group_feed(group, from, &feed, cx);
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

    fn content_area(window: &Window) -> Rect {
        let vs = window.viewport_size();
        let (w, h) = (f32::from(vs.width), f32::from(vs.height));
        Rect::new(0.0, 0.0, w, (h - TAB_BAR_H - HINT_H).max(0.0))
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
        if let Some(h) = self.tab().handle(id) {
            h.focus(window, cx);
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
                // A trio on the focused pane's market, in its link group.
                let target = self.focused_target(cx);
                let link = self
                    .tab()
                    .focused_handle()
                    .cloned()
                    .and_then(|h| h.link(cx));
                let layout = trading_group(link, &mut |k, l| PaneDoc::new(k, l, target.clone()));
                let tab = Self::build_tab(&TabDoc { focused: 0, layout }, cx);
                self.adopt(tab, cx);
                let i = self.tabs.len() - 1;
                self.activate_tab(i, window, cx);
            }
            Command::CloseTab => self.close_tab(self.active, window, cx),
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
            Command::Menu => {
                let area = Self::content_area(window);
                let at = self
                    .tab()
                    .layout
                    .index_of(&self.tab().focused)
                    .and_then(|i| self.tab().layout.place(area).get(i).map(|p| p.rect))
                    .map_or(point(px(40.0), px(TAB_BAR_H + 30.0)), |r| {
                        point(px(r.x + 24.0), px(TAB_BAR_H + r.y + 24.0))
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

    fn focused_target(&self, cx: &App) -> Option<wsdoc::TargetDoc> {
        self.tab()
            .focused_handle()
            .and_then(|h| h.feed(cx).target_doc)
    }

    fn move_focus(&mut self, dir: Direction, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.tab();
        let area = Self::content_area(window);
        let Some(i) = tab.layout.index_of(&tab.focused) else {
            return;
        };
        if let Some(j) = tab.layout.neighbour(i, dir, area) {
            let id = *tab.layout.leaves()[j];
            self.focus_pane(id, window, cx);
        }
    }

    fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        let Some(h) = self.tab().focused_handle().cloned() else {
            return;
        };
        // Same kind, symbol, link and view state as the source.
        let doc = h.doc(cx);
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
                let leaves = tab.layout.leaves();
                let next = *leaves[i.min(leaves.len() - 1)];
                self.focus_pane(next, window, cx);
            }
            Ok(None) => {
                tab.layout = Layout::Pane(focused);
                self.close_tab(self.active, window, cx);
            }
            Err(l) => tab.layout = l,
        }
    }

    /// Close a tab. The last tab of the last window stays (closing it would leave nothing to
    /// trade from); the last tab of another window closes that window.
    fn close_tab(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() == 1 {
            if cx.global::<Workspace>().windows.len() > 1 {
                window.remove_window();
            } else {
                cx.global_mut::<Workspace>()
                    .add_report("the last tab can't be closed".into());
            }
            return;
        }
        // Dropping the tab drops its panes; the registry stops feeds nobody else watches.
        self.tabs.remove(i);
        if self.active >= self.tabs.len() || self.active > i {
            self.active = self.active.saturating_sub(1).min(self.tabs.len() - 1);
        }
        let a = self.active;
        self.activate_tab(a, window, cx);
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
            origin: point(px(at.0 - TAB_W * 0.5), px(at.1 - 12.0)),
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
        let bar = Rect::new(own.x, own.y, own.w, TAB_BAR_H);
        let rights: Vec<f32> = (0..self.tabs.len())
            .map(|i| own.x + TAB_X0 + (i as f32 + 1.0) * TAB_W)
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

    fn open_menu(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(h) = self.tab().focused_handle().cloned() else {
            return;
        };
        let kind = h.kind();
        let link = h.link(cx);
        let sync = cx.global::<Workspace>().crosshair_sync;
        let item = |label: &str, cmd: Command, checked: bool| MenuItem {
            label: label.to_owned(),
            hint: commands::hint(cmd),
            cmd: Some(cmd),
            checked,
        };
        let sep = || MenuItem {
            label: String::new(),
            hint: "",
            cmd: None,
            checked: false,
        };
        let mut items = vec![
            item("Split right", Command::Split(Axis::Row), false),
            item("Split down", Command::Split(Axis::Column), false),
            item("Close pane", Command::ClosePane, false),
            sep(),
        ];
        for k in [PaneKind::Chart, PaneKind::Dom, PaneKind::Tape] {
            items.push(item(k.label(), Command::SetKind(k), k == kind));
        }
        items.push(sep());
        for l in [None, Some(0), Some(1), Some(2), Some(3)] {
            let label = match l {
                None => "Unlinked".to_owned(),
                Some(_) => format!("Link group {}", link_letter(l)),
            };
            let mut it = item(&label, Command::SetLink(l), l == link);
            if l.is_none() {
                it.hint = commands::hint(Command::CycleLink);
            }
            items.push(it);
        }
        items.push(sep());
        for p in Preset::ALL {
            items.push(item(
                &format!("Layout: {}", p.label()),
                Command::Preset(p),
                false,
            ));
        }
        items.push(sep());
        items.push(item("New tab", Command::NewTab, false));
        if self.tabs.len() > 1 {
            items.push(item(
                "Move tab to new window",
                Command::TabToNewWindow,
                false,
            ));
        }
        items.push(item("Crosshair sync", Command::ToggleCrosshairSync, sync));
        items.push(item("Save workspace as…", Command::SaveAs, false));
        items.push(item("Open workspace…", Command::Open, false));
        self.menu = Some(Menu {
            at,
            items,
            selected: 0,
        });
        cx.notify();
    }

    fn menu_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let n = menu.items.len();
        let step = |from: usize, d: isize, items: &[MenuItem]| {
            let mut i = from;
            for _ in 0..n {
                i = (i as isize + d).rem_euclid(n as isize) as usize;
                if items[i].cmd.is_some() {
                    break;
                }
            }
            i
        };
        match ev.keystroke.key.as_str() {
            "escape" => self.menu = None,
            "down" => menu.selected = step(menu.selected, 1, &menu.items),
            "up" => menu.selected = step(menu.selected, -1, &menu.items),
            "enter" => {
                if let Some(cmd) = menu.items[menu.selected].cmd {
                    self.run(cmd, window, cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn prompt_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let key = k.key.as_str();
        if key == "escape" {
            self.prompt = None;
            cx.notify();
            return;
        }
        match self.prompt.as_mut() {
            Some(Prompt::SaveAs { text, error }) => match key {
                "enter" => match wsdoc::valid_name(text) {
                    Ok(name) => {
                        save_named(&name, cx);
                        self.prompt = None;
                    }
                    Err(e) => *error = Some(e),
                },
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
                items,
                selected,
                error,
            }) => match key {
                "down" => *selected = (*selected + 1) % items.len().max(1),
                "up" => *selected = (*selected + items.len().max(1) - 1) % items.len().max(1),
                "enter" => {
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
                _ => {}
            },
            None => {}
        }
        cx.notify();
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
            self.run(cmd, window, cx);
            cx.stop_propagation();
        }
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = &self.split_drag {
            if ev.pressed_button != Some(MouseButton::Left) {
                self.split_drag = None;
                return;
            }
            let r = d.ratio_at(
                f32::from(ev.position.x),
                f32::from(ev.position.y) - TAB_BAR_H,
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
        let ws = cx.global::<Workspace>();
        let (name, sync, report) = (ws.name.clone(), ws.crosshair_sync, ws.report.clone());
        let is_first = ws.windows.first().map(|w| w.window_id());
        let mut bar = div()
            .h(px(TAB_BAR_H))
            .flex_none()
            .flex()
            .flex_row()
            .items_end()
            .pl(px(TAB_X0))
            .bg(t.panel)
            .border_b_1()
            .border_color(t.axis_line)
            .text_size(px(12.0));
        for (i, tab) in self.tabs.iter().enumerate() {
            let active = i == self.active;
            let title = tab.title(cx);
            bar = bar.child(
                div()
                    .id(("tab", i))
                    .w(px(TAB_W))
                    .h_full()
                    .flex()
                    .items_center()
                    .px_3()
                    .overflow_hidden()
                    .border_b_2()
                    .border_color(if active {
                        t.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(if active { t.text_main } else { t.text_dim })
                    .when(active, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .child(title)
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
                    ),
            );
        }
        let right = div()
            .flex_1()
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .gap_3()
            .px_3()
            .text_color(t.text_dim)
            .when_some(report.filter(|_| is_first.is_some()), |d, r| {
                d.child(
                    div()
                        .id("report")
                        .text_color(t.text_main)
                        .overflow_hidden()
                        .child(format!("{r}  (click to dismiss)"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| {
                                cx.global_mut::<Workspace>().report = None;
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        ),
                )
            })
            .when(sync, |d| d.child("crosshair sync"))
            .child(div().text_color(t.text_main).child(name));
        bar.child(right)
    }

    fn panes(&self, area: Rect, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = self.theme;
        let tab = self.tab();
        let mut out = Vec::new();
        let leaves = tab.layout.leaves();
        for p in tab.layout.place(area) {
            let id = *leaves[p.index];
            let Some(h) = tab.handle(id) else { continue };
            let r = p.rect;
            out.push(
                div()
                    .absolute()
                    .left(px(r.x))
                    .top(px(r.y))
                    .w(px(r.w))
                    .h(px(r.h))
                    .overflow_hidden()
                    .capture_any_mouse_down(cx.listener(
                        move |this, _: &MouseDownEvent, window, cx| {
                            this.menu = None;
                            this.focus_pane(id, window, cx);
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
                    .into_any_element(),
            );
        }
        // Separators and their grab handles (on top of the panes).
        for (i, d) in tab.layout.dividers(area).into_iter().enumerate() {
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
        // Focus: always visible on the focused pane (docs/design.md: violet is focus).
        if let Some(i) = tab.layout.index_of(&tab.focused)
            && let Some(p) = tab.layout.place(area).get(i)
        {
            let r = p.rect;
            let (color, width) = if self.window_active {
                (t.accent, px(2.0))
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
        let t = self.theme;
        let vs = window.viewport_size();
        let h = menu
            .items
            .iter()
            .map(|i| if i.cmd.is_some() { MENU_ROW_H } else { 9.0 })
            .sum::<f32>()
            + 8.0;
        let x = f32::from(menu.at.x)
            .min(f32::from(vs.width) - MENU_W - 4.0)
            .max(0.0);
        let y = f32::from(menu.at.y)
            .min(f32::from(vs.height) - h - 4.0)
            .max(0.0);
        let mut list = div()
            .id("menu")
            .occlude()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(MENU_W))
            .py_1()
            .flex()
            .flex_col()
            .bg(t.panel)
            .border_1()
            .border_color(t.axis_line)
            .rounded_md()
            .text_size(px(12.0));
        for (i, item) in menu.items.iter().enumerate() {
            let Some(cmd) = item.cmd else {
                list = list.child(div().my_1().h(px(1.0)).bg(t.axis_line));
                continue;
            };
            let selected = i == menu.selected;
            list = list.child(
                div()
                    .id(("menu-item", i))
                    .h(px(MENU_ROW_H))
                    .px_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .border_l_2()
                    .border_color(if selected {
                        t.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .when(selected, |d| d.bg(t.accent.opacity(0.16)))
                    .child(
                        div()
                            .w(px(12.0))
                            .text_color(t.text_main)
                            .child(if item.checked { "✓" } else { "" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_color(t.text_main)
                            .child(item.label.clone()),
                    )
                    .child(div().text_color(t.text_dim).child(item.hint))
                    .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                        if let Some(m) = this.menu.as_mut()
                            && m.selected != i
                        {
                            m.selected = i;
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.run(cmd, window, cx);
                        }),
                    ),
            );
        }
        Some(
            div()
                .absolute()
                .size_full()
                .child(
                    // Clicking anywhere else closes the menu.
                    div()
                        .id("menu-backdrop")
                        .absolute()
                        .size_full()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                                this.menu = None;
                                cx.notify();
                            }),
                        ),
                )
                .child(list)
                .into_any_element(),
        )
    }

    fn prompt_overlay(&self) -> Option<AnyElement> {
        let p = self.prompt.as_ref()?;
        let t = self.theme;
        let (title, body, error, hint) = match p {
            Prompt::SaveAs { text, error } => (
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
                    .child(div().w(px(2.0)).h(px(15.0)).bg(t.accent)),
                error.clone(),
                "Enter save   Esc cancel",
            ),
            Prompt::Open {
                items,
                selected,
                error,
            } => {
                let mut list = div().flex().flex_col().py_1();
                for (i, name) in items.iter().enumerate() {
                    let sel = i == *selected;
                    let builtin = wsdoc::is_builtin(name);
                    list = list.child(
                        div()
                            .px_2()
                            .py_1()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .border_l_2()
                            .border_color(if sel {
                                t.accent
                            } else {
                                gpui::transparent_black()
                            })
                            .when(sel, |d| d.bg(t.accent.opacity(0.16)))
                            .child(div().text_color(t.text_main).child(name.clone()))
                            .child(div().text_color(t.text_dim).child(if builtin {
                                "built-in"
                            } else {
                                "saved"
                            })),
                    );
                }
                (
                    "Open workspace",
                    list,
                    error.clone(),
                    "↑↓ select   Enter open   Esc cancel",
                )
            }
        };
        Some(
            div()
                .absolute()
                .top(px(TAB_BAR_H + 24.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("prompt")
                        .occlude()
                        .w(px(420.0))
                        .flex()
                        .flex_col()
                        .bg(t.panel)
                        .border_1()
                        .border_color(t.axis_line)
                        .rounded_md()
                        .text_size(px(13.0))
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .border_b_1()
                                .border_color(t.axis_line)
                                .text_color(t.text_dim)
                                .child(title),
                        )
                        .child(body)
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .justify_between()
                                .px_3()
                                .py_1()
                                .border_t_1()
                                .border_color(t.axis_line)
                                .text_size(px(11.0))
                                .text_color(t.text_dim)
                                .child(hint)
                                .when_some(error, |d, e| {
                                    d.child(div().text_color(t.text_main).child(e))
                                }),
                        ),
                )
                .into_any_element(),
        )
    }

    fn drag_ghost(&self) -> Option<AnyElement> {
        let d = self.tab_drag.as_ref().filter(|d| d.active)?;
        let t = self.theme;
        Some(
            div()
                .absolute()
                .left(d.now.x - px(TAB_W * 0.5))
                .top(d.now.y - px(10.0))
                .w(px(TAB_W))
                .h(px(TAB_BAR_H - 6.0))
                .px_3()
                .flex()
                .items_center()
                .bg(t.panel.opacity(0.9))
                .border_1()
                .border_color(t.accent)
                .rounded_md()
                .text_size(px(12.0))
                .text_color(t.text_main)
                .child("move tab")
                .into_any_element(),
        )
    }
}

impl Render for WindowView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        self.screen = screen_rect(window);
        let area = Self::content_area(window);
        let hints = match self.tabs.get(self.active).and_then(|t| t.focused_handle()) {
            Some(PaneHandle::Dom(_)) => {
                "DOM: wheel scroll, C centre, G / Shift+G grouping, Ctrl+K symbol"
            }
            Some(PaneHandle::Tape(_)) => "Time & sales: type a symbol or Ctrl+K: switch",
            _ => chart_view::HINTS_SHORT,
        };
        let content = if self.tabs.is_empty() {
            Vec::new()
        } else {
            self.panes(area, cx)
        };
        let menu = self.menu_overlay(window, cx);
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
            .child(self.tab_bar(cx))
            .child(
                div()
                    .relative()
                    .flex_none()
                    .w(px(area.w))
                    .h(px(area.h))
                    .overflow_hidden()
                    .children(content),
            )
            .child(
                div()
                    .h(px(HINT_H))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px_3()
                    .gap_4()
                    .overflow_hidden()
                    .text_size(px(11.0))
                    .text_color(t.text_dim)
                    .bg(t.panel)
                    .border_t_1()
                    .border_color(t.axis_line)
                    .child(hints)
                    .child(commands::workspace_hints()),
            )
            .children(menu)
            .children(self.prompt_overlay())
            .children(self.drag_ghost())
    }
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
fn placement(window: &Window, cx: &App) -> Option<Placement> {
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
fn window_options(
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

/// Take a workspace's name and settings (not its windows).
pub fn set_meta(doc: &WorkspaceDoc, cx: &mut App) {
    let ws = cx.global_mut::<Workspace>();
    ws.name = doc.name.clone();
    ws.crosshair_sync = doc.crosshair_sync;
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
    }
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
}

/// App-wide hooks for the interactive app: forget closed windows (also those closed by
/// moving their last tab away), quit when none is left, and save on quit.
pub fn install(cx: &mut App) {
    cx.on_window_closed(|cx, id| {
        if cx.has_global::<Workspace>() {
            cx.global_mut::<Workspace>()
                .windows
                .retain(|w| w.window_id() != id);
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
        async move {
            if let Some(w) = writer {
                w.flush(Duration::from_secs(3));
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
    let (name, sync, windows) = (ws.name.clone(), ws.crosshair_sync, ws.windows.clone());
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
        windows: docs,
    })
}

/// The whole workspace as a document (from outside any window update).
pub fn snapshot(cx: &mut App) -> WorkspaceDoc {
    let ws = cx.global::<Workspace>();
    let (name, sync, windows) = (ws.name.clone(), ws.crosshair_sync, ws.windows.clone());
    let windows = windows
        .into_iter()
        .filter_map(|w| w.update(cx, |wv, window, cx| wv.doc(window, cx)).ok())
        .collect();
    WorkspaceDoc {
        name,
        crosshair_sync: sync,
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

/// Crosshair sync: chart `from` has `t` under its pointer.
fn forward_crosshair(from: PaneId, t: Option<UtcMillis>, cx: &mut App) {
    if !cx.global::<Workspace>().crosshair_sync {
        return;
    }
    let windows = cx.global::<Workspace>().windows.clone();
    cx.defer(move |cx| {
        for w in windows {
            let charts: Vec<PaneHandle> = w
                .read(cx)
                .map(|wv| {
                    wv.tabs
                        .iter()
                        .flat_map(|t| t.panes.iter())
                        .filter(|(id, s)| **id != from && s.handle.kind() == PaneKind::Chart)
                        .map(|(_, s)| s.handle.clone())
                        .collect()
                })
                .unwrap_or_default();
            for h in charts {
                h.set_synced_time(t, cx);
            }
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
                h.set_synced_time(None, cx);
            }
        }
    });
}
