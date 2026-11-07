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
//! Crosshair sync (decision 0016, opt-in per workspace): after each paint the chart emits
//! [`CrosshairTime`] when the time or price under its pointer changed; the workspace hands it
//! to the other charts of the same link group (`tabstate::crosshair_peers`), which mark that
//! time with a vertical line and a time tag, and, with price sync on, the price as well where
//! the product is the same (`tabstate::syncs_price`: other instruments' prices mean nothing
//! here). Mapping a time back to a bar is pure (`geometry::time_slot`).
//!
//! Magnet crosshair (per chart, saved): over a bar the price snaps to its open, high, low or
//! close, whichever is nearest the pointer (`geometry::magnet_price`).
//!
//! Layers (seam for the volume profile and other studies): a [`ChartLayer`] is prepared under
//! the store lock with this frame's data and viewport, then painted inside the plot after the
//! candles. Both steps must not allocate (docs/design.md §2).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    AnyElement, App, Bounds, ContentMask, Context, EventEmitter, FocusHandle, Font, FontWeight,
    Hsla, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollEvent, ShapedLine, SharedString,
    Styled, TextAlign, TextRun, Window, canvas, div, fill, point, prelude::*, px, size,
};
use lines_core::drawings::{Anchor, TimeAxis, Tool, snap_anchor, write_measure};
use lines_core::geometry::{
    Crosshair, CrosshairMode, GridSpace, Span, TimeLabel, crosshair, label_clear_of_tags, slot_x,
    snap, write_ohlcv,
};
use lines_core::timeframe::bar_date;
use lines_core::viewport::{MAX_BAR_PX, MIN_BAR_PX, ScaleDrag, ScaleMode};
use lines_core::{
    Bar, BarsView, Derived, FixedStr, Price, Product, ProductId, Scene, Timeframe, UtcMillis,
    Viewport,
};
use lines_feed::ChartData;
use lines_feed::catalog::Target;
use lines_feed::store::{ChartStore, FeedStatus};

use crate::chart_draw::{self, ChartDrawings};
use crate::commands::DrawCmd;
use crate::feeds::{Feeds, PaneFeed};
use crate::levels_layer::{LevelsConfig, LevelsLayer};
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::picker_view::{PickerKey, PickerUi};
use crate::pnl::PnlDisplay;
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
    /// A time gridline's date (`MM-DD`), and date with time.
    Date,
    DateTime,
    LastPrice,
    CrossPrice,
    /// A sub-pane's value labels (cumulative delta): range ends, and the newest-value tag
    /// (its own kind: the label cache keys by kind and value, and the colors differ).
    SubValue,
    SubLast,
    /// The sub-pane's title (keyed by its reset) and its "≈ from bars" note.
    SubTitle,
    SubNote,
    /// The crosshair's value tag on the sub-pane's axis.
    SubCross,
    CrossTime,
    Indicator,
}

/// Behind a drawing's note text: the box's padding (logical px).
const NOTE_PAD: f32 = 6.0;

/// Where a pointer event landed.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Region {
    Plot,
    /// The price axis column, including the corner below it.
    PriceAxis,
    TimeAxis,
    /// A sub-pane below the plot (`crate::subpane`): drags pan time only.
    Sub,
    /// The divider between the plot and the sub-pane.
    Divider,
    /// The axis column beside the sub-pane: its scale fits itself, so nothing happens here
    /// (never the price axis's scaling).
    SubAxis,
}

