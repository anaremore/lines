//! The chart pane: header + candlestick canvas with pan/zoom/scaling and a crosshair.
//!
//! A pane (`crate::pane`): given a [`PaneFeed`] and a theme, it owns its view state and knows
//! nothing about other panes or the layout. Switching symbol emits [`FeedChanged`]; the
//! workspace forwards it to linked panes.
//!
//! Frame flow (decision 0001): a UI-thread timer compares the store's atomic revision and
//! calls `cx.notify()` only when data changed (so 5k msgs/s never means 5k redraws). In
//! paint we `try_lock` the store, update the viewport and rebuild the reusable `Scene` (no
//! heap allocation), release the lock, then emit quads. Text labels are shaped once and
//! cached. Coordinates handed to GPUI are relative to the plot origin before the `f32` cast.
//!
//! Crosshair: input handlers only record the pointer position (plot-local logical px). Paint
//! hit-tests it against the viewport *after* this frame's pan/zoom/data update
//! (`lines_core::geometry::crosshair`), so the crosshair can never lag or fight the view.
//!
//! Input map: drag = pan; Ctrl+drag (plot) or drag on an axis = scale (`ScaleDrag`);
//! wheel = time zoom (pinned to the right edge while the newest bar is visible);
//! Ctrl+wheel = price zoom; Shift+wheel or horizontal scroll = pan through time;
//! double-click plot / Home = newest bar + auto-scale; double-click price axis = auto-scale.
//! While any drag is active the viewport is `held`, so arriving bars can't move the axes
//! under the pointer.
//!
//! Symbol picker (roadmap 4b): any letter or Ctrl+K opens it over the chart. Its logic lives in
//! `crate::picker` (pure, tested) and its UI in `crate::picker_view`. Picking a symbol
//! subscribes to the same source with a new target through the shared registry; the old
//! subscription is dropped, and if no other pane watches it the registry stops that feed on a
//! background thread (decision 0001: never on the UI thread). While closed it costs nothing
//! per frame.
//!
//! Crosshair time sync (decision 0016, opt-in per workspace): after each paint the chart emits
//! [`CrosshairTime`] when the time under its pointer changed; the workspace hands it to the
//! other charts, which mark that time with a vertical line and a time tag (no price: the
//! instruments differ). Mapping a time back to a bar is pure (`geometry::time_slot`).
//!
//! Layers (seam for the volume profile and other studies): a [`ChartLayer`] is prepared under
//! the store lock with this frame's data and viewport, then painted inside the plot after the
//! candles. Both steps must not allocate (docs/design.md §2).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    App, Bounds, ContentMask, Context, EventEmitter, FocusHandle, Font, Hsla, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, Render, ScrollEvent, ShapedLine, SharedString, Styled, TextAlign,
    TextRun, Window, canvas, div, fill, point, prelude::*, px, size,
};
use lines_core::geometry::{
    Crosshair, Span, crosshair, label_clear_of_tags, slot_x, snap, time_slot, write_ohlcv,
};
use lines_core::viewport::{MAX_BAR_PX, MIN_BAR_PX, ScaleDrag, ScaleMode};
use lines_core::{Bar, FixedStr, Price, Product, Scene, UtcMillis, Viewport};
use lines_feed::ChartData;
use lines_feed::catalog::Target;
use lines_feed::store::{ChartStore, FeedStatus};

use crate::feeds::{Feeds, PaneFeed};
use crate::levels_layer::{LevelsConfig, LevelsLayer};
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::picker_view::{PickerKey, PickerUi};
use crate::profile::{ProfileConfig, VolumeProfileLayer};
use crate::study_menu::{
    Item, MenuCmd, MenuInput, SourceInfo, StudyAction, StudyMenu, StudyRef, StudySet, anchor_at,
    save_template,
};
use crate::theme::Theme;
use crate::wsdoc::{ChartState, PaneDoc, StudyVisibility};

const AXIS_W: f32 = 78.0;
const AXIS_H: f32 = 24.0;
const LABEL_PX: f32 = 12.0;
const MAX_CACHED_LABELS: usize = 4096;
/// Height of the last-price and crosshair price tags on the price axis.
const TAG_H: f32 = LABEL_PX * 1.5;
/// Shown in the price-axis corner while price auto-scale is off.
const MANUAL_LABEL: &str = "manual";

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
enum LabelKind {
    Price,
    Time,
    LastPrice,
    CrossPrice,
    CrossTime,
    Indicator,
}

/// Where a pointer event landed.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Region {
    Plot,
    /// The price axis column, including the corner below it.
    PriceAxis,
    TimeAxis,
}

/// A study drawn over the plot (e.g. the volume profile). Implementations keep their own
/// buffers; neither method may allocate in steady state.
pub trait ChartLayer: 'static {
    /// Under the store lock, after this frame's viewport update: copy what `paint` needs.
    fn prepare(&mut self, data: &ChartData, vp: &Viewport);
    /// Inside the plot's content mask, after the candles. `plot` is the plot's bounds; the
    /// viewport maps prices/bars to plot-local coordinates.
    fn paint(
        &mut self,
        plot: Bounds<Pixels>,
        vp: &Viewport,
        theme: &Theme,
        window: &mut Window,
        cx: &mut App,
    );
}

/// Deterministic input for screenshot mode (decision 0009), standing in for the mouse.
#[derive(Clone, Debug, Default)]
pub struct Fixture {
    /// Fixed pointer position, plot-local logical px.
    pub cursor: Option<(f32, f32)>,
    /// Zoom the auto-fitted price range by this factor around the plot's centre, as Ctrl+wheel
    /// would (turns auto-scale off).
    pub price_zoom: Option<f64>,
    /// Render with the symbol picker open on this query.
    pub picker: Option<String>,
    /// Indicators to start with (screenshots, `--vwap`).
    pub indicators: Vec<lines_indicators::IndicatorConfig>,
    /// Initial volume profile settings (CLI). A profile saved with the pane in the workspace
    /// file (`ChartState::profile`) takes precedence.
    pub profile: ProfileConfig,
    /// Study labels / studies hidden from the start (screenshots of a clean chart).
    pub visibility: StudyVisibility,
    /// Studies to start with, as a template or a new chart's defaults would give them
    /// (screenshots: `--study-set FILE`).
    pub studies: Option<StudySet>,
    /// Open the studies menu (screenshots): `open`, or `edit:vwap:N`, `edit:profile`,
    /// `edit:levels:ID` to open that study's editor.
    pub studies_menu: Option<String>,
}

/// Which part of a studies-menu row a click hit.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MenuPart {
    /// The label or the gear: open the editor; a control or catalog entry: run it.
    Primary,
    /// The eye: hide or show.
    Eye,
    /// Remove a study, delete a template.
    Delete,
}

enum Drag {
    /// Last pointer position.
    Pan(Point<Pixels>),
    Scale(ScaleDrag),
}

/// Performance stats for the header (off by default: the "Performance stats" command).
struct Rates {
    at: Instant,
    updates: u64,
    msgs: u64,
    paints: u64,
    updates_per_sec: f64,
    /// Feed messages (wire frames) per second: depth batches included, so this is far above
    /// the paint rate on a busy book.
    msgs_per_sec: f64,
    /// Frames this chart actually painted per second.
    paints_per_sec: f64,
}

/// The time under this chart's crosshair changed (`None`: it left the plot). The workspace
/// forwards it to the other charts when crosshair sync is on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CrosshairTime(pub Option<UtcMillis>);

pub struct ChartView {
    feed: PaneFeed,
    common: PaneCommon,
    /// The volume profile, painted first (under other studies). Kept typed, not in `layers`,
    /// so its settings can be read and changed.
    profile: VolumeProfileLayer,
    /// Studies over the plot, painted in order.
    layers: Vec<Box<dyn ChartLayer>>,
    /// Indicators (VWAP …) from the shared engine (`crate::studies`), saved with the pane.
    studies: crate::studies::Studies,
    /// `studies.revision()` last drawn.
    seen_studies: u64,
    /// The studies menu (Ctrl+I, "Studies…"), while open.
    study_menu: Option<StudyMenu>,
    /// Waiting for a click on a bar to anchor a VWAP: `Some(None)` adds a new one.
    anchoring: Option<Option<usize>>,
    /// External levels (decision 0022), saved with the pane.
    levels: LevelsLayer,
    /// `levels.revision()` last drawn.
    seen_levels: u64,
    /// Study legends/tags or all studies hidden (saved with the pane). Hidden studies keep
    /// their configuration and engine nodes, so showing them again is exact and immediate,
    /// but cost no prepare or paint work while hidden.
    visibility: StudyVisibility,
    picker: PickerUi,
    store: Arc<ChartStore>,
    theme: Theme,
    /// Screenshot mode: no wall-clock-dependent text (rates, paint time) is shown.
    frozen: bool,
    seen_revision: u64,
    vp: Viewport,
    scene: Scene,
    instrument: Product,
    last_trade: Option<Price>,
    series_len: usize,
    series_trimmed: u64,
    plot: Bounds<Pixels>,
    /// The whole canvas: plot plus axes.
    chart: Bounds<Pixels>,
    drag: Option<Drag>,
    focus: FocusHandle,
    /// Another chart's crosshair time (crosshair sync), drawn while this one has none.
    synced_time: Option<UtcMillis>,
    /// This frame's sync marker: plot-local x of the synced time's bar, and the time.
    sync_marker: Option<(f32, UtcMillis)>,
    /// The time this chart last reported under its own crosshair.
    reported_time: Option<UtcMillis>,
    /// Screenshot fixture: price zoom to apply once, after the first auto-fit.
    pending_price_zoom: Option<f64>,
    /// Pointer position in plot-local logical px, while it is over the plot.
    hover: Option<(f32, f32)>,
    /// This frame's crosshair (from `hover`), and a copy of the bar under it.
    cross: Option<Crosshair>,
    cross_bar: Option<Bar>,
    period_ms: i64,
    /// OHLCV readout, shaped once per distinct bar value.
    readout: Option<(Bar, ShapedLine)>,
    labels: HashMap<(LabelKind, i64), ShapedLine>,
    font: Option<Font>,
    rates: Rates,
    paint_time_ewma_us: f64,
    /// Show `rates` in the header (workspace preference, read at render).
    perf_stats: bool,
    /// The rates changed since the last paint (redraw once a second while shown).
    rates_changed: bool,
    /// The chart's account (chart trading is roadmap 8; Flatten/Cancel All hotkeys work now).
    trade: crate::trade_bar::PaneTrade,
}

impl crate::trade_bar::TradePane for ChartView {
    fn trade(&mut self) -> &mut crate::trade_bar::PaneTrade {
        &mut self.trade
    }
    fn trade_ref(&self) -> &crate::trade_bar::PaneTrade {
        &self.trade
    }
    fn trade_store(&self) -> &Arc<ChartStore> {
        &self.store
    }
    fn trade_unavailable(&self) -> bool {
        self.feed.unavailable.is_some()
    }
}

impl ChartView {
    /// Restore the pane's trading choices (its account).
    pub fn restore_trade(
        &mut self,
        doc: Option<&crate::trade_bar::TradeDoc>,
        cx: &mut Context<Self>,
    ) {
        self.trade = crate::trade_bar::PaneTrade::new(doc, cx);
    }

    pub fn new(
        feed: PaneFeed,
        theme: Theme,
        frozen: bool,
        fixture: Fixture,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = feed.store().clone();
        let instrument = store.read().product.clone();
        let picker = PickerUi::new(fixture.picker.as_deref(), cx);
        // Poll the lock-free revision counter at ~120 Hz; redraw only on change. Reads the
        // view's current store, which changes when the picker switches symbols.
        pane::watch_store(cx);
        let mut studies = crate::studies::Studies::new(fixture.indicators.clone());
        studies.attach(feed_info(&store));
        let mut levels = LevelsLayer::new(LevelsConfig::default());
        levels.attach(&store);
        let (fixture_studies, fixture_menu) =
            (fixture.studies.clone(), fixture.studies_menu.clone());
        if feed.sub.is_some()
            && let Some(t) = crate::trading::Trading::try_global(cx)
        {
            t.follow(&store);
        }
        let trade = crate::trade_bar::PaneTrade::new(None, cx);

        let mut view = ChartView {
            study_menu: None,
            anchoring: None,
            feed,
            common: PaneCommon::default(),
            profile: VolumeProfileLayer::new(fixture.profile),
            layers: Vec::new(),
            studies,
            seen_studies: u64::MAX,
            levels,
            seen_levels: u64::MAX,
            visibility: fixture.visibility,
            picker,
            store,
            theme,
            frozen,
            seen_revision: u64::MAX,
            vp: Viewport::default(),
            scene: Scene::with_capacity(Scene::candles_needed(4096.0)),
            instrument,
            last_trade: None,
            series_len: 0,
            series_trimmed: 0,
            plot: Bounds::default(),
            chart: Bounds::default(),
            drag: None,
            focus: cx.focus_handle(),
            synced_time: None,
            sync_marker: None,
            reported_time: None,
            hover: fixture.cursor,
            pending_price_zoom: fixture.price_zoom,
            cross: None,
            cross_bar: None,
            period_ms: 60_000,
            readout: None,
            labels: HashMap::with_capacity(MAX_CACHED_LABELS),
            font: None,
            rates: Rates {
                at: Instant::now(),
                updates: 0,
                msgs: 0,
                paints: 0,
                updates_per_sec: 0.0,
                msgs_per_sec: 0.0,
                paints_per_sec: 0.0,
            },
            paint_time_ewma_us: 0.0,
            perf_stats: false,
            rates_changed: false,
            trade,
        };
        if let Some(set) = fixture_studies {
            view.set_study_set(set);
        }
        if let Some(spec) = fixture_menu {
            view.open_menu_at(&spec, cx);
        }
        view
    }

    // ---- studies menu (crate::study_menu) ---------------------------------------------------

    /// Every study on this chart, as one value (templates, defaults).
    pub fn study_set(&self) -> StudySet {
        StudySet {
            indicators: self.studies.configs(),
            profile: self.profile.config(),
            levels: self.levels.config.clone(),
        }
    }

    /// Replace this chart's studies (a template, the defaults, a menu change).
    pub fn set_study_set(&mut self, s: StudySet) {
        if s.indicators != self.studies.configs() {
            self.set_indicator_configs(s.indicators);
        }
        self.profile.set_config(s.profile);
        self.levels.config = s.levels;
        self.seen_levels = u64::MAX;
        self.seen_studies = u64::MAX;
    }

    /// A new chart (not restored from a workspace file): the saved defaults, if any.
    pub fn apply_new_chart_defaults(&mut self, cx: &mut Context<Self>) {
        if let Some(set) = crate::picker_view::settings(cx).chart_defaults.clone() {
            self.set_study_set(set);
        }
    }

    fn menu_items(&self, menu: &StudyMenu, cx: &App) -> Vec<Item> {
        let set = self.study_set();
        let sources: Vec<SourceInfo> = self
            .levels
            .sources()
            .into_iter()
            .map(|(id, label, kinds)| SourceInfo {
                id,
                label: label.to_owned(),
                kinds,
            })
            .collect();
        let templates: Vec<String> = crate::picker_view::settings(cx)
            .study_templates
            .iter()
            .map(|t| t.name.clone())
            .collect();
        let has_rth = lines_feed::catalog::sessions(self.instrument.id).is_none_or(|c| c.has_rth());
        menu.items(&MenuInput {
            set: &set,
            sources: &sources,
            templates: &templates,
            has_rth,
        })
    }