/// A study drawn over the plot (e.g. the volume profile). Implementations keep their own
/// buffers; neither method may allocate in steady state.
pub trait ChartLayer: 'static {
    /// Under the store lock, after this frame's viewport update: copy what `paint` needs.
    /// `bars` are the bars the chart shows (its timeframe's; the viewport indexes them).
    fn prepare(&mut self, data: &ChartData, bars: BarsView<'_>, vp: &Viewport);
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
    /// Start with the magnet crosshair (screenshots: `--magnet`).
    pub magnet: bool,
    /// Initial timeframe (`--timeframe`). One saved with the pane takes precedence.
    pub timeframe: Timeframe,
    /// Start with the timeframe list open (screenshots: `--timeframe-menu`).
    pub timeframe_menu: bool,
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
    /// Last pointer position; `time_only`: started in a sub-pane (no price pan).
    Pan(Point<Pixels>, bool),
    Scale(ScaleDrag),
    /// The sub-pane divider: where the drag started (window y) and the ratio then.
    Divider {
        y: f32,
        ratio: f32,
    },
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

/// What is under this chart's crosshair changed (`time: None`: it left the plot). The
/// workspace forwards it to the chart's link group when crosshair sync is on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CrosshairTime {
    pub time: Option<UtcMillis>,
    /// The crosshair's price (with the magnet: the bar's open/high/low/close).
    pub price: Option<Price>,
    /// The chart's product: prices sync only between charts of the same product.
    pub product: ProductId,
}

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
    /// The footprint chart mode (roadmap 10e), saved with the pane. Kept typed (it replaces
    /// candles bar by bar).
    footprint: crate::footprint::FootprintLayer,
    /// The sub-pane's share of the height (`crate::subpane`), saved with the pane.
    subpane_ratio: f32,
    /// This frame's split between the plot and the sub-pane (`None`: no sub-pane).
    split: Option<crate::subpane::Split>,
    /// Height above the time axis this frame (the plot plus any sub-pane).
    above_axis: f32,
    /// The pointer is in the sub-pane, this far below its top: the crosshair's vertical line
    /// and time follow it; the horizontal line is in the sub-pane, with no price.
    cross_in_sub: Option<f32>,
    /// The legend and readout block at the plot's top-left (width, bottom; plot-local), as
    /// the last render built it: the footprint draws no cell number under it.
    legend_band: std::cell::Cell<(f32, f32)>,
    /// The footprint's reasons for candles as last rendered in the legend.
    fp_seen: crate::footprint::Fallbacks,
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
    /// Another chart's crosshair price (price sync, same product only).
    synced_price: Option<Price>,
    /// This frame's synced price, with its plot-local y (when on screen).
    sync_price: Option<(f32, Price)>,
    /// Magnet crosshair: snap to the hovered bar's open/high/low/close (saved).
    crosshair_mode: CrosshairMode,
    /// Axis label spacing has been measured with this window's font.
    space_measured: bool,
    /// The time this chart last reported under its own crosshair.
    reported: (Option<UtcMillis>, Option<Price>),
    /// Screenshot fixture: price zoom to apply once, after the first auto-fit.
    pending_price_zoom: Option<f64>,
    /// Pointer position in plot-local logical px, while it is over the plot.
    hover: Option<(f32, f32)>,
    /// This frame's crosshair (from `hover`), and a copy of the bar under it.
    cross: Option<Crosshair>,
    cross_bar: Option<Bar>,
    period_ms: i64,
    /// OHLCV readout (and whether the bar is partial), shaped once per distinct value.
    readout: Option<(Bar, bool, ShapedLine)>,
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
    /// The position tag's text (glyphs shaped once).
    pos_glyphs: crate::text::Glyphs,
    /// The chart's timeframe (roadmap 9), saved with the pane. Never shared through link
    /// groups: they share the market only.
    timeframe: Timeframe,
    /// Its bars when it isn't 1 minute: derived from the store's 1-minute bars, updated
    /// incrementally in paint.
    derived: Option<Derived>,
    /// After a timeframe change: put the bar holding this time at this plot-local x.
    tf_anchor: Option<(UtcMillis, f64)>,
    /// The timeframe list (header chip, pane menu), while open.
    tf_menu: bool,
    /// A timeframe being typed (digits, then a unit), while typing.
    tf_entry: Option<String>,
    /// The hovered bar was cut short by a session break.
    cross_partial: bool,
    /// The product's drawings on this chart (roadmap 11).
    drawings: ChartDrawings,
    /// Note texts shaped (by drawing id; re-shaped only when the text changes).
    note_lines: HashMap<u64, (String, ShapedLine)>,
    /// The note being typed, shaped once per key.
    note_entry_line: Option<(String, ShapedLine)>,
    /// Measure readouts (`None`: the temporary one; else a pinned measure's id), shaped once
    /// per distinct reading.
    measure_lines: HashMap<Option<u64>, (lines_core::drawings::MeasureStats, ShapedLine)>,
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
        let timeframe = fixture.timeframe;
        let drawings = ChartDrawings::new(crate::drawings_store::for_product(&instrument, cx));

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
            footprint: crate::footprint::FootprintLayer::new(Default::default()),
            subpane_ratio: crate::subpane::DEFAULT_RATIO,
            split: None,
            above_axis: 0.0,
            cross_in_sub: None,
            fp_seen: Default::default(),
            legend_band: std::cell::Cell::new((8.0 + LEGEND_MAX_W, READOUT_BOTTOM)),
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
            synced_price: None,
            sync_price: None,
            crosshair_mode: if fixture.magnet {
                CrosshairMode::Magnet
            } else {
                CrosshairMode::Normal
            },
            space_measured: false,
            reported: (None, None),
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
            pos_glyphs: crate::text::Glyphs::new(LABEL_PX),
            timeframe: Timeframe::ONE_MINUTE,
            derived: None,
            tf_anchor: None,
            tf_menu: fixture.timeframe_menu,
            tf_entry: None,
            cross_partial: false,
            drawings,
            note_lines: HashMap::new(),
            note_entry_line: None,
            measure_lines: HashMap::new(),
        };
        view.apply_timeframe(timeframe);
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
            footprint: self.footprint.config(),
        }
    }

    /// Replace this chart's studies (a template, the defaults, a menu change).
    pub fn set_study_set(&mut self, s: StudySet) {
        if s.indicators != self.studies.configs() {
            self.set_indicator_configs(s.indicators);
        }
        self.profile.set_config(s.profile);
        self.levels.config = s.levels;
        self.footprint.set_config(s.footprint);
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
            let y = f32::from(pos.y - self.plot.origin.y);
            if self.split.is_some() && y >= f32::from(self.plot.size.height) && y < self.above_axis
            {
                Some(Region::SubAxis)
            } else {
                Some(Region::PriceAxis)
            }
        } else if let Some(s) = self.split
            && f32::from(pos.y - self.plot.origin.y) < self.above_axis
        {
            let y = f32::from(pos.y - self.plot.origin.y);
            Some(if s.on_divider(y) {
                Region::Divider
            } else {
                Region::Sub
            })
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
        let tf = std::mem::take(&mut self.tf_menu);
        if std::mem::take(&mut self.trade.account_menu) || tf {
            // Clicking the chart dismisses the timeframe or account list (their rows handle
            // their clicks).
            cx.notify();
            return;
        }
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
                    .and_then(|d| anchor_at(bars_view(&self.derived, &d).bars, &self.vp, x))
            {
                self.anchoring = None;
                self.run_menu(
                    Some(MenuCmd::Set(StudyAction::AnchorAt { index, from })),
                    cx,
                );
            }
            return;
        }
        // Drawings take a press only in drawing mode or on a drawing, never with Ctrl (chart
        // trading's); otherwise the chart pans and scales as usual.
        if region == Region::Plot {
            let anchor = self.drawing_anchor(x, y);
            let ctrl = ev.modifiers.control || ev.modifiers.platform;
            if self.drawings.on_down(x as f32, y as f32, ctrl, anchor) {
                cx.notify();
                return;
            }
        }
        if ev.click_count >= 2 {
            match region {
                Region::Plot => self.vp.reset(self.series_len),
                Region::PriceAxis => self.vp.restore_auto_price(),
                // Each axis resets only itself: time = bar width and the live edge.
                Region::TimeAxis | Region::Sub => self.vp.reset_time(self.series_len),
                Region::Divider => self.subpane_ratio = crate::subpane::DEFAULT_RATIO,
                Region::SubAxis => return,
            }
            cx.notify();
            return;
        }
        let drag = match region {
            Region::Plot if ev.modifiers.control => {
                Drag::Scale(ScaleDrag::new(&self.vp, ScaleMode::Free, x, y))
            }
            Region::Plot => Drag::Pan(ev.position, false),
            Region::Sub => Drag::Pan(ev.position, true),
            // From the split actually drawn (the minimums may have clamped the saved ratio),
            // so the divider follows the pointer from the first move.
            Region::Divider => Drag::Divider {
                y: f32::from(ev.position.y),
                ratio: self.split.map_or(self.subpane_ratio, |s| {
                    s.sub_h / (self.above_axis - crate::subpane::DIVIDER_PX).max(1.0)
                }),
            },
            Region::SubAxis => return,
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
        // Drawings: a note being typed or a confirmation takes every key; Esc, Enter, Delete
        // go to them when they have a use for it (a tool, a measure, a selection).
        let typed = k
            .key_char
            .as_deref()
            .filter(|_| !m.control && !m.alt && !m.platform);
        if !m.control
            && !m.platform
            && (self.drawings.typing()
                || (self.tf_entry.is_none()
                    && matches!(k.key.as_str(), "escape" | "enter" | "delete" | "backspace")))
            && self.drawings.key(&k.key, typed) == chart_draw::Key::Handled
        {
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.tf_entry.is_some() {
            self.timeframe_key(k, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.tf_menu && k.key == "escape" {
            self.tf_menu = false;
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
        } else if let Some(digit) = k.key_char.as_deref().filter(|c| {
            !m.control && !m.alt && !m.platform && c.len() == 1 && c.as_bytes()[0].is_ascii_digit()
        }) {
            // Type a timeframe: digits, then an optional unit (m, h, d, w), then Enter.
            self.tf_menu = false;
            self.tf_entry = Some(digit.to_owned());
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
        // The same timeframe on the new market.
        self.derived = None;
        self.tf_anchor = None;
        let tf = std::mem::replace(&mut self.timeframe, Timeframe::ONE_MINUTE);
        self.apply_timeframe(tf);
    }

    pub fn timeframe(&self) -> Timeframe {
        self.timeframe
    }

    /// Switch the timeframe (header list, typed, palette): the time under the pointer stays
    /// where it is (with no pointer, the plot's middle, unless the chart follows the live
    /// edge, which it keeps doing).
    pub fn set_timeframe(&mut self, tf: Timeframe, cx: &mut Context<Self>) {
        self.tf_menu = false;
        self.tf_entry = None;
        if tf != self.timeframe && tf.is_valid() {
            let x = match self.hover {
                Some((x, _)) => Some(f64::from(x)),
                None if !self.vp.follow_live => Some(self.vp.width * 0.5),
                None => None,
            };
            let anchor = x.and_then(|x| {
                let d = self.store.try_read()?;
                let view = bars_view(&self.derived, &d);
                let last = view.len().checked_sub(1)?;
                let i = self.vp.x_to_index(x).round().clamp(0.0, last as f64) as usize;
                Some((view.bars[i].time, x))
            });
            self.apply_timeframe(tf);
            self.tf_anchor = anchor;
        }
        cx.notify();
    }

    /// Set the timeframe without moving the view (restore, a new feed).
    fn apply_timeframe(&mut self, tf: Timeframe) {
        if tf == self.timeframe || !tf.is_valid() {
            return;
        }
        self.timeframe = tf;
        let capacity = self.store.read().series.capacity();
        self.derived = (!tf.is_one_minute()).then(|| Derived::new(tf, capacity));
        // Indices change: re-baseline the trim count on the next paint.
        self.series_trimmed = u64::MAX;
        // Time labels are cached by bar time; their text depends on the timeframe.
        self.labels.clear();
        self.readout = None;
        self.cross = None;
        self.cross_bar = None;
    }

    /// Open or close the timeframe list (header chip, pane menu, palette).
    pub fn toggle_timeframe_menu(&mut self, cx: &mut Context<Self>) {
        self.tf_menu = !self.tf_menu;
        if self.tf_menu {
            self.picker.close();
            self.study_menu = None;
            self.tf_entry = None;
            self.trade.account_menu = false;
        }
        cx.notify();
    }

    /// Another chart's crosshair time (crosshair sync); `None` clears it.
    pub fn set_synced_time(
        &mut self,
        t: Option<UtcMillis>,
        price: Option<Price>,
        cx: &mut Context<Self>,
    ) {
        if self.synced_time != t || self.synced_price != price {
            self.synced_time = t;
            self.synced_price = price;
            cx.notify();
        }
    }

    /// The product this chart shows.
    pub fn product(&self) -> ProductId {
        self.instrument.id
    }

    /// Magnet crosshair (snap to OHLC) on this chart.
    pub fn magnet(&self) -> bool {
        self.crosshair_mode == CrosshairMode::Magnet
    }

    /// The `ToggleMagnet` command.
    pub fn toggle_magnet(&mut self, cx: &mut Context<Self>) {
        self.crosshair_mode = if self.magnet() {
            CrosshairMode::Normal
        } else {
            CrosshairMode::Magnet
        };
        cx.notify();
    }

    /// Restore saved view state (time zoom, indicators).
    pub fn restore(&mut self, state: &ChartState) {
        self.drawings.hidden = state.hide_drawings;
        if state.magnet {
            self.crosshair_mode = CrosshairMode::Magnet;
        }

        // A saved timeframe wins; a pane without one keeps its initial (CLI) timeframe.
        if !state.timeframe.is_default() {
            self.apply_timeframe(state.timeframe.0);
        }
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
        if !state.footprint.is_default() {
            self.footprint.set_config(state.footprint);
        }
        if let Some(r) = state.subpane {
            self.subpane_ratio = crate::subpane::clamp_ratio(r);
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
            // The readout still shows over the plot's top-left.
            self.legend_band.set((8.0 + LEGEND_MAX_W, READOUT_BOTTOM));
            return None;
        }
        let studies = self.studies.legend(t, &self.instrument);
        // The footprint's line: its mode and why bars are candles (as of the last frame;
        // paint re-renders when that changes).
        let fp = self.footprint.legend();
        let fp = fp.map(|(label, note)| {
            // Capped at the legend's width (the clearance block assumes it), ending in "…".
            div()
                .max_w(px(LEGEND_MAX_W))
                .whitespace_nowrap()
                .truncate()
                .flex()
                .flex_row()
                .gap_2()
                .px(px(8.0))
                .py(px(4.0))
                .bg(t.label_bg)
                .text_size(px(12.0))
                .child(div().text_color(t.text_main).child(label))
                .when_some(note, |d, n| d.child(div().text_color(t.text_dim).child(n)))
        });
        let rows = if self.levels.config.hide_labels {
            Vec::new()
        } else {
            self.levels.legend()
        };
        // The block this legend takes at the plot's top-left (with the readout above it), for
        // the footprint to keep its numbers out of (the same frame: render runs before paint).
        // Rows of 12 px text in boxes padded 4 px, boxes 4 px apart; a margin of one gap.
        let (boxes, text_rows) = (
            usize::from(studies.is_some())
                + usize::from(fp.is_some())
                + usize::from(!rows.is_empty()),
            self.studies.legend_rows() + usize::from(fp.is_some()) + rows.len(),
        );
        let row_h = 12.0 * 1.3 + 4.0;
        let bottom = 30.0 + text_rows as f32 * row_h + boxes as f32 * 12.0 + 4.0;
        self.legend_band
            .set((8.0 + LEGEND_MAX_W, bottom.max(READOUT_BOTTOM)));
        if studies.is_none() && rows.is_empty() && fp.is_none() {
            self.legend_band.set((8.0 + LEGEND_MAX_W, READOUT_BOTTOM));
            return None;
        }
        // Level rows: the source's name, then its state; a long reason ends in an ellipsis at
        // the legend's width, never running over the chart.
        let levels = (!rows.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .max_w(px(LEGEND_MAX_W))
                .px(px(8.0))
                .py(px(4.0))
                .bg(t.label_bg)
                .text_size(px(12.0))
                .children(rows.into_iter().map(|r| {
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .min_w_0()
                        .child(div().flex_shrink_0().text_color(t.text_main).child(r.label))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
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
                .children(fp)
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
        let l = pos - self.plot.origin;
        let (x, y) = (f32::from(l.x), f32::from(l.y));
        let in_sub = self
            .split
            .is_some_and(|s| s.in_sub(y) && x >= 0.0 && x < f32::from(self.plot.size.width));
        let hover = (self.plot.contains(&pos) || in_sub).then_some((x, y));
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
            Some(Drag::Pan(from, time_only)) => {
                let d = ev.position - *from;
                *from = ev.position;
                let dy = if *time_only { 0.0 } else { f64::from(d.y) };
                self.vp.pan(f64::from(d.x), dy, len);
            }
            Some(Drag::Scale(drag)) => drag.update(&mut self.vp, x, y, len, tick),
            Some(Drag::Divider { y: y0, ratio }) => {
                let dy = f32::from(ev.position.y) - *y0;
                self.subpane_ratio = crate::subpane::drag(self.above_axis, *ratio, dy);
            }
            None => return,
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (x, y) = self.local(ev.position);
        // A release off the plot (an axis) never places an anchor.
        let anchor = (self.region(ev.position) == Some(Region::Plot))
            .then(|| self.drawing_anchor(x, y))
            .flatten();
        if self.drawings.on_up(x as f32, y as f32, anchor) {
            cx.notify();
        }
        if self.drag.is_some() {
            self.end_drag();
            cx.notify();
        }
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.set_hover(ev.position, cx);
        // The sub-pane shares the plot's time axis: the wheel pans and zooms time there too
        // (its value scale fits itself, so Ctrl+wheel does nothing).
        let in_sub = matches!(self.region(ev.position), Some(Region::Sub));
        if !self.plot.contains(&ev.position) && !in_sub {
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
                if in_sub {
                    return;
                }
                self.vp.zoom_price_at(y, factor, self.instrument.price.tick);
            } else {
                let anchor = self.vp.time_zoom_anchor(x, len);
                self.vp.zoom_at(anchor, factor, len);
            }
        }
        cx.notify();
    }

    /// A key while a timeframe is being typed: digits and units extend it, Backspace edits,
    /// Enter switches (if it names an offered timeframe), Esc cancels.
    fn timeframe_key(&mut self, k: &gpui::Keystroke, cx: &mut Context<Self>) {
        let Some(entry) = self.tf_entry.as_mut() else {
            return;
        };
        match k.key.as_str() {
            "escape" => self.tf_entry = None,
            "enter" => {
                if let Some(tf) = Timeframe::parse(entry) {
                    self.set_timeframe(tf, cx);
                }
            }
            "backspace" => {
                entry.pop();
                if entry.is_empty() {
                    self.tf_entry = None;
                }
            }
            _ => {
                let m = k.modifiers;
                if let Some(c) = k.key_char.as_deref().filter(|c| {
                    !m.control
                        && !m.alt
                        && !m.platform
                        && c.len() == 1
                        && (c.as_bytes()[0].is_ascii_digit() || "mhdwMHDW".contains(*c))
                }) && entry.len() < 4
                {
                    entry.push_str(c);
                }
            }
        }
    }

    // ---- paint ---------------------------------------------------------------------

    fn paint_chart(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let t0 = Instant::now();
        self.common.paints += 1;
        let scale = window.scale_factor();
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let plot_w = (w - AXIS_W).max(10.0);
        let above_axis = (h - AXIS_H).max(10.0);
        // A sub-pane (cumulative delta) takes its share below the plot, above the time axis.
        self.split = (self.visibility.draws_studies() && self.studies.has_subpane())
            .then(|| crate::subpane::split_at_scale(above_axis, self.subpane_ratio, scale))
            .flatten();
        self.above_axis = above_axis;
        let plot_h = self.split.map_or(above_axis, |s| s.price_h);
        self.footprint.ensure_capacity(plot_w);
        self.chart = bounds;
        self.plot = Bounds::new(bounds.origin, size(px(plot_w), px(plot_h)));
        self.vp.set_size(plot_w as f64, plot_h as f64);
        // Only allocates when the window grows wider than ever before.
        self.scene.ensure_capacity(plot_w as f64);
        if self.font.is_none() {
            self.font = Some(window.text_style().font());
        }
        if !self.space_measured {
            self.measure_grid_space(window);
        }

        // Sample the feed. If it holds the lock this instant, redraw the previous scene.
        if let Some(d) = self.store.try_read() {
            // A higher timeframe: bring its bars up to date (only what changed).
            if let Some(dv) = &mut self.derived {
                dv.update(&d.series, lines_feed::catalog::sessions(d.product.id));
            }
            let view = bars_view(&self.derived, &d);
            let trimmed = self
                .derived
                .as_ref()
                .map_or(d.series.trimmed(), Derived::trimmed);
            if trimmed > self.series_trimmed && (!self.vp.follow_live || self.vp.held) {
                // Bars dropped from the front shift indices; keep the view anchored.
                self.vp.right_index -= (trimmed - self.series_trimmed) as f64;
            }
            self.series_trimmed = trimmed;
            self.series_len = view.len();
            self.last_trade = d.last_trade;
            let fmt = self.instrument.price;
            let bars = view.bars;
            if let Some((t, x)) = self.tf_anchor.take()
                && !bars.is_empty()
            {
                // After a timeframe change: the bar holding the anchored time goes where that
                // time was; live following resumes if the newest bar is then in view.
                let i = bars.partition_point(|b| b.time <= t).saturating_sub(1);
                self.vp.right_index += i as f64 - self.vp.x_to_index(x);
                if self.drag.is_none() {
                    self.vp.held = true;
                    self.vp.set_held(false, bars.len());
                }
            }
            self.vp.on_data(bars, fmt.tick);
            if self.visibility.draws_studies()
                && self.footprint.config().shown()
                && self.vp.auto_price
                && !self.vp.visible_range(bars.len()).is_empty()
            {
                // (Only after a real fit: with nothing visible the range isn't refitted, and
                // raising it again each frame would compound.)
                // Auto-fit leaves the readout and legend block (plus a row) clear of cells.
                let top = f64::from(self.legend_band.get().1) + crate::footprint::MIN_ROW_PX;
                crate::footprint::clear_top(&mut self.vp, top);
            }
            self.period_ms = view.period_ms;
            // Only once bars are visible, i.e. auto-scale has fitted a real range (the first
            // layout pass can paint at a placeholder size with nothing on screen).
            if !self.vp.visible_range(bars.len()).is_empty()
                && let Some(f) = self.pending_price_zoom.take()
            {
                self.vp.zoom_price_at(self.vp.height * 0.5, f, fmt.tick);
            }
            self.scene
                .build(bars, &self.vp, &fmt, scale, self.period_ms);
            // Hit-test after the viewport update so the crosshair matches this frame. In the
            // sub-pane only the time follows the pointer (the plot's y is the top row).
            self.cross_in_sub = self
                .hover
                .and_then(|(_, y)| self.split.filter(|s| s.in_sub(y)).map(|s| y - s.sub_top));
            let in_sub = self.cross_in_sub.is_some();
            self.cross = self.hover.and_then(|(x, y)| {
                crosshair(
                    bars,
                    &self.vp,
                    fmt.tick,
                    self.period_ms,
                    scale,
                    self.crosshair_mode,
                    f64::from(x),
                    if in_sub { 0.0 } else { f64::from(y) },
                )
            });
            self.cross_bar = self.cross.and_then(|c| c.bar).map(|i| bars[i]);
            self.cross_partial = match (self.cross.and_then(|c| c.bar), &self.derived) {
                (Some(i), Some(dv)) => dv.is_partial(i),
                _ => false,
            };
            // Another chart's crosshair time, where this chart has a bar for it (each chart
            // maps it into its own timeframe's bars).
            self.sync_marker = self.synced_time.and_then(|t| {
                let slot = view.slot(t)?;
                let x = slot_x(&self.vp, slot as f64, scale);
                (x >= 0.0 && f64::from(x) < self.vp.width).then_some((x, t))
            });
            // And its price, if this chart is told it (same product, price sync on).
            self.sync_price = self.synced_price.and_then(|p| {
                let y = snap(self.vp.price_to_y(p.0 as f64), scale);
                (y >= 0.0 && f64::from(y) < self.vp.height).then_some((y, p))
            });
            let max_columns = crate::studies::Studies::max_columns(self.plot.size.width);
            let (bw, bb) = self.legend_band.get();
            self.footprint.set_legend_block(bw, bb);
            if !self.visibility.draws_studies() {
                self.footprint.reset(); // hidden: no cells, no line gaps
            }
            prepare_studies(
                self.visibility,
                &mut self.profile,
                &mut self.footprint,
                &mut self.layers,
                &mut self.studies,
                &mut self.levels,
                &d,
                view,
                &self.vp,
                max_columns,
            );
            // The legend says why footprint bars are candles: re-render when that changes.
            let fb = self.footprint.fallbacks();
            if fb != self.fp_seen {
                self.fp_seen = fb;
                cx.notify();
            }
            // Drawings: projected for this timeframe; previews and drags follow the pointer.
            let magnet = self.magnet();
            self.drawings.set_pointer(self.hover);
            self.drawings.prepare(
                &TimeAxis::new(view),
                &self.vp,
                fmt.tick,
                magnet,
                &fmt,
                self.instrument.point_value,
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
            self.cross_in_sub = None;
        }

        self.draw(bounds, window, cx);

        // Crosshair sync: report this frame's time and price under the pointer when changed.
        let now = (
            self.cross.and_then(|c| c.time),
            self.cross
                .filter(|c| c.time.is_some() && self.cross_in_sub.is_none())
                .map(|c| c.price),
        );
        if now != self.reported {
            self.reported = now;
            cx.emit(CrosshairTime {
                time: now.0,
                price: now.1,
                product: self.instrument.id,
            });
        }

        let us = t0.elapsed().as_secs_f64() * 1e6;
        self.paint_time_ewma_us = if self.paint_time_ewma_us == 0.0 {
            us
        } else {
            self.paint_time_ewma_us * 0.95 + us * 0.05
        };
    }

    /// The chart's open position in its account (side and quantity signed, average price,
    /// open P&L, the P&L display mode), copied for this frame; `None` when flat, without an
    /// account, or while the trading cache is busy.
    fn position_for_draw(&self, cx: &App) -> Option<(i64, f64, lines_trading::Money, PnlDisplay)> {
        let trading = crate::trading::Trading::try_global(cx)?;
        let account = self.trade.account.as_ref()?;
        let contract = crate::trade_bar::contract_of(&self.store)?;
        let s = trading.cache.try_read()?;
        let p = s.position(account, contract)?;
        let mode = cx
            .try_global::<crate::picker_view::AppSettings>()
            .map(|a| a.settings.pnl_display)
            .unwrap_or_default();
        Some((p.qty, p.avg_price?, p.unrealised, mode))
    }

    /// The position line's tag at plot-local `y`: `LONG 2 @ 6000.25` and, unless P&L is off,
    /// the open P&L in the up/down colour with its sign (`+2.5 t  +$125.00`). Allocation-free
    /// (fixed buffers, cached glyphs).
    #[allow(clippy::too_many_arguments)]
    fn draw_position_tag(
        &mut self,
        (qty, avg, pnl, mode): (i64, f64, lines_trading::Money, PnlDisplay),
        y: f32,
        (ox, oy): (f32, f32),
        hair: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        let mut head = FixedStr::<64>::new();
        let _ = write!(
            head,
            "{} {} @ ",
            if qty > 0 { "LONG" } else { "SHORT" },
            qty.abs()
        );
        let _ = crate::trade_bar::write_avg(&mut head, &self.instrument, avg);
        let mut money = FixedStr::<64>::new();
        let ticks = self
            .last_trade
            .and_then(|m| crate::dom_trade::open_ticks(qty, avg, m, self.instrument.price.tick));
        let shown = crate::pnl::write_pnl(&mut money, mode, ticks, pnl).unwrap_or(false);
        let tone = match crate::pnl::tone(pnl) {
            crate::pnl::Tone::Up => t.up,
            crate::pnl::Tone::Down => t.down,
            crate::pnl::Tone::Flat => t.text_main,
        };
        let w_head = self
            .pos_glyphs
            .width(head.as_str(), t.text_main, true, window);
        let w_money = if shown {
            8.0 + self.pos_glyphs.width(money.as_str(), tone, true, window)
        } else {
            0.0
        };
        let (x, h) = (ox + 4.0, TAG_H);
        let top = oy + y - h * 0.5;
        let w = w_head + w_money + 12.0;
        window.paint_quad(fill(
            Bounds::new(point(px(x), px(top)), size(px(w), px(h))),
            t.label_bg,
        ));
        // Outlined, so the tag reads as the position's even over candles.
        for (bx, by, bw, bh) in [
            (x, top, w, hair),
            (x, top + h - hair, w, hair),
            (x, top, hair, h),
            (x + w - hair, top, hair, h),
        ] {
            window.paint_quad(fill(
                Bounds::new(point(px(bx), px(by)), size(px(bw), px(bh))),
                t.text_dim,
            ));
        }
        self.pos_glyphs.paint(
            head.as_str(),
            x + 6.0,
            top,
            h,
            t.text_main,
            true,
            window,
            cx,
        );
        if shown {
            self.pos_glyphs.paint(
                money.as_str(),
                x + 6.0 + w_head + 8.0,
                top,
                h,
                tone,
                true,
                window,
                cx,
            );
        }
    }

    fn draw(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let t = self.theme;
        let position = self.position_for_draw(cx);
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
            let fp_first = self.footprint.first_bar();
            for (k, c) in self.scene.candles.iter().enumerate() {
                // The footprint draws this bar as cells (only where it has trade data).
                if studies && self.footprint.covers(fp_first + k) {
                    continue;
                }
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
                self.footprint.paint(plot, &self.vp, &t, window, cx);
                for l in &mut self.layers {
                    l.paint(plot, &self.vp, &t, window, cx);
                }
                self.studies.paint((ox, oy), &t, window);
                self.levels.paint_front(plot, window);
            }
            // Drawings: the trader's own marks, above the studies (shown unless hidden on
            // this chart; hiding studies or a clean chart leaves them).
            if !self.drawings.hidden {
                self.drawings.paint((ox, oy), hair, &t, window);
                self.paint_drawing_text((ox, oy), window, cx);
            }
            if labels {
                self.levels.paint_labels(plot, &t, window, cx);
                self.profile.paint_label(plot, &t, window, cx);
            }
            // Horizontal lines break where they cross footprint cells (never through numbers).
            let fp = &self.footprint;
            let hline = |window: &mut Window, y: f32, x0: f32, x1: f32, color| {
                fp.hline_segments(y, x0, x1, |a, b| {
                    window.paint_quad(fill(rect(a, y, b - a, hair), color));
                });
            };
            if let Some(p) = self.last_trade {
                let y = self.vp.price_to_y(p.0 as f64) as f32;
                hline(window, y, 0.0, pw, t.last.opacity(0.6));
            }
            // The open position: a dashed line at its average price (never the last price's
            // solid violet).
            if let Some((_, avg, _, _)) = position {
                let y = self.vp.price_to_y(avg) as f32;
                let mut x = 0.0;
                while x < pw {
                    window.paint_quad(fill(rect(x, y, 6.0_f32.min(pw - x), hair), t.text_dim));
                    x += 10.0;
                }
            }
            if self.cross.is_none()
                && let Some((x, _)) = self.sync_marker
            {
                fp.vline_segments(x, 0.0, ph, |a, b| {
                    window.paint_quad(fill(rect(x - hair * 0.5, a, hair, b - a), t.crosshair));
                });
                if let Some((y, _)) = self.sync_price {
                    hline(window, y, 0.0, pw, t.crosshair);
                }
            }
            if let Some(c) = self.cross {
                // Vertical line through the wick; the horizontal one stops either side of it
                // so the translucent lines don't double up where they cross.
                let (left, right) = (c.x - hair * 0.5, c.x + hair * 0.5);
                fp.vline_segments(c.x, 0.0, ph, |a, b| {
                    window.paint_quad(fill(rect(left, a, hair, b - a), t.crosshair));
                });
                if self.cross_in_sub.is_none() {
                    hline(window, c.y, 0.0, left.max(0.0), t.crosshair);
                    hline(window, c.y, right, pw, t.crosshair);
                }
            }
        });
        if let Some(s) = self.split {
            self.draw_subpane(s, bounds, window, cx);
        }

        // Axis borders.
        window.paint_quad(fill(rect(pw, 0.0, hair, ph), t.axis_line));
        window.paint_quad(fill(rect(0.0, ph, pw, hair), t.axis_line));

        let line_h = px(LABEL_PX * 1.3);
        // The tags drawn over the price axis below; a label any of them would cover isn't
        // drawn at all (a half-covered number reads as another price).
        let last_y = self.last_trade.map(|p| last_tag_y(&self.vp, p, ph));
        // The synced price shows only with the synced time (both from the other chart).
        let sync_price = self
            .sync_price
            .filter(|_| self.cross.is_none() && self.sync_marker.is_some());
        let tags = [
            last_y.map(|y| Span::around(y, TAG_H * 0.5)),
            self.cross
                .filter(|_| self.cross_in_sub.is_none())
                .map(|c| Span::around(cross_tag_y(c.y, ph), TAG_H * 0.5)),
            sync_price.map(|(y, _)| Span::around(cross_tag_y(y, ph), TAG_H * 0.5)),
        ];
        // The time tag (own crosshair, else the synced time) on the time axis: time labels
        // it would cover aren't drawn either.
        let time_tag = match self.cross {
            Some(c) => c.time.map(|time| (c.x, time)),
            None => self.sync_marker,
        }
        .map(|(x, time)| {
            let (left, w) = self.time_tag_box(x, time, window);
            Span {
                top: left,
                bottom: left + w,
            }
        });
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
        let tf = self.timeframe;
        let calendar = lines_feed::catalog::sessions(self.instrument.id);
        for i in 0..self.scene.time_lines.len() {
            let l = self.scene.time_lines[i];
            if l.x < 0.0 {
                continue;
            }
            let kind = match l.label {
                _ if tf.is_daily() => LabelKind::Date,
                TimeLabel::Time => LabelKind::Time,
                TimeLabel::Date => LabelKind::Date,
                TimeLabel::DateTime => LabelKind::DateTime,
            };
            let text = |s: &mut FixedStr<32>| {
                if tf.is_daily() {
                    // Days and weeks are labeled by trade date, not the open's UTC clock.
                    let d = bar_date(tf, calendar, l.time);
                    let _ = write!(s, "{:02}-{:02}", d.month, d.day);
                } else {
                    let _ = l.label.write(l.time, s);
                }
            };
            let w = f32::from(self.label(kind, l.time.0, t.text_dim, window, text).width);
            // Entirely on the time axis (not under the price axis), clear of the time tag.
            let span = Span {
                top: l.x + 4.0,
                bottom: l.x + 4.0 + w,
            };
            if span.bottom > pw || !label_clear_of_tags(span, &[time_tag]) {
                continue;
            }
            let origin = point(px(ox + l.x + 4.0), px(oy + self.above_axis + 4.0));
            self.paint_label(kind, l.time.0, origin, line_h, t.text_dim, window, cx, text);
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
        if let Some(pos) = position {
            let y = self.vp.price_to_y(pos.1) as f32;
            if (0.0..=ph).contains(&y) {
                self.draw_position_tag(pos, y, (ox, oy), hair, window, cx);
            }
        }
        if !self.vp.auto_price {
            self.draw_manual_scale_indicator(bounds, window, cx);
        }
        if let Some(c) = self.cross {
            self.draw_crosshair_tags(c, bounds, window, cx);
        } else if let Some((x, time)) = self.sync_marker {
            self.draw_time_tag(x, time, bounds, window, cx);
            if let Some((y, price)) = sync_price {
                self.draw_price_tag(y, price, bounds, window, cx);
            }
        }
        if let Some(bar) = self.cross_bar {
            let partial = self.cross_partial;
            self.draw_readout(bar, partial, bounds, window, cx);
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
        let pw = f32::from(self.plot.size.width);
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
            px(oy + self.above_axis + 4.0),
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
    /// The sub-pane (`crate::subpane`) below the plot: background, the divider, the time grid
    /// (shared with the plot), the studies drawn there (cumulative delta), their scale's zero
    /// on the axis, and the crosshair's vertical line (and horizontal line when the pointer is
    /// in it). Quads only.
    fn draw_subpane(
        &mut self,
        s: crate::subpane::Split,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        let hair = 1.0 / window.scale_factor();
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let pw = f32::from(self.plot.size.width);
        let rect = |x: f32, y: f32, w: f32, h: f32| {
            Bounds::new(point(px(ox + x), px(oy + y)), size(px(w), px(h)))
        };
        window.paint_quad(fill(rect(0.0, s.sub_top, pw, s.sub_h), t.bg));
        // The divider: panel colour with a line in its middle (it is dragged).
        window.paint_quad(fill(
            rect(0.0, s.divider_top, pw, s.sub_top - s.divider_top),
            t.panel,
        ));
        window.paint_quad(fill(rect(0.0, s.divider_mid(), pw, hair), t.axis_line));
        let sub = rect(0.0, s.sub_top, pw, s.sub_h);
        let scale = self.studies.subpane_scale(s.sub_h);
        window.with_content_mask(Some(ContentMask { bounds: sub }), |window| {
            for l in &self.scene.time_lines {
                window.paint_quad(fill(rect(l.x, s.sub_top, hair, s.sub_h), t.grid));
            }
            if let Some(sc) = scale.filter(|sc| sc.contains(0.0)) {
                window.paint_quad(fill(rect(0.0, s.sub_top + sc.y(0.0), pw, hair), t.grid));
            }
            if let Some(sc) = &scale {
                self.studies
                    .paint_subpane((ox, oy + s.sub_top), sc, &self.vp, &t, window);
            }
            let x = self.cross.map(|c| c.x).or(self.sync_marker.map(|(x, _)| x));
            if let Some(x) = x {
                window.paint_quad(fill(
                    rect(x - hair * 0.5, s.sub_top, hair, s.sub_h),
                    t.crosshair,
                ));
            }
            if let Some(y) = self.cross_in_sub {
                window.paint_quad(fill(rect(0.0, s.sub_top + y, pw, hair), t.crosshair));
            }
        });
        // Its title at its own top-left, on the legend box (as the price plot's legends).
        if self.visibility.draws_labels()
            && let Some((reset, approx)) = self.studies.subpane_title()
        {
            let key = reset as i64;
            let title = move |b: &mut FixedStr<32>| {
                let _ = write!(b, "Cumulative delta · {}", reset.label());
            };
            let note = |b: &mut FixedStr<32>| {
                let _ = b.write_str("≈ from bars");
            };
            let tw = f32::from(
                self.label(LabelKind::SubTitle, key, t.text_main, window, title)
                    .width,
            );
            let nw = if approx {
                f32::from(
                    self.label(LabelKind::SubNote, 0, t.text_dim, window, note)
                        .width,
                ) + 8.0
            } else {
                0.0
            };
            let (bx, by, bh) = (8.0, s.sub_top + 4.0, LABEL_PX * 1.6);
            window.paint_quad(fill(rect(bx, by, tw + nw + 16.0, bh), t.label_bg));
            let line_h = px(LABEL_PX * 1.3);
            let ty = oy + by + bh * 0.5 - LABEL_PX * 0.65;
            self.paint_label(
                LabelKind::SubTitle,
                key,
                point(px(ox + bx + 8.0), px(ty)),
                line_h,
                t.text_main,
                window,
                cx,
                title,
            );
            if approx {
                self.paint_label(
                    LabelKind::SubNote,
                    0,
                    point(px(ox + bx + 16.0 + tw), px(ty)),
                    line_h,
                    t.text_dim,
                    window,
                    cx,
                    note,
                );
            }
        }
        // The axis beside it and the time axis below: borders; the visible range's ends and the
        // newest value.
        window.paint_quad(fill(rect(pw, s.sub_top, hair, s.sub_h), t.axis_line));
        window.paint_quad(fill(rect(0.0, s.sub_top + s.sub_h, pw, hair), t.axis_line));
        let Some(sc) = scale else { return };
        let line_h = px(LABEL_PX * 1.3);
        // Cumulative delta is a quantity: printed in the product's quantity format.
        let qty = self.instrument.qty;
        let value = |v: i64| {
            move |b: &mut FixedStr<32>| {
                let _ = qty.write(lines_core::Qty(v), b);
            }
        };
        let (top, bottom) = (
            (sc.hi - (sc.hi - sc.lo) * 0.08).round() as i64,
            (sc.lo + (sc.hi - sc.lo) * 0.08).round() as i64,
        );
        // The newest value's tag, and range labels only where it leaves them whole (as the
        // price axis: a half-covered number reads as another value).
        let newest = self.studies.subpane_last().map(|last| {
            let y = (s.sub_top + sc.y(last as f64))
                .clamp(s.sub_top + TAG_H * 0.5, s.sub_top + s.sub_h - TAG_H * 0.5);
            (last, y)
        });
        let tag = newest.map(|(_, y)| Span::around(y, TAG_H * 0.5));
        for v in [top, bottom] {
            let y = s.sub_top + sc.y(v as f64);
            if !label_clear_of_tags(Span::around(y, LABEL_PX * 0.65), &[tag]) {
                continue;
            }
            let origin = point(px(ox + pw + 8.0), px(oy + y - LABEL_PX * 0.65));
            self.paint_label(
                LabelKind::SubValue,
                v,
                origin,
                line_h,
                t.text_dim,
                window,
                cx,
                value(v),
            );
        }
        if let Some((last, y)) = newest {
            // A filled chip (inverted text colors), so it never reads as a range label.
            window.paint_quad(fill(
                rect(pw + hair, y - TAG_H * 0.5, AXIS_W, TAG_H),
                t.text_main,
            ));
            let origin = point(px(ox + pw + 8.0), px(oy + y - LABEL_PX * 0.65));
            self.paint_label(
                LabelKind::SubLast,
                last,
                origin,
                line_h,
                t.bg,
                window,
                cx,
                value(last),
            );
        }
        // The pointer in the sub-pane: its value on the axis, in the crosshair tag's style.
        if let Some(cy) = self.cross_in_sub {
            let v = sc.value(cy).round() as i64;
            let y =
                (s.sub_top + cy).clamp(s.sub_top + TAG_H * 0.5, s.sub_top + s.sub_h - TAG_H * 0.5);
            window.paint_quad(fill(
                rect(pw + hair, y - TAG_H * 0.5, AXIS_W, TAG_H),
                t.cross_tag,
            ));
            let origin = point(px(ox + pw + 8.0), px(oy + y - LABEL_PX * 0.65));
            self.paint_label(
                LabelKind::SubCross,
                v,
                origin,
                line_h,
                t.on_cross_tag,
                window,
                cx,
                value(v),
            );
        }
    }

    fn draw_crosshair_tags(
        &mut self,
        c: Crosshair,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.cross_in_sub.is_none() {
            self.draw_price_tag(c.y, c.price, bounds, window, cx);
        }
        if let Some(time) = c.time {
            self.draw_time_tag(c.x, time, bounds, window, cx);
        }
    }

    /// A crosshair price tag on the price axis at plot-local `y`: same box as the last-price
    /// tag, kept fully inside the plot's height.
    fn draw_price_tag(
        &mut self,
        y: f32,
        price: Price,
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
        let y = cross_tag_y(y, ph);
        let top = snap(f64::from(y - TAG_H * 0.5), scale);
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox + pw + hair), px(oy + top)),
                size(px(AXIS_W), px(TAG_H)),
            ),
            t.cross_tag,
        ));
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
    }

    /// Measure the axis labels' spacing (`GridSpace`) with this window's font, once.
    fn measure_grid_space(&mut self, window: &mut Window) {
        let dim = self.theme.text_dim;
        let mut w = |key: i64, text: &'static str| {
            f32::from(
                self.label(LabelKind::Indicator, key, dim, window, |s| {
                    let _ = s.write_str(text);
                })
                .width,
            ) as f64
        };
        // Widest digits; a gap of two digits' width between labels.
        let (time, date_time) = (w(1, "88:88"), w(2, "88-88 88:88"));
        let gap = time * 0.4 + 8.0;
        self.scene.space = GridSpace {
            price_px: f64::from(LABEL_PX) * 2.5,
            time_px: time + gap,
            date_time_px: date_time + gap,
        };
        self.space_measured = true;
    }

    /// The pointer at plot-local (`x`, `y`) as a drawing anchor: the bar's start and a price
    /// on the tick grid (or its O/H/L/C with the magnet).
    fn drawing_anchor(&self, x: f64, y: f64) -> Option<Anchor> {
        let d = self.store.try_read()?;
        let axis = TimeAxis::new(bars_view(&self.derived, &d));
        snap_anchor(
            &axis,
            &self.vp,
            self.instrument.price.tick,
            self.magnet(),
            x,
            y,
        )
    }

    /// Notes' text and the measure's readout, on `label_bg` boxes (inside the plot mask).
    /// Shaped text is cached: a note re-shapes only when its text changes.
    fn paint_drawing_text(&mut self, origin: (f32, f32), window: &mut Window, cx: &mut App) {
        let t = self.theme;
        let (ox, oy) = origin;
        let line_h = px(LABEL_PX * 1.3);
        let box_h = LABEL_PX * 1.3 + NOTE_PAD;
        for i in 0..self.drawings.drawn().len() {
            let d = self.drawings.drawn()[i];
            if d.tool != Tool::Note {
                continue;
            }
            let Some(text) = self.drawings.note_text(d.id) else {
                continue;
            };
            if self.note_lines.get(&d.id).is_none_or(|(s, _)| s != text) {
                let text = text.to_owned();
                let line = self.shape(&text, t.text_main, window);
                self.note_lines.insert(d.id, (text, line));
            }
            let Some((_, line)) = self.note_lines.get(&d.id) else {
                continue;
            };
            let w = f32::from(line.width) + 2.0 * NOTE_PAD;
            let (x, y) = d.p0;
            window.paint_quad(fill(
                Bounds::new(point(px(ox + x + 2.0), px(oy + y)), size(px(w), px(box_h))),
                t.label_bg,
            ));
            let _ = line.paint(
                point(px(ox + x + 2.0 + NOTE_PAD), px(oy + y + NOTE_PAD * 0.5)),
                line_h,
                TextAlign::Left,
                None,
                window,
                cx,
            );
            self.drawings.set_note_box(i, (w + 2.0, box_h));
        }
        if let Some((d, text)) = self.drawings.note_entry() {
            // The note being typed: re-shaped only when a key changed it.
            if self
                .note_entry_line
                .as_ref()
                .is_none_or(|(s, _)| s.strip_suffix('▏') != Some(text))
            {
                let shown = format!("{text}▏");
                let line = self.shape(&shown, t.text_main, window);
                self.note_entry_line = Some((shown, line));
            }
            let Some((_, line)) = &self.note_entry_line else {
                return;
            };
            let w = f32::from(line.width) + 2.0 * NOTE_PAD;
            let b = Bounds::new(
                point(px(ox + d.p0.0 + 2.0), px(oy + d.p0.1)),
                size(px(w), px(box_h)),
            );
            window.paint_quad(fill(b, t.label_bg));
            window.paint_quad(gpui::outline(b, t.accent, gpui::BorderStyle::Solid));
            let _ = line.paint(
                point(
                    px(ox + d.p0.0 + 2.0 + NOTE_PAD),
                    px(oy + d.p0.1 + NOTE_PAD * 0.5),
                ),
                line_h,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
        for i in 0..self.drawings.pinned_measures().len() {
            let (k, stats) = self.drawings.pinned_measures()[i];
            let m = self.drawings.drawn()[k];
            self.paint_measure_label(Some(m.id), m, stats, origin, window, cx);
        }
        if let Some((m, stats)) = self.drawings.measure_shown() {
            self.paint_measure_label(None, m, stats, origin, window, cx);
        }
    }

    /// A measure's readout beside its end anchor, kept inside the plot.
    fn paint_measure_label(
        &mut self,
        key: Option<u64>,
        m: lines_core::drawings::Projected,
        stats: lines_core::drawings::MeasureStats,
        (ox, oy): (f32, f32),
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        if self
            .measure_lines
            .get(&key)
            .is_none_or(|(s, _)| *s != stats)
        {
            let mut s = FixedStr::<96>::new();
            let _ = write_measure(&stats, &self.instrument.price, &mut s);
            let line = self.shape(s.as_str(), t.text_main, window);
            if self.measure_lines.len() > 64 {
                self.measure_lines.clear();
            }
            self.measure_lines.insert(key, (stats, line));
        }
        let Some((_, line)) = self.measure_lines.get(&key) else {
            return;
        };
        let box_h = LABEL_PX * 1.3 + NOTE_PAD;
        let w = f32::from(line.width) + 2.0 * NOTE_PAD;
        let pw = f32::from(self.plot.size.width);
        let x = (m.p1.0 + 8.0).min(pw - w).max(0.0);
        let y = (m.p1.1 - box_h - 4.0).max(0.0);
        window.paint_quad(fill(
            Bounds::new(point(px(ox + x), px(oy + y)), size(px(w), px(box_h))),
            t.label_bg,
        ));
        let _ = line.paint(
            point(px(ox + x + NOTE_PAD), px(oy + y + NOTE_PAD * 0.5)),
            px(LABEL_PX * 1.3),
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }

    /// Takes typed text now (symbol search, a timeframe, a note, a menu's filter): the
    /// workspace leaves plain and Alt keys to it (`commands::runs_while_typing`).
    pub fn takes_text(&self) -> bool {
        self.picker.is_open()
            || self.tf_entry.is_some()
            || self.drawings.typing()
            || self.study_menu.is_some()
    }

    /// A drawing command (`Command::Draw`) on this chart.
    pub fn drawing_command(&mut self, cmd: DrawCmd, cx: &mut Context<Self>) {
        use lines_core::drawings::Style;
        let d = &mut self.drawings;
        match cmd {
            DrawCmd::Tool(tool) => {
                self.picker.close();
                self.tf_menu = false;
                self.tf_entry = None;
                self.drawings.set_tool(tool);
            }
            DrawCmd::Undo => {
                d.undo();
            }
            DrawCmd::Redo => {
                d.redo();
            }
            DrawCmd::DeleteSelected => {
                d.delete_selected();
            }
            DrawCmd::ToggleVisible => {
                d.cancel();
                d.hidden = !d.hidden;
            }
            DrawCmd::DeleteAll => d.confirm_clear = d.count() > 0,
            DrawCmd::NextColor => d.restyle(|s| Style {
                color: (s.color + 1) % Style::COLORS,
                ..s
            }),
            DrawCmd::ToggleWidth => d.restyle(|s| Style {
                width: 3 - s.width,
                ..s
            }),
            DrawCmd::ToggleDashed => d.restyle(|s| Style {
                dashed: !s.dashed,
                ..s
            }),
            DrawCmd::PinMeasure => {
                d.pin_measure();
            }
        }
        cx.notify();
    }

    pub fn drawings_hidden(&self) -> bool {
        self.drawings.hidden
    }

    pub fn product_symbol(&self) -> &'static str {
        self.instrument.symbol
    }

    /// The banner for drawing mode, a temporary measure or the delete-all confirmation.
    fn drawing_banner(&self, t: &Theme) -> Option<AnyElement> {
        let d = &self.drawings;
        let text = if d.confirm_clear {
            let n = d.count();
            let plural = if n == 1 { "" } else { "s" };
            format!(
                "Delete all {n} drawing{plural} for {}?  Enter: delete  ·  Esc: keep  (Ctrl+Z undoes)",
                self.instrument.symbol
            )
        } else if let Some(tool) = d.tool() {
            let how = if tool.anchors() == 2 {
                "click two points (or drag)"
            } else {
                "click to place"
            };
            format!("Drawing: {}  ·  {how}  ·  Esc cancels", tool.label())
        } else if d.typing() {
            "Note: type, Enter to keep, Esc to cancel".to_owned()
        } else if d.measure_shown().is_some() {
            "Measure: Enter pins it  ·  Esc or a click clears it".to_owned()
        } else {
            return None;
        };
        Some(
            div()
                .absolute()
                .top(px(6.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .bg(t.panel)
                        .border_1()
                        .border_color(if d.confirm_clear { t.warn } else { t.accent })
                        .text_color(t.text_main)
                        .text_size(px(12.0))
                        .child(text),
                )
                .into_any_element(),
        )
    }

    /// The crosshair time tag's text: date and time, or on daily and weekly charts the trade
    /// date ("Mon 2026-11-09", "Week of 2026-11-09").
    fn time_tag_text(&self, time: UtcMillis) -> impl FnOnce(&mut FixedStr<32>) + use<> {
        let period = self.period_ms;
        let tf = self.timeframe;
        let calendar = lines_feed::catalog::sessions(self.instrument.id);
        move |s: &mut FixedStr<32>| {
            const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
            match tf {
                Timeframe::Day => {
                    let d = bar_date(tf, calendar, time);
                    let _ = write!(s, "{} {d}", DAYS[usize::from(d.weekday())]);
                }
                Timeframe::Week => {
                    let _ = write!(s, "Week of {}", bar_date(tf, calendar, time));
                }
                _ => {
                    let _ = time.write_crosshair_label(period, s);
                }
            }
        }
    }

    /// The time tag's box for `time` at plot-local `x`: (left, width), centred on `x` and
    /// kept on the time axis.
    fn time_tag_box(&mut self, x: f32, time: UtcMillis, window: &mut Window) -> (f32, f32) {
        let scale = window.scale_factor();
        let pw = f32::from(self.plot.size.width);
        let on = self.theme.on_cross_tag;
        let text = self.time_tag_text(time);
        let text_w = f32::from(
            self.label(LabelKind::CrossTime, time.0, on, window, text)
                .width,
        );
        let box_w = text_w + 12.0;
        let left = snap(f64::from((x - box_w * 0.5).min(pw - box_w).max(0.0)), scale);
        (left, box_w)
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
        let line_h = px(LABEL_PX * 1.3);
        let text = self.time_tag_text(time);
        let (left, box_w) = self.time_tag_box(x, time, window);
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox + left), px(oy + self.above_axis + hair)),
                size(px(box_w), px(AXIS_H - hair)),
            ),
            t.cross_tag,
        ));
        self.paint_label(
            LabelKind::CrossTime,
            time.0,
            point(px(ox + left + 6.0), px(oy + self.above_axis + 4.0)),
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
        partial: bool,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        if self
            .readout
            .as_ref()
            .is_none_or(|(b, p, _)| *b != bar || *p != partial)
        {
            let mut s = FixedStr::<128>::new();
            let _ = write_ohlcv(&bar, &self.instrument.price, self.instrument.qty, &mut s);
            if partial {
                // Cut short by a session break (the close, or a holiday halt).
                let _ = s.write_str("  partial");
            }
            let line = self.shape(s.as_str(), t.text_main, window);
            self.readout = Some((bar, partial, line));
        }
        let Some((_, _, line)) = &self.readout else {
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

    /// The chart's header. `account`: its trading account chip (with the PAPER badge), shown
    /// with the contract and the timeframe chip on the left; the right end is the hover
    /// controls' gutter.
    fn header(&self, account: Option<gpui::AnyElement>, cx: &mut Context<Self>) -> gpui::Div {
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
                .pr(px(pane::controls_gutter(crate::layout::PaneKind::Chart)))
                .py_1()
                .bg(t.panel)
                .border_b_1()
                .border_color(t.axis_line)
                .text_sm()
        };
        let chip = pane::link_chip(self.common.link, &t);
        if let Some(title) = pane::unavailable_title(&self.feed) {
            return bar().child(
                pane::header_content().child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .items_center()
                        .text_color(t.text_dim)
                        .children(chip)
                        .child(title),
                ),
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
        let list = self.timeframe_list(&t, cx);
        let timeframe = crate::dropdown::dropdown(self.timeframe_chip(&t, cx), list);
        bar().child(
            pane::header_content()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .items_center()
                        .flex_shrink_0()
                        .children(chip)
                        .child(div().text_color(t.text_main).child(left))
                        .child(timeframe)
                        .children(account)
                        .children(quotes)
                        .children(pane::stale_badge(&self.store, &t)),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(t.text_dim)
                        .child(right),
                ),
        )
    }
}

// ---- timeframe controls (roadmap 9) ---------------------------------------------------

impl ChartView {
    /// The header's timeframe chip ("5m ▾"): opens the list.
    fn timeframe_chip(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let accent = t.accent;
        div()
            .id("timeframe")
            .px_1()
            .rounded_sm()
            .border_1()
            .border_color(if self.tf_menu { accent } else { t.axis_line })
            .text_color(t.text_main)
            .cursor_pointer()
            .hover(move |s| s.border_color(accent))
            .child(pane::timeframe_chip_text(self.timeframe))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, _, _, cx| {
                    cx.stop_propagation();
                    v.toggle_timeframe_menu(cx);
                }),
            )
    }

    /// The timeframe list, while open: under the chip (`dropdown`).
    fn timeframe_list(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.tf_menu {
            return None;
        }
        let accent = t.accent;
        let mut list = div()
            .id("timeframe-list")
            .occlude()
            .min_w(px(150.0))
            .flex()
            .flex_col()
            .py_1()
            .bg(t.panel)
            .border_1()
            .border_color(accent)
            .rounded_sm()
            .text_size(px(12.0));
        for (i, tf) in Timeframe::ALL.into_iter().enumerate() {
            let on = tf == self.timeframe;
            list = list.child(
                div()
                    .id(("timeframe", i as u64))
                    .px_2()
                    .py(px(2.0))
                    .cursor_pointer()
                    .text_color(t.text_main)
                    .hover(move |s| s.bg(accent.opacity(0.18)))
                    .font_weight(if on {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .child(if on {
                        format!("✓ {tf}")
                    } else {
                        format!("   {tf}")
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |v, _, _, cx| {
                            cx.stop_propagation();
                            v.set_timeframe(tf, cx);
                        }),
                    ),
            );
        }
        Some(
            list.child(
                div()
                    .px_2()
                    .pt_1()
                    .text_color(t.text_dim)
                    .text_size(px(11.0))
                    .child("Or type 5, 1h, 1d … + Enter"),
            )
            .into_any_element(),
        )
    }

    /// The timeframe being typed, bottom left of the plot: what it will switch to.
    fn timeframe_entry(&self, t: &Theme) -> Option<AnyElement> {
        let entry = self.tf_entry.as_deref()?;
        let (hint, ok) = match Timeframe::parse(entry) {
            Some(tf) => (format!("Enter: {tf}"), true),
            None => (
                "1 2 3 5 10 15 30 (m)  ·  1h 2h 4h  ·  1d  ·  1w".to_owned(),
                false,
            ),
        };
        Some(
            div()
                .absolute()
                .bottom(px(AXIS_H + 8.0))
                .left(px(8.0))
                .flex()
                .flex_row()
                .gap_2()
                .items_center()
                .px_2()
                .py_1()
                .bg(t.panel)
                .border_1()
                .border_color(t.accent)
                .rounded_sm()
                .text_size(px(12.0))
                .child(div().text_color(t.text_dim).child("Timeframe"))
                .child(
                    div()
                        .text_color(t.text_main)
                        .font_weight(FontWeight::BOLD)
                        .child(format!("{entry}▏")),
                )
                .child(
                    div()
                        .text_color(if ok { t.text_main } else { t.text_dim })
                        .child(hint),
                )
                .child(div().text_color(t.text_dim).child("Esc cancels"))
                .into_any_element(),
        )
    }
}

impl EventEmitter<FeedChanged> for ChartView {}

impl EventEmitter<CrosshairTime> for ChartView {}

impl ChartView {
    /// Smoothed paint time in µs (diagnostics).
    pub fn paint_us(&self) -> f64 {
        self.paint_time_ewma_us
    }
}

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
            magnet: self.magnet(),
            hide_drawings: self.drawings.hidden,
            timeframe: crate::wsdoc::TimeframeDoc(self.timeframe),
            footprint: self.footprint.config(),
            subpane: (self.subpane_ratio != crate::subpane::DEFAULT_RATIO)
                .then_some(self.subpane_ratio),
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
        self.drawings
            .set_data(crate::drawings_store::for_product(&self.instrument, cx));
        self.note_lines.clear();
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
        // Drawings changed on any chart of this product (shown here).
        let rates = std::mem::take(&mut self.rates_changed)
            | self.trade.changed()
            | (self.drawings.changed() && !self.drawings.hidden);
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
    footprint: &mut crate::footprint::FootprintLayer,
    layers: &mut [Box<dyn ChartLayer>],
    studies: &mut crate::studies::Studies,
    levels: &mut LevelsLayer,
    d: &ChartData,
    bars: BarsView<'_>,
    vp: &Viewport,
    max_columns: usize,
) {
    if !visibility.draws_studies() {
        return;
    }
    profile.prepare(d, bars, vp);
    footprint.prepare(d, bars, vp);
    for l in layers {
        l.prepare(d, bars, vp);
    }
    studies.prepare(bars, vp, max_columns);
    let last_y = d.last_trade.map(|p| vp.price_to_y(p.0 as f64) as f32);
    levels.prepare(d, bars, vp, last_y);
}

/// The bars a chart shows: its timeframe's, or the store's 1-minute bars.
fn bars_view<'a>(derived: &'a Option<Derived>, d: &'a ChartData) -> BarsView<'a> {
    match derived {
        Some(dv) => dv.view(),
        None => BarsView::fixed(d.series.bars(), d.series.period_ms()),
    }
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

/// The OHLCV readout box's bottom (it sits 6 px from the top, `LABEL_PX × 1.6` tall), plus
/// a gap: the footprint keeps its numbers below it.
const READOUT_BOTTOM: f32 = 6.0 + LABEL_PX * 1.6 + 4.0;

/// Widest the level-source legend grows (logical px); longer text ends in an ellipsis.
const LEGEND_MAX_W: f32 = 360.0;

/// Key and mouse hints for a focused chart, in the window's status line.
pub const HINTS_SHORT: &str = "Chart: drag pan  ·  wheel zoom  ·  Ctrl+wheel or drag an axis: scale  ·  Home latest  ·  type a symbol: switch  ·  type 5, 15, 1h, 1d + Enter: timeframe  ·  Alt+H/T/Z draw  ·  Ctrl+I studies  ·  Alt+V profile";

impl Render for ChartView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let entity = cx.entity();
        // The workspace refreshes every window when this preference changes.
        self.perf_stats = cx
            .try_global::<crate::workspace::Workspace>()
            .is_some_and(|w| w.show_perf_stats);
        // This frame's level-source snapshot: the legend below and the levels painted later
        // in this frame both describe it.
        let market_time = self.store.try_read().and_then(|d| d.market_time);
        self.levels.snapshot(market_time);
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
                .child(self.header(None, cx))
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
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .child(self.header(account, cx))
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
                    .when(self.drawings.active(), |d| d.cursor_crosshair())
                    .when(
                        !self.drawings.active() && self.drawings.hover_hit().is_some(),
                        |d| d.cursor_pointer(),
                    )
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
                    .when_some(self.timeframe_entry(&t), |d, e| d.child(e))
                    .when_some(self.drawing_banner(&t), |d, b| d.child(b)),
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
                &mut crate::footprint::FootprintLayer::new(Default::default()),
                &mut [],
                &mut studies,
                &mut LevelsLayer::new(LevelsConfig::default()),
                &d,
                BarsView::fixed(d.series.bars(), d.series.period_ms()),
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
            &mut crate::footprint::FootprintLayer::new(Default::default()),
            &mut [],
            &mut studies,
            &mut LevelsLayer::new(LevelsConfig::default()),
            &d,
            BarsView::fixed(d.series.bars(), d.series.period_ms()),
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