    /// Open or close the studies menu (command `Studies`: Ctrl+I, "Studies…", the palette).
    pub fn toggle_studies_menu(&mut self, cx: &mut Context<Self>) {
        self.picker.close();
        self.anchoring = None;
        self.study_menu = match self.study_menu.take() {
            Some(_) => None,
            None => {
                let mut m = StudyMenu::default();
                let items = self.menu_items(&m, cx);
                m.clamp(&items);
                Some(m)
            }
        };
        cx.notify();
    }

    /// Screenshot fixture: the menu open, optionally on a study's editor.
    fn open_menu_at(&mut self, spec: &str, cx: &mut Context<Self>) {
        let editing = match spec.split(':').collect::<Vec<_>>().as_slice() {
            ["edit", "vwap", i] => i.parse().ok().map(StudyRef::Vwap),
            ["edit", "profile"] => Some(StudyRef::Profile),
            ["edit", "levels", id] => Some(StudyRef::Levels((*id).to_owned())),
            _ => None,
        };
        let mut m = StudyMenu {
            editing,
            ..StudyMenu::default()
        };
        let items = self.menu_items(&m, cx);
        m.clamp(&items);
        self.study_menu = Some(m);
    }

    /// A click on menu item `i`.
    pub fn menu_click(&mut self, i: usize, part: MenuPart, cx: &mut Context<Self>) {
        let Some(mut m) = self.study_menu.take() else {
            return;
        };
        let items = self.menu_items(&m, cx);
        let cmd = match part {
            MenuPart::Primary => m.activate(&items, i),
            MenuPart::Eye => m.eye(&items, i),
            MenuPart::Delete => m.delete(&items, i),
        };
        self.study_menu = Some(m);
        self.run_menu(cmd, cx);
    }

    fn menu_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(mut m) = self.study_menu.take() else {
            return;
        };
        let k = &ev.keystroke;
        let mods = k.modifiers;
        let text = k
            .key_char
            .as_deref()
            .filter(|_| !mods.control && !mods.alt && !mods.platform);
        let items = self.menu_items(&m, cx);
        let query = m.query.clone();
        let cmd = m.key(&items, &k.key, text);
        if m.query != query {
            let items = self.menu_items(&m, cx);
            m.follow_query(&items);
        }
        self.study_menu = Some(m);
        self.run_menu(cmd, cx);
    }

    /// Run a menu command: the menu's own part, then the chart's (studies, anchoring,
    /// templates and defaults in the app settings).
    fn run_menu(&mut self, cmd: Option<MenuCmd>, cx: &mut Context<Self>) {
        let cmd = match (cmd, self.study_menu.as_mut()) {
            (Some(c), Some(m)) => m.local(c),
            (c, _) => c,
        };
        match cmd {
            Some(MenuCmd::Set(a)) => {
                let mut s = self.study_set();
                s.apply(&a);
                self.set_study_set(s);
            }
            Some(MenuCmd::BeginAnchor(i)) => {
                self.study_menu = None;
                self.anchoring = Some(i);
            }
            Some(MenuCmd::ApplyTemplate(name)) => {
                let t = crate::picker_view::settings(cx)
                    .study_templates
                    .iter()
                    .find(|t| t.name == name)
                    .map(|t| t.set.clone());
                if let Some(set) = t {
                    let mut s = self.study_set();
                    s.apply(&StudyAction::Replace(set));
                    self.set_study_set(s);
                }
            }
            Some(MenuCmd::DeleteTemplate(name)) => {
                crate::picker_view::update_settings(cx, |s| {
                    s.study_templates.retain(|t| t.name != name)
                });
            }
            Some(MenuCmd::SaveTemplate(name)) => {
                let set = self.study_set();
                crate::picker_view::update_settings(cx, |s| {
                    save_template(&mut s.study_templates, &name, set)
                });
            }
            Some(MenuCmd::SaveDefault) => {
                let set = self.study_set();
                crate::picker_view::update_settings(cx, |s| s.chart_defaults = Some(set));
            }
            Some(MenuCmd::Close) => self.study_menu = None,
            Some(MenuCmd::Edit(_) | MenuCmd::StartSaveTemplate) | None => {}
        }
        if let Some(mut m) = self.study_menu.take() {
            let items = self.menu_items(&m, cx);
            m.clamp(&items);
            self.study_menu = Some(m);
        }
        cx.notify();
    }

    /// Add a study layer (volume profile, …), painted above earlier layers.
    #[allow(dead_code)] // the seam for the volume profile milestone
    pub fn add_layer(&mut self, layer: Box<dyn ChartLayer>) {
        self.layers.push(layer);
    }

    /// Once a second while the performance stats are shown; `true` if they changed.
    fn update_rates(&mut self) -> bool {
        let dt = self.rates.at.elapsed().as_secs_f64();
        if dt < 1.0 {
            return false;
        }
        let Some(d) = self.store.try_read() else {
            return false;
        };
        let (u, m, p) = (d.stats.updates_in, d.stats.frames_in, self.common.paints);
        drop(d);
        let r = &mut self.rates;
        let per_sec = |now: u64, then: u64| (now - then.min(now)) as f64 / dt;
        let next = (
            per_sec(u, r.updates),
            per_sec(m, r.msgs),
            per_sec(p, r.paints),
        );
        let changed = next != (r.updates_per_sec, r.msgs_per_sec, r.paints_per_sec);
        (r.updates_per_sec, r.msgs_per_sec, r.paints_per_sec) = next;
        (r.updates, r.msgs, r.paints) = (u, m, p);
        r.at = Instant::now();
        changed
    }

    // ---- input ---------------------------------------------------------------------

    fn region(&self, pos: Point<Pixels>) -> Option<Region> {
        if !self.chart.contains(&pos) {
            None
        } else if self.plot.contains(&pos) {
            Some(Region::Plot)
        } else if pos.x >= self.plot.right() {
            Some(Region::PriceAxis)
        } else {
            Some(Region::TimeAxis)
        }
    }

    /// Plot-local logical px (may lie outside the plot, e.g. on an axis).
    fn local(&self, pos: Point<Pixels>) -> (f64, f64) {
        let l = pos - self.plot.origin;
        (f64::from(l.x), f64::from(l.y))
    }

    fn start_drag(&mut self, drag: Drag) {
        self.drag = Some(drag);
        self.vp.set_held(true, self.series_len);
    }

    fn end_drag(&mut self) {
        self.drag = None;
        self.vp.set_held(false, self.series_len);
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(region) = self.region(ev.position) else {
            return;
        };
        self.focus.focus(window, cx);
        if self.picker.close() {
            // Clicking the chart dismisses the picker (rows handle their own clicks).
            cx.notify();
            return;
        }
        self.end_drag();
        let (x, y) = self.local(ev.position);
        if let Some(index) = self.anchoring {
            // Anchored VWAP: this click picks the bar (off the bars: keep waiting).
            if region == Region::Plot
                && let Some(from) = self
                    .store
                    .try_read()
                    .and_then(|d| anchor_at(d.series.bars(), &self.vp, x))
            {
                self.anchoring = None;
                self.run_menu(
                    Some(MenuCmd::Set(StudyAction::AnchorAt { index, from })),
                    cx,
                );
            }
            return;
        }
        if ev.click_count >= 2 {
            match region {
                Region::Plot => self.vp.reset(self.series_len),
                Region::PriceAxis => self.vp.restore_auto_price(),
                Region::TimeAxis => return,
            }
            cx.notify();
            return;
        }
        let drag = match region {
            Region::Plot if ev.modifiers.control => {
                Drag::Scale(ScaleDrag::new(&self.vp, ScaleMode::Free, x, y))
            }
            Region::Plot => Drag::Pan(ev.position),
            Region::PriceAxis => Drag::Scale(ScaleDrag::new(&self.vp, ScaleMode::Price, x, y)),
            Region::TimeAxis => Drag::Scale(ScaleDrag::new(&self.vp, ScaleMode::Time, x, y)),
        };
        self.start_drag(drag);
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let m = k.modifiers;
        if !self.picker.is_open() && pane::unavailable_key(self, ev, cx) {
            return;
        }
        if self.picker.is_open() {
            let today = pane::today(self.frozen, &self.store);
            let outcome = self.picker.key(ev, today, &self.feed.availability, cx);
            self.picked(outcome, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.study_menu.is_some() {
            self.menu_key(ev, cx);
            cx.stop_propagation();
            return;
        }
        if self.anchoring.is_some() && k.key == "escape" {
            self.anchoring = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if m.control && !m.alt && k.key == "k" {
            self.open_picker("", cx);
        } else if k.key == "home" {
            self.vp.reset(self.series_len);
        } else if m.alt && !m.control && !m.platform && {
            let mut c = self.profile.config();
            let changed = c.on_alt_key(&k.key);
            if changed {
                self.profile.set_config(c);
            }
            changed
        } {
            // Volume profile keys (`crate::profile`).
        } else if let Some(letter) = k.key_char.as_deref().filter(|c| {
            !m.control
                && !m.alt
                && !m.platform
                && c.len() == 1
                && c.as_bytes()[0].is_ascii_alphabetic()
        }) {
            // Type a symbol straight onto the chart: the picker opens with that letter.
            self.open_picker(letter, cx);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn open_picker(&mut self, query: &str, cx: &mut Context<Self>) {
        self.end_drag();
        self.picker.open(query, cx);
    }

    fn picker_click(&mut self, row: usize, cx: &mut Context<Self>) {
        let today = pane::today(self.frozen, &self.store);
        let outcome = self.picker.click(row, today, &self.feed.availability);
        self.picked(outcome, cx);
    }

    /// A picker confirmed a target: subscribe on the same source, swap it in and tell linked
    /// panes. The old subscription is dropped (the registry stops its feed in the background
    /// if unused).
    fn picked(&mut self, outcome: PickerKey, cx: &mut Context<Self>) {
        let PickerKey::Confirm(target, product) = outcome else {
            return;
        };
        match self.switch_to(target, cx) {
            Ok(()) => self.picker.switched(product, cx),
            Err(e) => self.picker.error = Some(format!("{e:#}")),
        }
    }

    fn switch_to(&mut self, target: Target, cx: &mut Context<Self>) -> anyhow::Result<()> {
        let feed = self.feed.retarget(&Feeds::registry(cx), target)?;
        self.apply_feed(feed.clone());
        cx.emit(FeedChanged(feed));
        Ok(())
    }

    fn apply_feed(&mut self, feed: PaneFeed) {
        self.store = feed.store().clone();
        // The same indicators on the new feed (new shared nodes).
        self.studies.attach(feed_info(&self.store));
        self.levels.attach(&self.store);
        self.seen_levels = u64::MAX;
        self.feed = feed;
        self.instrument = self.store.read().product.clone();
        // A fresh view for the new series; price labels belong to the old product's format.
        // The time zoom is the trader's choice, not the market's: keep it.
        let bar_px = self.vp.bar_px;
        self.vp = Viewport::default();
        self.vp.bar_px = bar_px;
        self.drag = None;
        self.series_len = 0;
        self.series_trimmed = 0;
        self.last_trade = None;
        self.seen_revision = u64::MAX;
        self.cross = None;
        self.cross_bar = None;
        self.readout = None;
        self.labels.clear();
    }

    /// Another chart's crosshair time (crosshair sync); `None` clears it.
    pub fn set_synced_time(&mut self, t: Option<UtcMillis>, cx: &mut Context<Self>) {
        if self.synced_time != t {
            self.synced_time = t;
            cx.notify();
        }
    }

    /// Restore saved view state (time zoom, indicators).
    pub fn restore(&mut self, state: &ChartState) {
        if state.bar_px.is_finite() {
            self.vp.bar_px = state.bar_px.clamp(MIN_BAR_PX, MAX_BAR_PX);
        }
        if state.visibility != StudyVisibility::default() {
            self.visibility = state.visibility;
        }
        if !state.levels.is_default() {
            self.levels.config = state.levels.clone();
        }
        // A saved profile wins; a pane without one keeps its initial (CLI) settings.
        if !state.profile.is_default() {
            self.profile.set_config(state.profile);
        }
        // Saved indicators, then any given on the command line (`--vwap`) not among them.
        let mut configs = state.indicators.clone();
        for c in self.studies.configs() {
            if !configs.contains(&c) {
                configs.push(c);
            }
        }
        self.set_indicator_configs(configs);
    }

    /// Study legends (VWAP …) and level-source rows, top left under the readout, unless the
    /// chart hides its study labels (render time).
    fn legend(&self, t: &Theme) -> Option<gpui::AnyElement> {
        if !self.visibility.draws_labels() {
            return None;
        }
        let studies = self.studies.legend(t, &self.instrument);
        let rows = if self.levels.config.hide_labels {
            Vec::new()
        } else {
            self.levels.legend()
        };
        if studies.is_none() && rows.is_empty() {
            return None;
        }
        let levels = (!rows.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .px(px(8.0))
                .py(px(4.0))
                .bg(t.label_bg)
                .text_size(px(12.0))
                .children(rows.into_iter().map(|r| {
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(div().text_color(t.text_main).child(r.label))
                        .child(
                            div()
                                .text_color(if r.warn { t.text_main } else { t.text_dim })
                                .child(r.text),
                        )
                }))
        });
        Some(
            div()
                .absolute()
                .top(px(30.0))
                .left(px(8.0))
                .flex()
                .flex_col()
                .gap_1()
                .children(studies)
                .children(levels)
                .into_any_element(),
        )
    }

    /// External level sources on this chart, `(id, label, kinds)`, for menus.
    pub fn level_sources(&self) -> Vec<(String, &'static str, Vec<String>)> {
        self.levels.sources()
    }

    pub fn levels_config(&self) -> LevelsConfig {
        self.levels.config.clone()
    }

    /// Change which levels and labels this chart shows (commands `ToggleLevels`, ...).
    pub fn set_levels_config(&mut self, c: LevelsConfig, cx: &mut Context<Self>) {
        if c != self.levels.config {
            self.levels.config = c;
            self.seen_levels = u64::MAX;
            cx.notify();
        }
    }

    /// What this chart draws of its studies.
    pub fn visibility(&self) -> StudyVisibility {
        self.visibility
    }

    /// Hide or show study labels / studies (commands `ToggleStudyLabels`, `ToggleStudies`,
    /// `CleanChart`). Configurations are untouched.
    pub fn set_visibility(&mut self, v: StudyVisibility, cx: &mut Context<Self>) {
        if v != self.visibility {
            self.visibility = v;
            self.seen_studies = u64::MAX; // studies shown again: draw their latest values
            cx.notify();
        }
    }

    /// This chart's indicators, as the workspace saves them.
    pub fn indicator_configs(&self) -> Vec<lines_indicators::IndicatorConfig> {
        self.studies.configs()
    }

    /// Replace this chart's indicators.
    pub fn set_indicator_configs(&mut self, configs: Vec<lines_indicators::IndicatorConfig>) {
        self.studies = crate::studies::Studies::new(configs);
        self.studies.attach(feed_info(&self.store));
        self.seen_studies = u64::MAX;
    }

    /// A click on indicator-menu row `i`.
    /// Track the pointer for the crosshair; it shows only while the pointer is over the plot.
    fn set_hover(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let hover = self.plot.contains(&pos).then(|| {
            let l = pos - self.plot.origin;
            (f32::from(l.x), f32::from(l.y))
        });
        if hover != self.hover {
            self.hover = hover;
            cx.notify();
        }
    }

    fn on_mouse_exit(&mut self, _: &MouseExitEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.hover.take().is_some() {
            cx.notify();
        }
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.set_hover(ev.position, cx);
        if self.drag.is_none() {
            return;
        }
        if ev.pressed_button != Some(MouseButton::Left) {
            self.end_drag();
            return;
        }
        let (x, y) = self.local(ev.position);
        let len = self.series_len;
        let tick = self.instrument.price.tick;
        match &mut self.drag {
            Some(Drag::Pan(from)) => {
                let d = ev.position - *from;
                *from = ev.position;
                self.vp.pan(f64::from(d.x), f64::from(d.y), len);
            }
            Some(Drag::Scale(drag)) => drag.update(&mut self.vp, x, y, len, tick),
            None => return,
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.drag.is_some() {
            self.end_drag();
            cx.notify();
        }
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.set_hover(ev.position, cx);
        if !self.plot.contains(&ev.position) {
            return;
        }
        let d = ev.delta.pixel_delta(window.line_height());
        let (dx, dy) = (f64::from(d.x), f64::from(d.y));
        let (x, y) = self.local(ev.position);
        let len = self.series_len;
        if ev.modifiers.shift || dx.abs() > dy.abs() {
            // Shift+wheel (GPUI on Windows already reports it as delta.x), tilt wheel and
            // trackpad swipes pan through time; the content follows the delta.
            let amount = if dx != 0.0 { dx } else { dy };
            if amount == 0.0 {
                return;
            }
            self.vp.pan(amount, 0.0, len);
        } else if dy == 0.0 {
            return;
        } else {
            // Wheel up (positive delta) zooms in. ~10% per notch.
            let factor = (dy * 0.005).exp();
            if ev.modifiers.control {
                self.vp.zoom_price_at(y, factor, self.instrument.price.tick);
            } else {
                let anchor = self.vp.time_zoom_anchor(x, len);
                self.vp.zoom_at(anchor, factor, len);
            }
        }
        cx.notify();
    }

    // ---- paint ---------------------------------------------------------------------

    fn paint_chart(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let t0 = Instant::now();
        self.common.paints += 1;
        let scale = window.scale_factor();
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let plot_w = (w - AXIS_W).max(10.0);
        let plot_h = (h - AXIS_H).max(10.0);
        self.chart = bounds;
        self.plot = Bounds::new(bounds.origin, size(px(plot_w), px(plot_h)));
        self.vp.set_size(plot_w as f64, plot_h as f64);
        // Only allocates when the window grows wider than ever before.
        self.scene.ensure_capacity(plot_w as f64);

        // Sample the feed. If it holds the lock this instant, redraw the previous scene.
        if let Some(d) = self.store.try_read() {
            let trimmed = d.series.trimmed();
            if trimmed > self.series_trimmed && (!self.vp.follow_live || self.vp.held) {
                // Bars dropped from the front shift indices; keep the view anchored.
                self.vp.right_index -= (trimmed - self.series_trimmed) as f64;
            }
            self.series_trimmed = trimmed;
            self.series_len = d.series.len();
            self.last_trade = d.last_trade;
            let fmt = self.instrument.price;
            let bars = d.series.bars();
            self.vp.on_data(bars, fmt.tick);
            self.period_ms = d.series.period_ms();
            // Only once bars are visible, i.e. auto-scale has fitted a real range (the first
            // layout pass can paint at a placeholder size with nothing on screen).
            if !self.vp.visible_range(bars.len()).is_empty()
                && let Some(f) = self.pending_price_zoom.take()
            {
                self.vp.zoom_price_at(self.vp.height * 0.5, f, fmt.tick);
            }
            self.scene
                .build(bars, &self.vp, &fmt, scale, self.period_ms);
            // Hit-test after the viewport update so the crosshair matches this frame.
            self.cross = self.hover.and_then(|(x, y)| {
                crosshair(
                    bars,
                    &self.vp,
                    fmt.tick,
                    self.period_ms,
                    scale,
                    f64::from(x),
                    f64::from(y),
                )
            });
            self.cross_bar = self.cross.and_then(|c| c.bar).map(|i| bars[i]);
            // Another chart's crosshair time, where this chart has a bar for it.
            self.sync_marker = self.synced_time.and_then(|t| {
                let slot = time_slot(bars, t, self.period_ms)?;
                let x = slot_x(&self.vp, slot as f64, scale);
                (x >= 0.0 && f64::from(x) < self.vp.width).then_some((x, t))
            });
            let max_columns = crate::studies::Studies::max_columns(self.plot.size.width);
            prepare_studies(
                self.visibility,
                &mut self.profile,
                &mut self.layers,
                &mut self.studies,
                &mut self.levels,
                &d,
                &self.vp,
                max_columns,
            );
        }
        if !self.frozen && !window.is_window_hovered() {
            // GPUI on Windows reports leaving the window only through this flag (it redraws
            // on change) and never sends `MouseExitEvent`. Screenshots keep their fixed cursor.
            self.hover = None;
        }
        if self.hover.is_none() {
            // Left the plot while the feed held the lock: still hide immediately.
            self.cross = None;
            self.cross_bar = None;
        }

        if self.font.is_none() {
            self.font = Some(window.text_style().font());
        }
        self.draw(bounds, window, cx);

        // Crosshair sync: report this frame's time under the pointer when it changed.
        let time = self.cross.and_then(|c| c.time);
        if time != self.reported_time {
            self.reported_time = time;
            cx.emit(CrosshairTime(time));
        }

        let us = t0.elapsed().as_secs_f64() * 1e6;
        self.paint_time_ewma_us = if self.paint_time_ewma_us == 0.0 {
            us
        } else {
            self.paint_time_ewma_us * 0.95 + us * 0.05
        };
    }

    fn draw(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let t = self.theme;
        let scale = window.scale_factor();
        let hair = 1.0 / scale; // one device pixel
        let ox = f32::from(bounds.origin.x);
        let oy = f32::from(bounds.origin.y);
        let plot = self.plot;
        let (pw, ph) = (f32::from(plot.size.width), f32::from(plot.size.height));
        let rect = |x: f32, y: f32, w: f32, h: f32| {
            Bounds::new(point(px(ox + x), px(oy + y)), size(px(w), px(h)))
        };
        let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));

        window.paint_quad(fill(bounds, t.bg));
        window.paint_quad(fill(rect(pw, 0.0, bw - pw, bh), t.panel));
        window.paint_quad(fill(rect(0.0, ph, pw, bh - ph), t.panel));

        let fmt = self.instrument.price;
        window.with_content_mask(Some(ContentMask { bounds: plot }), |window| {
            for l in &self.scene.price_lines {
                window.paint_quad(fill(rect(0.0, l.y, pw, hair), t.grid));
            }
            for l in &self.scene.time_lines {
                window.paint_quad(fill(rect(l.x, 0.0, hair, ph), t.grid));
            }
            // Paint order inside the plot: grid, volume profile (behind the candles), candles,
            // `layers`, indicator studies (VWAP lines above), the profile's label, last-price
            // line, crosshair.
            let (studies, labels) = (
                self.visibility.draws_studies(),
                self.visibility.draws_labels(),
            );
            if studies {
                self.profile.paint(plot, &self.vp, &t, window, cx);
                self.levels.paint_back(plot, window);
            }
            for c in &self.scene.candles {
                let color = if c.up { t.up } else { t.down };
                let (bx, bw) = (c.x - c.half_width, c.half_width * 2.0);
                let bh = c.body_bottom - c.body_top;
                // Direction is shown by shape as well as colour (docs/design.md §4): one
                // direction's bodies are hollow. A body needs 3 device pixels each way to
                // have an interior; smaller ones stay filled.
                let hollow = t.hollow.is_hollow(c.up) && bw >= 3.0 * hair && bh >= 3.0 * hair;
                if hollow {
                    // Wick above and below the body only, then a one-device-pixel outline.
                    let wx = c.x - hair * 0.5;
                    if c.body_top > c.wick_top {
                        window.paint_quad(fill(
                            rect(wx, c.wick_top, hair, c.body_top - c.wick_top),
                            color,
                        ));
                    }
                    if c.wick_bottom > c.body_bottom {
                        window.paint_quad(fill(
                            rect(wx, c.body_bottom, hair, c.wick_bottom - c.body_bottom),
                            color,
                        ));
                    }
                    window.paint_quad(fill(rect(bx, c.body_top, bw, hair), color));
                    window.paint_quad(fill(rect(bx, c.body_bottom - hair, bw, hair), color));
                    window.paint_quad(fill(rect(bx, c.body_top, hair, bh), color));
                    window.paint_quad(fill(rect(bx + bw - hair, c.body_top, hair, bh), color));
                } else {
                    window.paint_quad(fill(
                        rect(
                            c.x - hair * 0.5,
                            c.wick_top,
                            hair,
                            c.wick_bottom - c.wick_top,
                        ),
                        color,
                    ));
                    window.paint_quad(fill(rect(bx, c.body_top, bw, bh), color));
                }
            }
            if studies {
                for l in &mut self.layers {
                    l.paint(plot, &self.vp, &t, window, cx);
                }
                self.studies.paint((ox, oy), &t, window);
                self.levels.paint_front(plot, window);
            }
            if labels {
                self.levels.paint_labels(plot, &t, window, cx);
                self.profile.paint_label(plot, &t, window, cx);
            }
            if let Some(p) = self.last_trade {
                let y = self.vp.price_to_y(p.0 as f64) as f32;
                window.paint_quad(fill(rect(0.0, y, pw, hair), t.last.opacity(0.6)));
            }
            if self.cross.is_none()
                && let Some((x, _)) = self.sync_marker
            {
                window.paint_quad(fill(rect(x - hair * 0.5, 0.0, hair, ph), t.crosshair));
            }
            if let Some(c) = self.cross {
                // Vertical line through the wick; the horizontal one stops either side of it
                // so the translucent lines don't double up where they cross.
                let (left, right) = (c.x - hair * 0.5, c.x + hair * 0.5);
                window.paint_quad(fill(rect(left, 0.0, hair, ph), t.crosshair));
                window.paint_quad(fill(rect(0.0, c.y, left.max(0.0), hair), t.crosshair));
                window.paint_quad(fill(rect(right, c.y, pw - right, hair), t.crosshair));
            }
        });

        // Axis borders.
        window.paint_quad(fill(rect(pw, 0.0, hair, ph), t.axis_line));
        window.paint_quad(fill(rect(0.0, ph, pw, hair), t.axis_line));

        let line_h = px(LABEL_PX * 1.3);
        // The tags drawn over the price axis below; a label any of them would cover isn't
        // drawn at all (a half-covered number reads as another price).
        let last_y = self.last_trade.map(|p| last_tag_y(&self.vp, p, ph));
        let tags = [
            last_y.map(|y| Span::around(y, TAG_H * 0.5)),
            self.cross
                .map(|c| Span::around(cross_tag_y(c.y, ph), TAG_H * 0.5)),
        ];
        for i in 0..self.scene.price_lines.len() {
            let l = self.scene.price_lines[i];
            if l.y < 0.0 || l.y > ph {
                continue;
            }
            if !label_clear_of_tags(Span::around(l.y, LABEL_PX * 0.65), &tags) {
                continue;
            }
            let origin = point(px(ox + pw + 8.0), px(oy + l.y - LABEL_PX * 0.65));
            self.paint_label(
                LabelKind::Price,
                l.price.0,
                origin,
                line_h,
                t.text_dim,
                window,
                cx,
                |s| {
                    let _ = fmt.write(l.price, s);
                },
            );
        }
        for i in 0..self.scene.time_lines.len() {
            let l = self.scene.time_lines[i];
            if l.x < 0.0 || l.x > pw - 20.0 {
                continue;
            }
            let origin = point(px(ox + l.x + 4.0), px(oy + ph + 4.0));
            self.paint_label(
                LabelKind::Time,
                l.time.0,
                origin,
                line_h,
                t.text_dim,
                window,
                cx,
                |s| {
                    let _ = l.time.write_axis_label(s);
                },
            );
        }
        if let (Some(p), Some(y)) = (self.last_trade, last_y) {
            window.paint_quad(fill(
                rect(pw + hair, y - TAG_H * 0.5, AXIS_W, TAG_H),
                t.last,
            ));
            let origin = point(px(ox + pw + 8.0), px(oy + y - LABEL_PX * 0.65));
            self.paint_label(
                LabelKind::LastPrice,
                p.0,
                origin,
                line_h,
                t.on_last,
                window,
                cx,
                |s| {
                    let _ = fmt.write(p, s);
                },
            );
        }
        if !self.vp.auto_price {
            self.draw_manual_scale_indicator(bounds, window, cx);
        }
        if let Some(c) = self.cross {
            self.draw_crosshair_tags(c, bounds, window, cx);
        } else if let Some((x, time)) = self.sync_marker {
            self.draw_time_tag(x, time, bounds, window, cx);
        }
        if let Some(bar) = self.cross_bar {
            self.draw_readout(bar, bounds, window, cx);
        }
    }

    /// Price auto-scale is off: a small accent word in the empty corner under the price axis,
    /// where it never covers a label or moves anything. Double-click the price axis or press
    /// Home to restore auto-scale.
    fn draw_manual_scale_indicator(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let (pw, ph) = (
            f32::from(self.plot.size.width),
            f32::from(self.plot.size.height),
        );
        let accent = self.theme.accent;
        let w = f32::from(
            self.label(LabelKind::Indicator, 0, accent, window, |s| {
                let _ = s.write_str(MANUAL_LABEL);
            })
            .width,
        );
        let corner_w = f32::from(bounds.size.width) - pw;
        let origin = point(
            px(ox + pw + ((corner_w - w) * 0.5).max(4.0)),
            px(oy + ph + 4.0),
        );
        self.paint_label(
            LabelKind::Indicator,
            0,
            origin,
            px(LABEL_PX * 1.3),
            accent,
            window,
            cx,
            |s| {
                let _ = s.write_str(MANUAL_LABEL);
            },
        );
    }

    /// Price tag on the price axis and time tag on the time axis, drawn over the axis labels.
    fn draw_crosshair_tags(
        &mut self,
        c: Crosshair,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        let scale = window.scale_factor();
        let hair = 1.0 / scale;
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let (pw, ph) = (
            f32::from(self.plot.size.width),
            f32::from(self.plot.size.height),
        );
        let line_h = px(LABEL_PX * 1.3);
        let fmt = self.instrument.price;

        // Price tag: same box as the last-price tag, kept fully inside the plot's height.
        let y = cross_tag_y(c.y, ph);
        let top = snap(f64::from(y - TAG_H * 0.5), scale);
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox + pw + hair), px(oy + top)),
                size(px(AXIS_W), px(TAG_H)),
            ),
            t.cross_tag,
        ));
        let price = c.price;
        self.paint_label(
            LabelKind::CrossPrice,
            price.0,
            point(px(ox + pw + 8.0), px(oy + y - LABEL_PX * 0.65)),
            line_h,
            t.on_cross_tag,
            window,
            cx,
            |s| {
                let _ = fmt.write(price, s);
            },
        );

        if let Some(time) = c.time {
            self.draw_time_tag(c.x, time, bounds, window, cx);
        }
    }

    /// A time tag on the time axis, centred on `x` (plot-local) and clamped to the axis.
    fn draw_time_tag(
        &mut self,
        x: f32,
        time: UtcMillis,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        let scale = window.scale_factor();
        let hair = 1.0 / scale;
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let (pw, ph) = (
            f32::from(self.plot.size.width),
            f32::from(self.plot.size.height),
        );
        let line_h = px(LABEL_PX * 1.3);
        let period = self.period_ms;
        let text = move |s: &mut FixedStr<32>| {
            let _ = time.write_crosshair_label(period, s);
        };
        let text_w = f32::from(
            self.label(LabelKind::CrossTime, time.0, t.on_cross_tag, window, text)
                .width,
        );
        let box_w = text_w + 12.0;
        let left = snap(f64::from((x - box_w * 0.5).min(pw - box_w).max(0.0)), scale);
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox + left), px(oy + ph + hair)),
                size(px(box_w), px(AXIS_H - hair)),
            ),
            t.cross_tag,
        ));
        self.paint_label(
            LabelKind::CrossTime,
            time.0,
            point(px(ox + left + 6.0), px(oy + ph + 4.0)),
            line_h,
            t.on_cross_tag,
            window,
            cx,
            text,
        );
    }

    /// OHLCV of the hovered bar, top-left of the plot. Re-shaped only when the bar changes
    /// (a new bar is hovered, or the forming bar under the pointer ticks).
    fn draw_readout(
        &mut self,
        bar: Bar,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        if self.readout.as_ref().is_none_or(|(b, _)| *b != bar) {
            let mut s = FixedStr::<128>::new();
            let _ = write_ohlcv(&bar, &self.instrument.price, self.instrument.qty, &mut s);
            let line = self.shape(s.as_str(), t.text_main, window);
            self.readout = Some((bar, line));
        }
        let Some((_, line)) = &self.readout else {
            return;
        };
        let scale = window.scale_factor();
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let box_h = LABEL_PX * 1.6;
        let (left, top) = (snap(6.0, scale), snap(6.0, scale));
        let box_w = snap(f64::from(f32::from(line.width) + 12.0), scale);
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox + left), px(oy + top)),
                size(px(box_w), px(box_h)),
            ),
            t.panel.opacity(0.85),
        ));
        let origin = point(
            px(ox + left + 6.0),
            px(oy + top + box_h * 0.5 - LABEL_PX * 0.65),
        );
        let _ = line.paint(
            origin,
            px(LABEL_PX * 1.3),
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }

    fn shape(&self, text: &str, color: Hsla, window: &mut Window) -> ShapedLine {
        let run = TextRun {
            len: text.len(),
            font: self
                .font
                .clone()
                .unwrap_or_else(|| window.text_style().font()),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window.text_system().shape_line(
            SharedString::from(text.to_owned()),
            px(LABEL_PX),
            &[run],
            None,
        )
    }

    /// A cached shaped label. Shaping (and its allocation) happens only on a cache miss,
    /// i.e. the first time a given price/time label is shown.
    fn label(
        &mut self,
        kind: LabelKind,
        key: i64,
        color: Hsla,
        window: &mut Window,
        text: impl FnOnce(&mut FixedStr<32>),
    ) -> &ShapedLine {
        if self.labels.len() >= MAX_CACHED_LABELS && !self.labels.contains_key(&(kind, key)) {
            self.labels.clear();
        }
        if !self.labels.contains_key(&(kind, key)) {
            let mut s = FixedStr::<32>::new();
            text(&mut s);
            let line = self.shape(s.as_str(), color, window);
            self.labels.insert((kind, key), line);
        }
        &self.labels[&(kind, key)]
    }

    /// Paint a cached label (see [`Self::label`]).
    #[allow(clippy::too_many_arguments)]
    fn paint_label(
        &mut self,
        kind: LabelKind,
        key: i64,
        origin: Point<Pixels>,
        line_h: Pixels,
        color: Hsla,
        window: &mut Window,
        cx: &mut App,
        text: impl FnOnce(&mut FixedStr<32>),
    ) {
        let line = self.label(kind, key, color, window, text);
        let _ = line.paint(origin, line_h, TextAlign::Left, None, window, cx);
    }

    fn header(&self) -> gpui::Div {
        let t = self.theme;
        let bar = || {
            div()
                .flex()
                .flex_row()
                .justify_between()
                .gap_2()
                .whitespace_nowrap()
                .overflow_hidden()
                .px_3()
                .py_1()
                .bg(t.panel)
                .border_b_1()
                .border_color(t.axis_line)
                .text_sm()
        };
        let chip = pane::link_chip(self.common.link, &t);
        if let Some(title) = pane::unavailable_title(&self.feed) {
            return bar().child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .text_color(t.text_dim)
                    .children(chip)
                    .child(title),
            );
        }
        let mut left = String::with_capacity(96);
        let mut right = String::with_capacity(160);
        let Some(d) = self.store.try_read() else {
            return div(); // feed busy this instant; header refreshes next frame
        };
        let i = &d.product;
        let f = i.price;
        // The active contract is always shown (decision 0013); unknown is labelled, not hidden.
        let _ = match d.contract_label() {
            Some(c) => write!(left, "{c}  {}", i.description),
            None => write!(left, "{} (contract unknown)  {}", i.symbol, i.description),
        };
        // Last, bid, ask: a missing value shows as a dimmed dash (docs/design.md §7), never as
        // a bare label that reads like a layout bug.
        let quote = |label: &str, p: Option<Price>| {
            let (text, missing) = quote_text(label, p, &f);
            div()
                .ml_2()
                .text_color(if missing { t.text_dim } else { t.text_main })
                .child(text)
        };
        let mut quotes = vec![quote("last", d.last_trade)];
        if d.capabilities.bbo {
            quotes.push(quote("bid", d.bid));
            quotes.push(quote("ask", d.ask));
        }
        if let Some(n) = pane::adopted_note(&self.feed) {
            let _ = write!(right, "{n}  ·  ");
        }
        let _ = write!(right, "{}  ·  {}", d.provider, d.status.label());
        // History back-fill is shown as state, not a spinner (docs/design.md).
        if let Some(h) = d.history.label() {
            let _ = write!(right, "  ·  {h}");
        }
        if self.frozen {
            // Frozen clock: only data-derived time is shown, so screenshots are reproducible.
            if let Some(mt) = d.market_time {
                let _ = write!(right, "  ·  {mt}");
            }
        } else if self.perf_stats {
            // Debug detail, not data (docs/design.md §1): only behind "Performance stats".
            let r = &self.rates;
            let _ = write!(
                right,
                "  ·  {:.0} msgs/s  ·  {:.0} updates/s  ·  {:.0} paints/s  ·  paint {:.0} µs",
                r.msgs_per_sec, r.updates_per_sec, r.paints_per_sec, self.paint_time_ewma_us
            );
        }
        if d.stats.reconnects > 0 {
            let _ = write!(right, "  ·  reconnects {}", d.stats.reconnects);
        }
        if d.stats.gaps > 0 || d.stats.rejected > 0 {
            let _ = write!(
                right,
                "  ·  gaps {} rejected {}",
                d.stats.gaps, d.stats.rejected
            );
        }
        if let FeedStatus::Failed(msg) | FeedStatus::Reconnecting { reason: msg, .. } = &d.status {
            let _ = write!(right, "  ·  {msg}");
        }
        bar()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .flex_shrink_0()
                    .children(chip)
                    .child(div().text_color(t.text_main).child(left))
                    .children(quotes)
                    .children(pane::stale_badge(&self.store, &t)),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(t.text_dim)
                    .child(right),
            )
    }
}

impl EventEmitter<FeedChanged> for ChartView {}

impl EventEmitter<CrosshairTime> for ChartView {}

impl Pane for ChartView {
    fn store(&self) -> &Arc<ChartStore> {
        &self.store
    }

    fn feed(&self) -> &PaneFeed {
        &self.feed
    }

    fn common(&mut self) -> &mut PaneCommon {
        &mut self.common
    }

    fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn save_state(&self, doc: &mut PaneDoc) {
        doc.chart = Some(ChartState {
            bar_px: self.vp.bar_px,
            indicators: self.indicator_configs(),
            profile: self.profile.config(),
            visibility: self.visibility,
            levels: self.levels.config.clone(),
        });
        doc.trade = self.trade.doc();
    }

    fn seen_revision(&mut self) -> &mut u64 {
        &mut self.seen_revision
    }

    fn set_feed(&mut self, feed: PaneFeed, cx: &mut Context<Self>) {
        self.picker.close();
        let follow = feed.sub.is_some();
        self.apply_feed(feed);
        self.trade.feed_changed(&self.store, follow, cx);
        cx.notify();
    }

    fn on_tick(&mut self) {
        if self.perf_stats && !self.frozen {
            self.rates_changed |= self.update_rates();
        }
    }

    /// Charts don't show depth: a depth batch that leaves the quote alone doesn't repaint.
    fn revision(&self) -> u64 {
        self.store.chart_revision()
    }

    /// New indicator values (they arrive from the indicator thread after the store's), or new
    /// performance stats while shown (once a second). Hidden studies never ask for a repaint.
    fn changed(&mut self) -> bool {
        let rates = std::mem::take(&mut self.rates_changed);
        if !self.visibility.draws_studies() {
            return rates;
        }
        let r = self.studies.revision();
        let l = self.levels.revision();
        let changed = r != self.seen_studies || l != self.seen_levels;
        self.seen_studies = r;
        self.seen_levels = l;
        changed || rates
    }
}

/// Centre of the last-price tag (plot-local y). `max` last: the plot can be shorter than a
/// label (tiny windows).
fn last_tag_y(vp: &Viewport, p: Price, plot_h: f32) -> f32 {
    (vp.price_to_y(p.0 as f64) as f32)
        .min(plot_h - LABEL_PX)
        .max(0.0)
}

/// Centre of the crosshair's price tag, kept fully inside the plot's height.
fn cross_tag_y(y: f32, plot_h: f32) -> f32 {
    y.min(plot_h - TAG_H * 0.5).max(TAG_H * 0.5)
}

/// A header quote: `bid 6005.25`, or `bid —` (and `true`: show it dimmed) without a value.
fn quote_text(label: &str, p: Option<Price>, f: &lines_core::PriceFormat) -> (String, bool) {
    match p {
        Some(p) => (format!("{label} {}", f.format::<32>(p)), false),
        None => (format!("{label} —"), true),
    }
}

/// Under the store lock: this frame's study data, unless the chart hides its studies (then
/// no study does any work; their state stays as it was for when they are shown again).
#[allow(clippy::too_many_arguments)]
fn prepare_studies(
    visibility: StudyVisibility,
    profile: &mut VolumeProfileLayer,
    layers: &mut [Box<dyn ChartLayer>],
    studies: &mut crate::studies::Studies,
    levels: &mut LevelsLayer,
    d: &ChartData,
    vp: &Viewport,
    max_columns: usize,
) {
    if !visibility.draws_studies() {
        return;
    }
    profile.prepare(d, vp);
    for l in layers {
        l.prepare(d, vp);
    }
    studies.prepare(d, vp, max_columns);
    let last_y = d.last_trade.map(|p| vp.price_to_y(p.0 as f64) as f32);
    levels.prepare(d, vp, last_y);
}

/// The indicator engine's view of a chart store's feed.
fn feed_info(store: &ChartStore) -> lines_feed::indicators::FeedInfo {
    let d = store.read();
    lines_feed::indicators::FeedInfo {
        feed: store.feed_id(),
        product: d.product.id,
        period_ms: d.series.period_ms(),
        max_bars: d.series.max_len(),
    }
}

/// Key and mouse hints for a focused chart, in the window's status line.
pub const HINTS_SHORT: &str = "Chart: drag pan  ·  wheel zoom  ·  Ctrl+wheel or drag an axis: scale  ·  Home latest  ·  type a symbol: switch  ·  Ctrl+I studies  ·  Alt+V profile";

impl Render for ChartView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let entity = cx.entity();
        // The workspace refreshes every window when this preference changes.
        self.perf_stats = cx
            .try_global::<crate::workspace::Workspace>()
            .is_some_and(|w| w.show_perf_stats);
        let today = pane::today(self.frozen, &self.store);
        let overlay =
            self.picker
                .overlay(&t, &self.feed.availability, today, Self::picker_click, cx);
        let offer = self.common.offer_switch;
        if let Some(body) = pane::unavailable_body(&self.feed, &t, offer, cx) {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(t.bg)
                .child(self.header())
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .size_full()
                        .track_focus(&self.focus)
                        .on_key_down(cx.listener(Self::on_key_down))
                        .child(body)
                        .when_some(overlay, |d, o| d.child(o)),
                );
        }
        let _ = window;
        let account = crate::trade_bar::chart_chip(self, &t, cx);
        let accounts = crate::trade_bar::account_menu(self, &t, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .child(self.header().children(account))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .size_full()
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_exit(cx.listener(Self::on_mouse_exit))
                    .on_scroll(cx.listener(Self::on_scroll))
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, (), window, cx| {
                                entity.update(cx, |view, cx| view.paint_chart(bounds, window, cx));
                            },
                        )
                        .size_full(),
                    )
                    .when_some(self.legend(&t), |d, l| d.child(l))
                    .when_some(
                        self.study_menu.as_ref().map(|m| {
                            let items = self.menu_items(m, cx);
                            crate::study_menu_view::render(m, &items, &t, cx)
                        }),
                        |d, m| d.child(m),
                    )
                    .when(self.anchoring.is_some(), |d| {
                        d.child(crate::study_menu_view::anchor_banner(&t))
                    })
                    .when_some(overlay, |d, overlay| d.child(overlay))
                    .when_some(accounts, |d, o| d.child(o)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lines_core::{Aggressor, Capabilities, Month, Qty};
    use lines_feed::catalog;
    use lines_feed::provider::MarketEvent;

    use crate::profile::{ProfileConfig, ProfileMode};

    /// Hidden studies do no work: no profile rows are built, no study columns prepared.
    /// Showing them again prepares from the same configuration.
    #[test]
    fn hidden_studies_skip_prepare() {
        let es = catalog::get(catalog::ES).unwrap();
        let z26 = lines_core::ContractKey::monthly(es.id, 2026, Month::new(12).unwrap());
        let store = ChartStore::new(es, Some(z26), Capabilities::default(), "test", 60_000, 100);
        let mut d = store.read();
        d.apply(&[MarketEvent::Trade {
            time: UtcMillis::parse_iso("2026-11-05T15:00:00").unwrap(),
            price: Price(600_000),
            qty: Qty(3),
            aggressor: Some(Aggressor::Buy),
        }]);
        let mut vp = Viewport::default();
        vp.set_size(800.0, 600.0);
        (vp.price_lo, vp.price_hi) = (599_000.0, 601_000.0);
        let mut profile = VolumeProfileLayer::new(ProfileConfig {
            mode: ProfileMode::Session,
            ..ProfileConfig::default()
        });
        let mut studies =
            crate::studies::Studies::new(vec![lines_indicators::IndicatorConfig::Vwap {
                spec: lines_indicators::VwapSpec::default(),
                color: 0,
                display: Default::default(),
            }]);
        let hidden = StudyVisibility {
            hide_studies: true,
            ..StudyVisibility::default()
        };
        for _ in 0..3 {
            prepare_studies(
                hidden,
                &mut profile,
                &mut [],
                &mut studies,
                &mut LevelsLayer::new(LevelsConfig::default()),
                &d,
                &vp,
                256,
            );
        }
        assert!(!profile.has_built());
        assert_eq!(studies.reserved_columns(), 0);
        // Labels hidden only: the studies themselves are still prepared (and drawn).
        let labels_only = StudyVisibility {
            hide_study_labels: true,
            ..StudyVisibility::default()
        };
        prepare_studies(
            labels_only,
            &mut profile,
            &mut [],
            &mut studies,
            &mut LevelsLayer::new(LevelsConfig::default()),
            &d,
            &vp,
            256,
        );
        assert!(profile.has_built());
        assert!(studies.reserved_columns() >= 256);
    }

    #[test]
    fn missing_quotes_show_a_dimmed_dash() {
        let f = catalog::get(catalog::ES).unwrap().price;
        assert_eq!(quote_text("bid", None, &f), ("bid —".to_owned(), true));
        assert_eq!(
            quote_text("ask", Some(Price(600_025)), &f),
            ("ask 6000.25".to_owned(), false)
        );
    }
}
