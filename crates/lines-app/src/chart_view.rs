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
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, ContentMask, Context, EventEmitter, FocusHandle, Font, FontWeight,
    Hsla, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseExitEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollEvent, ShapedLine, SharedString,
    Styled, TextAlign, TextRun, Window, canvas, div, fill, point, prelude::*, px, size,
};
use lines_core::TickReset;
use lines_core::display_tz::{self, DisplayTz};
use lines_core::drawings::{Anchor, TimeAxis, Tool, snap_anchor, write_measure};
use lines_core::geometry::{
    Crosshair, CrosshairMode, GridSpace, Span, TimeLabel, crosshair, label_clear_of_tags, slot_x,
    snap, write_ohlcv,
};
use lines_core::timeframe::{BarSource, bar_date};
use lines_core::viewport::{MAX_BAR_PX, MIN_BAR_PX, ScaleDrag, ScaleMode};
use lines_core::{
    Bar, BarsView, Derived, FixedStr, Price, Product, ProductId, Scene, Timeframe, UtcMillis,
    Viewport,
};
use lines_feed::ChartData;
use lines_feed::catalog::Target;
use lines_feed::store::{ChartStore, FeedStatus};
use lines_feed::ticks::{TickKey, TickLease};

use crate::chart_draw::{self, ChartDrawings};
use crate::commands::{Command, DrawCmd};
use crate::feeds::PaneFeed;
use crate::levels_layer::{LevelsConfig, LevelsLayer};
use crate::pane::{self, FeedChanged, Pane, PaneCommon};
use crate::picker_view::{PickerKey, PickerUi};
use crate::pnl::PnlDisplay;
use crate::popover::{self, Kind};
use crate::profile::{ProfileConfig, VolumeProfileLayer};
use crate::study_menu::{
    Item, MenuCmd, MenuInput, SourceInfo, StudyAction, StudyMenu, StudyRef, StudySet, anchor_at,
    save_template,
};
use crate::study_style::StudyStyles;
use crate::theme::Theme;
use crate::wsdoc::{ChartState, PaneDoc, StudyVisibility};

pub(crate) const AXIS_W_100: f32 = 78.0;

/// [`AXIS_W_100`] at the UI zoom (`crate::zoom`).
#[inline]
pub(crate) fn axis_w() -> f32 {
    crate::zoom::f(AXIS_W_100)
}
/// Where a screenshot opens the style editor: drawing id, tab, and the focus (row, col).
pub type StyleEditorAt = (u64, usize, Option<(usize, usize)>);
const AXIS_H_100: f32 = 24.0;
/// [`AXIS_H_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn axis_h() -> f32 {
    crate::zoom::f(AXIS_H_100)
}
const LABEL_PX_100: f32 = 12.0;
/// [`LABEL_PX_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn label_px() -> f32 {
    crate::zoom::f(LABEL_PX_100)
}
const MAX_CACHED_LABELS: usize = 4096;

/// The space between the corner's two words, and their margin from its edges (at 100 %).
const CORNER_GAP_100: f32 = 8.0;
const CORNER_MARGIN_100: f32 = 4.0;

/// The corner under the price axis holds two words: "manual" (the price scale is frozen) and
/// the display zone ("ET"). Where each goes, as offsets from the corner's left edge (`None`: not
/// drawn): side by side, "manual" first, when both fit inside its margins; else "manual" alone
/// (the frozen scale matters more; the crosshair's tag still names the zone). The two never
/// overlap at any width, zoom or DPI; in a pane narrower than "manual" plus its margins, the
/// word starts at the margin and is clipped by the pane, as before.
pub(crate) fn corner_layout(
    corner_w: f32,
    manual_w: Option<f32>,
    zone_w: Option<f32>,
    gap: f32,
    margin: f32,
) -> (Option<f32>, Option<f32>) {
    let centred = |w: f32| ((corner_w - w) * 0.5).max(margin);
    match (manual_w, zone_w) {
        (None, None) => (None, None),
        (Some(m), None) => (Some(centred(m)), None),
        (None, Some(z)) => (None, Some(centred(z))),
        (Some(m), Some(z)) => {
            let both = m + gap + z;
            if both + 2.0 * margin <= corner_w {
                let left = (corner_w - both) * 0.5;
                (Some(left), Some(left + m + gap))
            } else {
                (Some(centred(m)), None)
            }
        }
    }
}
/// Colours in signatures (theme tokens only).
type Colour = gpui::Hsla;

/// Height of the last-price and crosshair price tags on the price axis.
const TAG_H_100: f32 = LABEL_PX_100 * 1.5;
/// [`TAG_H_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn tag_h() -> f32 {
    crate::zoom::f(TAG_H_100)
}
/// An order tag's height at the UI zoom (`order_tags::Metrics::for_ui_zoom`).
fn tag_h_now() -> f32 {
    lines_core::geometry::order_tags::Metrics::for_ui_zoom(crate::zoom::get()).tag_h
}
/// Shown in the price-axis corner while price auto-scale is off.
const MANUAL_LABEL: &str = "manual";

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
enum LabelKind {
    Price,
    Time,
    /// A time gridline's `HH:MM:SS` (steps under a minute; its own kind for the label cache).
    TimeSeconds,
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
    /// A part of Volume's title readout (`VOL_*`), keyed by its value.
    Volume(u8),
    /// An external series pane's title: its name, a line's label and value, its state.
    SeriesName,
    SeriesValue(u8),
    SeriesState,
    /// A lower pane's close button (×).
    PaneClose,
    CrossTime,
    Indicator,
    /// A drawing's price tag on the axis, its text in the drawing's color (when AA on the
    /// axis) or the theme's text color (`Theme::drawing_tag`). Keyed by that color (`0xRRGGBB`),
    /// as the shaped text carries it.
    DrawingTag(u32),
    /// "tick bars from 14:32" at a tick chart's first bar (keyed by that time).
    TickFrom,
    /// The display zone ("ET", "JST") in the corner under the price axis (keyed by its
    /// offset: an abbreviation changes with it).
    Zone,
}

/// A lower pane's × (logical px square): its hit target.
const CLOSE_BOX: f32 = 20.0;

/// Volume's title parts (`LabelKind::Volume`): its name alone, "Volume N", "avg N", "buy N",
/// "sell N", "no side N".
const VOL_NAME: u8 = 0;
const VOL_TOTAL: u8 = 1;
const VOL_AVG: u8 = 2;
const VOL_BUY: u8 = 3;
const VOL_SELL: u8 = 4;
const VOL_NO_SIDE: u8 = 5;

/// Drawing price tags on the axis considered per frame (labels they cover aren't drawn).
const MAX_DRAWING_TAGS: usize = 8;
/// A drawing tag's height: shorter than the last-price tag (`tag_h()`), never mistaken for it.
const DRAWING_TAG_H: f32 = 16.0;
/// A drawing tag starts this far into the axis, joined to the plot by its notch.
const DRAWING_TAG_INSET: f32 = 4.0;

/// Behind a drawing's note text: the box's padding (logical px).
const NOTE_PAD: f32 = 6.0;

/// Where a pointer event landed.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Region {
    Plot,
    /// The price axis column, including the corner below it.
    PriceAxis,
    TimeAxis,
    /// A lower pane below the plot (`crate::subpane`): drags pan time only.
    Sub,
    /// The divider above lower pane `k`.
    Divider(usize),
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

/// A Shift+drag from one plot-local point to another (`--shift-drag`).
pub type ShiftDrag = ((f32, f32), (f32, f32));

/// Deterministic input for screenshot mode (decision 0009), standing in for the mouse.
#[derive(Clone, Debug, Default)]
pub struct Fixture {
    /// Fixed pointer position, plot-local logical px.
    pub cursor: Option<(f32, f32)>,
    /// The position line dragged to this plot-local y (screenshots: `--drag-position Y`):
    /// the exit it would set is previewed, never placed.
    pub drag_position: Option<f32>,
    /// A Shift+drag from the first plot-local point to the second, released (screenshots:
    /// `--shift-drag X0,Y0:X1,Y1`): the measure it leaves, through the press path's own calls.
    pub shift_drag: Option<ShiftDrag>,
    /// Then a drag inside that measure from the first point to the second, released
    /// (`--move-measure X0,Y0:X1,Y1`): the measure moved.
    pub move_measure: Option<ShiftDrag>,
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
    /// Start with the order strip shown (screenshots: `--order-strip`).
    pub order_strip: bool,
    /// Start with a strip limit's price pick armed (screenshots: `--strip-pick buy|sell`).
    pub strip_pick: Option<crate::chart_trade::StripOrder>,
    /// Initial timeframe (`--timeframe`). One saved with the pane takes precedence.
    pub timeframe: Timeframe,
    /// Start with the timeframe list open (screenshots: `--timeframe-menu`).
    pub timeframe_menu: bool,
    /// Open the first group tag's list (screenshots:
    /// `--order-group-menu`).
    pub order_group_menu: bool,
    /// Open the style editor on this drawing (screenshots: `--style-editor
    /// ID[:TAB[:COL[:ROW]]]`), on tab `TAB` (0 = first), the focus on (row, col).
    pub style_editor: Option<StyleEditorAt>,
    /// Tick bars run across sessions (`--tick-continuous`). A mode saved with the pane wins.
    pub tick_continuous: bool,
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
    /// Lower pane `k`'s divider: where the drag started (window y), the shown panes' count
    /// and their ratios then.
    Divider {
        y: f32,
        k: usize,
        n: usize,
        ratios: [f32; crate::subpane::MAX_PANES],
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
    /// The studies menu as it was when last closed without finishing (an editor open, a
    /// template name half typed): reopening restores it (the popover rule).
    study_draft: Option<StudyMenu>,
    /// Waiting for a click on a bar to anchor a VWAP: `Some(None)` adds a new one.
    anchoring: Option<Option<usize>>,
    /// The style editor open on a study's parts (the studies menu's "Style…"): Esc steps
    /// back to the menu.
    study_style: Option<(StudyRef, crate::style_editor::StyleEditor)>,
    /// Study styles a newer build saved for studies this one doesn't have (kept, written
    /// back).
    styles_extra: serde_json::Map<String, serde_json::Value>,
    /// Level styles this chart (or a template, the chart defaults) saved per source before
    /// they were app-wide: promoted to settings `level_styles` at the next render
    /// (`study_style::promote_level_styles`), then dropped.
    pending_levels: crate::study_style::PendingLevels,
    /// `study_style::level_styles_generation()` this chart last read the app-wide styles at.
    level_styles_gen: u64,
    /// The level labels' gutter (plot px at the right, `lines_core::levels::step_gutter`): 0
    /// while no level label shows.
    gutter: f32,
    /// A press on the plot while a strip limit's pick is armed: where (window px), and the
    /// price there (the crosshair's, computed at the press). Sent on release if still a click.
    pick_press: Option<(Point<Pixels>, PickPrice)>,
    /// External levels (decision 0022), saved with the pane.
    levels: LevelsLayer,
    /// `levels.revision()` last drawn.
    seen_levels: u64,
    /// The 5 s step of the wall clock last seen while a series pane was live.
    series_tick: i64,
    /// The footprint chart mode (roadmap 10e), saved with the pane. Kept typed (it replaces
    /// candles bar by bar).
    footprint: crate::footprint::FootprintLayer,
    /// Each lower pane's share of the height (`crate::subpane`), top to bottom, saved with the
    /// pane.
    subpane_ratios: [f32; crate::subpane::MAX_PANES],
    /// This frame's plot and lower panes (`None`: no lower pane).
    split: Option<crate::subpane::Stack>,
    /// Each lower pane's × as last drawn (plot-local top-left; `CLOSE_BOX` square).
    lower_close: [Option<(f32, f32)>; crate::subpane::MAX_PANES],
    /// The lower pane whose × was pressed (removed on a release still on it).
    pane_closing: Option<usize>,
    /// Height above the time axis this frame (the plot plus any sub-pane).
    above_axis: f32,
    /// The pointer is in lower pane `k`, this far below its top: the crosshair's vertical
    /// line and time follow it; the horizontal line is in that pane, with no price.
    cross_in_sub: Option<(usize, f32)>,
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
    /// The order strip (phase 2b) is shown (saved).
    order_strip: bool,
    /// Shows no orders and sends none (`set_view_only`).
    view_only: bool,
    /// Axis label spacing has been measured with this window's font.
    space_measured: bool,
    /// The time this chart last reported under its own crosshair.
    reported: (Option<UtcMillis>, Option<Price>),
    /// Screenshot fixture: price zoom to apply once, after the first auto-fit.
    pending_price_zoom: Option<f64>,
    /// Screenshot fixture: open the first group tag's list once tags are laid out.
    pending_group_menu: bool,
    /// The plot a group's list was last placed for (a resize re-renders it, placed anew).
    group_plot: Bounds<Pixels>,
    /// Pointer position in plot-local logical px, while it is over the plot.
    hover: Option<(f32, f32)>,
    /// This frame's crosshair (from `hover`), and a copy of the bar under it.
    cross: Option<Crosshair>,
    cross_bar: Option<Bar>,
    period_ms: i64,
    /// OHLCV readout (and whether the bar is partial; a tick bar's last trade and count),
    /// shaped once per distinct value.
    readout: Option<(Bar, bool, TickInfo, ShapedLine)>,
    /// Shaped labels and the color each was shaped in: a label asked for in another color (a
    /// theme switch, a restyled study) is shaped again.
    labels: HashMap<(LabelKind, i64), (Hsla, ShapedLine)>,
    /// The zone label's width this frame, when the corner has it (`corner_layout`: "manual"
    /// is placed beside it, or it yields).
    corner_zone_w: Option<f32>,
    /// The display time zone the axis, the time tag and the readout are written in, and its
    /// epoch (`lines_core::display_tz`): a change clears the cached labels.
    zone: DisplayTz,
    zone_epoch: u64,
    font: Option<Font>,
    rates: Rates,
    paint_time_ewma_us: f64,
    /// Show `rates` in the header (workspace preference, read at render).
    perf_stats: bool,
    /// The rates changed since the last paint (redraw once a second while shown).
    rates_changed: bool,
    /// Show the latency HUD in the header (workspace preference, read at render; roadmap 6b).
    latency_hud: bool,
    /// Receive → paint for this chart (recorded only while the HUD is shown).
    paint_latency: crate::latency_hud::PaintLatency,
    /// The HUD's text and clock status, rebuilt at most once a second (at render).
    hud_text: String,
    hud_clock: Option<(String, bool)>,
    hud_at: Option<Instant>,
    /// The HUD text was built in its compact form (a narrow chart).
    hud_compact: bool,
    /// The chart's account (chart trading is roadmap 8; Flatten/Cancel All hotkeys work now).
    trade: crate::trade_bar::PaneTrade,
    /// The position tag's text (glyphs shaped once).
    pos_glyphs: crate::text::Glyphs,
    /// The chart's orders this frame: lines, tags, edge markers, handles, drags.
    pub(crate) ct: crate::chart_trade::ChartTradeState,
    /// Strategy markers (decision 0025): entries, exits and skipped signals on this contract.
    marks: crate::strategy_marks::StrategyMarksLayer,
    strategies: Option<Arc<dyn crate::strategy_panel::StrategySource>>,
    trading_cache: Option<Arc<crate::trading::TradeCache>>,
    /// Screenshot fixture: the position line held dragged to this plot-local y.
    drag_position: Option<f32>,
    /// Screenshot fixture: a Shift+drag, replayed each frame.
    shift_drag: Option<ShiftDrag>,
    /// Screenshot fixture: a drag moving that measure, replayed each frame.
    move_measure: Option<ShiftDrag>,
    /// The chart's timeframe (roadmap 9), saved with the pane. Never shared through link
    /// groups: they share the market only.
    timeframe: Timeframe,
    /// Its bars when it isn't 1 minute: derived from the store's 1-minute bars, updated
    /// incrementally in paint.
    derived: Option<Derived>,
    /// Its bars on a tick timeframe (roadmap 9b): the store's shared builder, held while this
    /// chart shows it.
    ticks: Option<TickLease>,
    /// Where this chart's tick bars restart their count (saved with the pane).
    tick_reset: TickReset,
    /// The hovered tick bar: its last trade's time, trade count and state (crosshair readout).
    cross_tick: TickInfo,
    /// Per drawn candle on a tick chart (aligned with `scene.candles`): `MARK_PARTIAL` (closed
    /// short by a reset, or still forming), `MARK_SESSION` (takes its session's first trade).
    /// Reserved with the scene (no allocation while painting).
    tick_marks: Vec<u8>,
    /// "tick bars from HH:MM": the first bar's time and plot-local x, when it is on screen.
    tick_from: Option<(UtcMillis, f32)>,
    /// After a timeframe change: put the bar holding this time at this plot-local x.
    tf_anchor: Option<(UtcMillis, f64)>,
    /// The timeframe list (header chip, pane menu), while open.
    tf_menu: bool,
    /// A timeframe being typed (digits, then a unit), while typing.
    tf_entry: Option<String>,
    /// What the last right click was on (an order's tag, a drawing): the pane menu opened by
    /// the same click is that object's (`crate::pane_menu`). Taken by the menu.
    menu_object: Option<crate::pane_menu::MenuObject>,
    /// The price of the last right click on the plot (snapped as the crosshair draws it), for
    /// the menu's order section; `None` off the plot, on an object, or with no bars there.
    menu_price: Option<Price>,
    /// The account the open menu's order rows were offered on.
    menu_account: Option<lines_trading::AccountId>,
    /// The hovered bar was cut short by a session break.
    cross_partial: bool,
    /// The product's drawings on this chart (roadmap 11).
    drawings: ChartDrawings,
    /// Note texts shaped (by drawing id; re-shaped only when the text changes).
    note_lines: HashMap<u64, (String, lines_core::style::TextStyle, ShapedLine)>,
    /// The note being typed, shaped once per key.
    note_entry_line: Option<(String, ShapedLine)>,
    /// Measure readouts (`None`: the temporary one; else a pinned measure's id), shaped once
    /// per distinct reading.
    /// The per-tool drawing styles last read from the settings (new defaults saved on any
    /// chart are picked up when a tool is next chosen).
    saved_styles: crate::chart_draw::ToolStyles,
    /// The area the style editor was last placed in (the chart's width and the height above
    /// its time axis; a new size places it again).
    editor_pane: (f32, f32),
    /// The style editor body's scroll state: its scrollbar, and the focus it last scrolled to
    /// show (a key that moves the focus scrolls again; a wheel scroll alone doesn't snap back).
    editor_scroll: crate::style_editor_view::EditorScroll,
    /// The UI zoom generation the shaped text below was made at (`crate::zoom`).
    zoom_generation: u32,
    measure_lines: HashMap<
        Option<u64>,
        (
            (
                lines_core::drawings::MeasureStats,
                lines_core::drawings::MeasureParts,
            ),
            ShapedLine,
        ),
    >,
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
    fn trade_product(&self) -> &Product {
        &self.instrument
    }
    fn trade_focus(&self) -> &FocusHandle {
        &self.focus
    }
    fn shows_size(&self) -> bool {
        self.strip_box().is_some() && self.keys_free()
    }
    fn keys_free(&self) -> bool {
        !self.picker.is_open() && self.study_menu.is_none() && !self.drawings.editing()
    }
    fn resize_drawn(&self, order: lines_trading::OrderId) -> bool {
        self.ct.group_of(order.0).is_none()
            && self.ct.tags[..self.ct.n_tags]
                .iter()
                .any(|b| b.id == order.0 && b.left >= crate::chart_trade::resize_w(b))
    }
    fn send_resize(
        &mut self,
        order: lines_trading::OrderId,
        step: crate::order_resize::SizeStep,
        one_leg: bool,
        cx: &mut Context<Self>,
    ) {
        let issued = std::time::Instant::now();
        let plan = crate::trade_bar::resize_plan(order, step, one_leg, cx);
        let (plan, Some(t)) = (plan, crate::trading::Trading::try_global(cx)) else {
            return;
        };
        match plan {
            Err(why) => self.trade.flash(why),
            // More quantity is more exposure: only through a linked DOM's switch, like an order
            // from the strip. Each order's account and contract are the chart's.
            Ok(p) if p.grows && self.store.stale_for().is_some() => {
                self.trade.flash(crate::trade_bar::FEED_STALE);
            }
            Ok(p) if p.grows => {
                let (Some(account), Some(contract)) = (
                    self.trade.account.clone(),
                    crate::trade_bar::contract_of(&self.store),
                ) else {
                    return;
                };
                for leg in p.commands {
                    cx.emit(ChartOrder {
                        command: leg.command,
                        refused: crate::chart_trade::ENABLE_TO_GROW,
                        account: account.clone(),
                        contract,
                        link: self.common.link,
                        issued,
                        resize: Some((leg.order, leg.price, leg.qty)),
                    });
                }
            }
            Ok(p) => {
                for leg in p.commands {
                    let command = t.submit(lines_trading::Source::Chart, issued, leg.command);
                    self.track_resize(leg.order, leg.price, leg.qty, command);
                }
            }
        }
        cx.notify();
    }
    fn trade_unavailable(&self) -> bool {
        self.feed.unavailable.is_some()
    }
}

impl ChartView {
    /// A new look (the Settings window's appearance).
    pub(crate) fn set_theme(&mut self, theme: Theme) {
        if theme != self.theme {
            // Text is shaped in its color: what was shaped in the old theme's goes (labels
            // are also keyed by color; these caches are by text only).
            self.readout = None;
            self.note_lines.clear();
            self.note_entry_line = None;
            self.measure_lines.clear();
            self.profile.theme_changed();
            self.levels.theme_changed();
        }
        self.theme = theme;
    }
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
        let mut drawings = ChartDrawings::new(crate::drawings_store::for_product(&instrument, cx));
        let saved_styles = saved_tool_styles(cx);
        drawings.defaults = saved_styles;
        let fixture_style_editor = fixture.style_editor;

        let mut view = ChartView {
            study_menu: None,
            study_style: None,
            styles_extra: serde_json::Map::new(),
            pending_levels: Default::default(),
            level_styles_gen: 0,
            gutter: 0.0,
            study_draft: None,
            anchoring: None,
            pick_press: None,
            feed,
            common: PaneCommon::default(),
            profile: VolumeProfileLayer::new(fixture.profile),
            // Scheduled economic releases (roadmap 8b) on every chart; none where no release moves the product.
            layers: vec![Box::new(crate::econ_layer::EconLayer::new())],
            studies,
            seen_studies: u64::MAX,
            levels,
            seen_levels: u64::MAX,
            series_tick: 0,
            footprint: crate::footprint::FootprintLayer::new(Default::default()),
            subpane_ratios: std::array::from_fn(crate::subpane::default_ratio),
            split: None,
            lower_close: [None; crate::subpane::MAX_PANES],
            pane_closing: None,
            above_axis: 0.0,
            cross_in_sub: None,
            fp_seen: Default::default(),
            legend_band: std::cell::Cell::new((8.0 + LEGEND_MAX_W, readout_bottom())),
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
            order_strip: fixture.order_strip || fixture.strip_pick.is_some(),
            view_only: false,
            space_measured: false,
            reported: (None, None),
            hover: fixture.cursor,
            pending_price_zoom: fixture.price_zoom,
            pending_group_menu: fixture.order_group_menu,
            group_plot: Bounds::default(),
            cross: None,
            cross_bar: None,
            period_ms: 60_000,
            readout: None,
            labels: HashMap::with_capacity(MAX_CACHED_LABELS),
            corner_zone_w: None,
            zone: display_tz::current(),
            zone_epoch: display_tz::epoch(),
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
            latency_hud: false,
            paint_latency: Default::default(),
            hud_text: String::new(),
            hud_clock: None,
            hud_at: None,
            hud_compact: false,
            trade,
            // Order and position tags zoom with their boxes (`order_tags::Metrics`).
            pos_glyphs: crate::text::Glyphs::new(LABEL_PX_100),
            ct: crate::chart_trade::ChartTradeState {
                strip_pick: fixture.strip_pick,
                ..Default::default()
            },
            marks: crate::strategy_marks::StrategyMarksLayer::default(),
            strategies: cx
                .try_global::<crate::strategy_panel::Strategies>()
                .map(|s| s.0.clone()),
            trading_cache: crate::trading::Trading::try_global(cx).map(|t| t.cache.clone()),
            drag_position: fixture.drag_position,
            shift_drag: fixture.shift_drag,
            move_measure: fixture.move_measure,
            timeframe: Timeframe::ONE_MINUTE,
            derived: None,
            ticks: None,
            tick_reset: TickReset::Session,
            cross_tick: None,
            tick_marks: Vec::new(),
            tick_from: None,
            tf_anchor: None,
            tf_menu: fixture.timeframe_menu,
            tf_entry: None,
            menu_object: None,
            menu_price: None,
            menu_account: None,
            cross_partial: false,
            drawings,
            saved_styles,
            editor_pane: (0.0, 0.0),
            editor_scroll: Default::default(),
            note_lines: HashMap::new(),
            note_entry_line: None,
            zoom_generation: crate::zoom::generation(),
            measure_lines: HashMap::new(),
        };
        if fixture.tick_continuous {
            view.tick_reset = TickReset::Continuous;
        }
        view.apply_timeframe(timeframe);
        if let Some(set) = fixture_studies {
            view.set_study_set(set);
        }
        if let Some(spec) = fixture_menu {
            view.open_menu_at(&spec, cx);
        }
        if let Some((id, tab, focus)) = fixture_style_editor
            && view.open_style_editor(id, cx)
        {
            view.drawings.edit_style(|e| {
                e.select_tab(tab);
                if let Some((row, col)) = focus {
                    for _ in 0..row {
                        e.key("down", false, None);
                    }
                    e.key("home", false, None);
                    for _ in 0..col {
                        e.key("right", false, None);
                    }
                }
                crate::style_editor::Outcome::Nothing
            });
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
            styles: self.study_styles(),
        }
    }

    /// The chart-wide studies' styles (profile, footprint, level sources).
    fn study_styles(&self) -> StudyStyles {
        StudyStyles {
            profile: self.profile.style().clone(),
            footprint: self.footprint.style().clone(),
            // App-wide (settings `level_styles`): a chart no longer saves its own, except an
            // older one not promoted yet (kept, never lost).
            levels: self.pending_levels.saved(),
            extra: self.styles_extra.clone(),
        }
    }

    fn set_study_styles(&mut self, s: &StudyStyles) {
        self.profile.set_style(&s.profile);
        self.footprint.set_style(&s.footprint);
        // A chart's own level styles (an older file, a template): promoted app-wide once
        // (`promote_levels`, on restore; else at the next render).
        self.pending_levels.restore(&s.levels);
        self.styles_extra = s.extra.clone();
    }

    /// Replace this chart's studies (a template, the defaults, a menu change).
    pub fn set_study_set(&mut self, s: StudySet) {
        // A restyle keeps the studies' engine nodes; anything else builds them anew.
        if s.indicators != self.studies.configs() && !self.studies.restyle(&s.indicators) {
            self.set_indicator_configs(s.indicators);
        }
        self.set_study_styles(&s.styles);
        self.profile.set_config(s.profile);
        self.levels.config = s.levels;
        self.footprint.set_config(s.footprint);
        self.seen_levels = u64::MAX;
        self.seen_studies = u64::MAX;
    }

    /// A new chart (not restored from a workspace file): the saved defaults, if any.
    pub fn apply_new_chart_defaults(&mut self, cx: &mut Context<Self>) {
        let s = crate::picker_view::settings(cx);
        let set = s.chart_defaults.clone();
        // Settings › Charts: the timeframe a new chart starts with.
        let tf = s
            .new_panes
            .timeframe
            .as_deref()
            .and_then(lines_core::Timeframe::parse)
            .filter(|t| t.is_valid());
        if let Some(set) = set {
            self.set_study_set(set);
        }
        self.promote_levels(cx);
        if let Some(tf) = tf {
            self.set_timeframe(tf, cx);
        }
    }

    fn menu_items(&self, menu: &StudyMenu, cx: &App) -> Vec<Item> {
        let set = self.study_set();
        let sources: Vec<SourceInfo> = self
            .levels
            .sources()
            .into_iter()
            .map(|(id, label, kinds)| SourceInfo {
                series: lines_feed::levels::registered()
                    .iter()
                    .find(|s| s.id() == id)
                    .map(|s| {
                        s.series_studies()
                            .iter()
                            .map(|d| (d.id.to_owned(), d.name.to_owned()))
                            .collect()
                    })
                    .unwrap_or_default(),
                parents: kinds
                    .iter()
                    .map(|k| self.levels.kind_parent(&id, k))
                    .collect(),
                styled_off: kinds
                    .iter()
                    .map(|k| !self.levels.kind_styled_shown(&id, k))
                    .collect(),
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
        if self.study_menu.is_some() {
            self.close_popover(Kind::Studies, cx);
            return;
        }
        self.close_popover(Kind::Timeframe, cx);
        self.close_popover(Kind::Account, cx);
        let mut m = popover::reopen(&mut self.study_draft);
        let items = self.menu_items(&m, cx);
        m.clamp(&items);
        self.study_menu = Some(m);
        popover::opened(cx, Kind::Studies, |v: &mut Self, cx| {
            v.close_popover(Kind::Studies, cx)
        });
        cx.notify();
    }

    /// Close this chart's popover `kind` if open (an outside click, Esc, another popover
    /// opening). The studies menu keeps its state as the draft the next open restores.
    pub fn close_popover(&mut self, kind: Kind, cx: &mut Context<Self>) {
        match kind {
            Kind::Studies => {
                if !popover::shut(&mut self.study_menu, &mut self.study_draft) {
                    return;
                }
            }
            Kind::Timeframe => {
                if !std::mem::take(&mut self.tf_menu) {
                    return;
                }
            }
            Kind::Account => return crate::trade_bar::close_account_menu(self, cx),
            Kind::StyleEditor => {
                let drawing = self.drawings.close_editor();
                let study = self.study_style.take().is_some();
                if !drawing && !study {
                    return;
                }
            }
            Kind::OrderGroup => {
                if self.ct.group_menu.take().is_none() {
                    return;
                }
            }
            Kind::Menu => return,
        }
        popover::closed(cx, kind);
        cx.notify();
    }

    /// Screenshot fixture: the menu open, optionally on a study's editor.
    fn open_menu_at(&mut self, spec: &str, cx: &mut Context<Self>) {
        let parts: Vec<&str> = spec.split(':').collect();
        let study = |p: &[&str]| -> Option<(StudyRef, usize)> {
            let n = |s: Option<&&str>| s.and_then(|s| s.parse().ok()).unwrap_or(0);
            match p {
                ["vwap", i, rest @ ..] => Some((StudyRef::Vwap(i.parse().ok()?), n(rest.first()))),
                ["delta", i, rest @ ..] => {
                    Some((StudyRef::Delta(i.parse().ok()?), n(rest.first())))
                }
                ["volume", i, rest @ ..] => {
                    Some((StudyRef::Volume(i.parse().ok()?), n(rest.first())))
                }
                ["external", i, rest @ ..] => {
                    Some((StudyRef::External(i.parse().ok()?), n(rest.first())))
                }
                ["profile", rest @ ..] => Some((StudyRef::Profile, n(rest.first()))),
                ["footprint", rest @ ..] => Some((StudyRef::Footprint, n(rest.first()))),
                ["levels", id, rest @ ..] => {
                    Some((StudyRef::Levels((*id).to_owned()), n(rest.first())))
                }
                _ => None,
            }
        };
        let (editing, style) = match parts.as_slice() {
            ["edit", rest @ ..] => (study(rest).map(|(r, _)| r), None),
            ["style", rest @ ..] => {
                let s = study(rest);
                (s.as_ref().map(|(r, _)| r.clone()), s)
            }
            _ => (None, None),
        };
        let mut m = StudyMenu {
            editing,
            ..StudyMenu::default()
        };
        let items = self.menu_items(&m, cx);
        m.clamp(&items);
        // On "Style…" (the draft the style editor steps back to).
        if let Some(i) = items
            .iter()
            .position(|it| {
                matches!(
                    it,
                    Item::Control {
                        cmd: MenuCmd::Style(_),
                        ..
                    }
                )
            })
            .filter(|_| style.is_some())
        {
            m.cursor = i;
        }
        self.study_menu = Some(m);
        if let Some((r, tab)) = style {
            self.open_study_style(r, cx);
            if let Some((_, e)) = self.study_style.as_mut() {
                e.select_tab(tab);
            }
        }
    }

    /// The style editor's title and parts for study `r` of `set` (`None`: not on the chart).
    fn study_style_editor(
        &self,
        set: &StudySet,
        r: &StudyRef,
    ) -> Option<crate::style_editor::StyleEditor> {
        use crate::study_style as ss;
        let (title, defs, looks, source): (String, &[ss::PartDef], Vec<ss::Look>, _) = match r {
            StudyRef::Vwap(i) => {
                let c = set.indicators.get(*i)?;
                let lines_indicators::IndicatorConfig::Vwap { spec, .. } = c else {
                    return None;
                };
                (
                    crate::study_menu::vwap_label(spec),
                    &ss::VWAP,
                    ss::vwap_looks(c).to_vec(),
                    None,
                )
            }
            StudyRef::Delta(i) => {
                let c = set.indicators.get(*i)?;
                let lines_indicators::IndicatorConfig::CumDelta { .. } = c else {
                    return None;
                };
                (
                    "Cumulative delta".to_owned(),
                    &ss::DELTA,
                    ss::read(c.style(), &ss::DELTA).to_vec(),
                    None,
                )
            }
            StudyRef::Volume(i) => {
                let c = set.indicators.get(*i)?;
                let lines_indicators::IndicatorConfig::Volume { .. } = c else {
                    return None;
                };
                (
                    "Volume".to_owned(),
                    &ss::VOLUME,
                    ss::read(c.style(), &ss::VOLUME).to_vec(),
                    None,
                )
            }
            StudyRef::External(i) => {
                let c = set.indicators.get(*i)?;
                let def = crate::studies::ext_def(c)?;
                let defs = crate::series_pane::part_defs(def);
                (
                    def.name.to_owned(),
                    defs,
                    ss::read_slice(c.style(), defs),
                    None,
                )
            }
            StudyRef::Profile => (
                "Volume profile".to_owned(),
                &ss::PROFILE,
                ss::read(&set.styles.profile, &ss::PROFILE).to_vec(),
                None,
            ),
            StudyRef::Footprint => (
                "Footprint".to_owned(),
                &ss::FOOTPRINT,
                ss::read(&set.styles.footprint, &ss::FOOTPRINT).to_vec(),
                None,
            ),
            StudyRef::Levels(id) => {
                let (_, label, _) = self.levels.sources().into_iter().find(|s| s.0 == *id)?;
                let (types, label_default) = self.levels.source_types(id)?;
                let empty = lines_indicators::PartStyles::new();
                return Some(ss::level_editor(
                    format!("Levels · {label} style"),
                    self.levels.styles().get(id).unwrap_or(&empty),
                    types,
                    label_default,
                    self.levels.source_color(id),
                    |kind| self.levels.kind_color(id, kind),
                ));
            }
        };
        Some(ss::editor(format!("{title} style"), defs, &looks, source))
    }

    /// Open the style editor on study `r`'s parts ("Style…" in its editor): the studies menu
    /// closes, kept as the draft Esc steps back to; any other popover closes (one at a time).
    fn open_study_style(&mut self, r: StudyRef, cx: &mut Context<Self>) {
        let Some(e) = self.study_style_editor(&self.study_set(), &r) else {
            return;
        };
        self.close_popover(Kind::StyleEditor, cx);
        self.close_popover(Kind::Studies, cx);
        self.close_popover(Kind::Timeframe, cx);
        self.close_popover(Kind::Account, cx);
        self.picker.close();
        self.anchoring = None;
        self.editor_scroll = Default::default();
        self.study_style = Some((r, e));
        popover::opened(cx, Kind::StyleEditor, |v: &mut Self, cx| {
            v.close_popover(Kind::StyleEditor, cx)
        });
        cx.notify();
    }

    /// A style editor is open (a drawing's or a study's).
    fn styling(&self) -> bool {
        self.drawings.editing() || self.study_style.is_some()
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
                // Finished with the menu: no draft to come back to.
                self.study_menu = None;
                self.study_draft = None;
                popover::closed(cx, Kind::Studies);
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
                    s.apply(&StudyAction::Replace(Box::new(set)));
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
            Some(MenuCmd::Close) => self.close_popover(Kind::Studies, cx),
            Some(MenuCmd::Style(r)) => self.open_study_style(r, cx),
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

    /// Rebuild the latency HUD's text (render time; at most once a second unless frozen).
    fn refresh_hud(&mut self, cx: &App) {
        use crate::latency_hud::{Measure, Snapshot};
        if !self.latency_hud {
            self.hud_at = None;
            return;
        }
        if !self.frozen
            && self
                .hud_at
                .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        // Copy each window's values out and drop its lock at once: the feed and the trading
        // core never wait on this text (decisions 0001, 0012).
        let mut stamped_on_receipt = false;
        let feed_now = self.store.feed_clock().map(|c| c.0);
        let (feed, bound) = {
            let Some(d) = self.store.try_read() else {
                return; // the feed holds the lock this instant: next frame
            };
            match d.latency.as_deref() {
                Some(l) => {
                    // The feed's clock (runs on while live) so old samples leave a quiet feed.
                    let now = self
                        .store
                        .feed_clock()
                        .map_or(l.at_ms, |c| c.0.max(l.at_ms));
                    let offset = l.offset.bound_ms(now);
                    // All-zero transit with a zero offset bound: no separate exchange time.
                    // (A clock behind the exchange also records 0s, but a negative bound.)
                    stamped_on_receipt = offset == Some(0);
                    let bound = if l.wall_clock { offset } else { None };
                    (Some(Snapshot::of(&l.exch_recv, now)), bound)
                }
                None => (None, None),
            }
        };
        // Wall-clock measures (paint, order) are not shown in a frozen (screenshot) frame;
        // feed and ack run on the data's and the core's clocks, so they are reproducible.
        let paint = (!self.frozen)
            .then(|| self.paint_latency.window())
            .flatten()
            .map(|w| Snapshot::of(w, (lines_feed::store::mono_us() / 1000) as i64));
        let orders =
            match crate::trading::Trading::try_global(cx).map(|t| t.order_latency().try_lock()) {
                Some(Ok(h)) => Some(h),
                // A panic while recording leaves plain counters: still readable.
                Some(Err(std::sync::TryLockError::Poisoned(p))) => Some(p.into_inner()),
                Some(Err(std::sync::TryLockError::WouldBlock)) => return, // recording: next frame
                None => None,
            };
        let (order, ack) = match orders {
            Some(h) => {
                // The core's clock, or this chart's feed clock when that is newer (the core
                // idles without orders): old samples leave the window either way.
                let now = h
                    .at_ms
                    .unwrap_or(i64::MIN)
                    .max(feed_now.unwrap_or(i64::MIN));
                (
                    (!self.frozen).then(|| Snapshot::of(&h.click_sent, now)),
                    Some(Snapshot::of(&h.ack_rtt, now)),
                )
            }
            None => (None, None),
        };
        let m = |label, data, ms_resolution| Measure {
            label,
            data,
            ms_resolution,
            zero_is_unknown: false,
        };
        // Feed and ack are timed in whole milliseconds; paint and order in µs.
        let mut measures = vec![Measure {
            zero_is_unknown: stamped_on_receipt,
            ..m("feed", feed, true)
        }];
        if !self.frozen {
            measures.push(m("paint", paint, false));
            measures.push(m("order", order, false));
        }
        // The app's trading runs the paper broker: its ack time is simulated.
        measures.push(m("ack (sim)", ack, true));
        // A narrow chart gets the p99s only (no sparklines), so the HUD isn't cut away.
        let compact = hud_compact(f32::from(self.chart.size.width));
        self.hud_compact = compact;
        let mut text = String::with_capacity(200);
        let clock = crate::latency_hud::write_hud(&mut text, &measures, compact, bound);
        self.hud_text = text;
        self.hud_clock = clock;
        self.hud_at = Some(Instant::now());
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
            Some(match s.divider_at(y) {
                Some(k) => Region::Divider(k),
                None => Region::Sub,
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
        // A drag moves the view itself: a switch's waiting anchor no longer applies.
        self.tf_anchor = None;
        self.drag = Some(drag);
        self.vp.set_held(true, self.series_len);
    }

    fn end_drag(&mut self) {
        self.drag = None;
        self.vp.set_held(false, self.series_len);
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        popover::clear_dismissing();
        let Some(region) = self.region(ev.position) else {
            return;
        };
        // An open list or menu normally takes this press on its backdrop (`crate::popover`);
        // this is the fallback (a list whose chip is clipped away draws none). A press that
        // closes something does only that: no focus change, no pan.
        if self.tf_menu
            || self.trade.account_menu
            || self.study_menu.is_some()
            || self.styling()
            || self.ct.group_menu.is_some()
        {
            popover::mark_dismissing();
            self.close_popover(Kind::OrderGroup, cx);
            self.close_popover(Kind::Timeframe, cx);
            self.close_popover(Kind::Account, cx);
            self.close_popover(Kind::Studies, cx);
            self.close_popover(Kind::StyleEditor, cx);
            cx.stop_propagation();
            return;
        }
        self.focus.focus(window, cx);
        // A press on the chart leaves a size being typed.
        self.trade.qty_edit = None;
        let typing = self.trade.resize_edit.take().map(|(id, _)| id.0);
        if self.picker.close() {
            // Clicking the chart dismisses the picker (rows handle their own clicks).
            cx.notify();
            return;
        }
        self.end_drag();
        let (x, y) = self.local(ev.position);
        // A lower pane's ×: its study goes (as Remove in the studies menu) on a release still
        // on the same × (a pan that starts there removes nothing).
        if region == Region::Sub
            && let Some(k) = self.close_hit(x as f32, y as f32)
        {
            self.pane_closing = Some(k);
            return;
        }
        if self
            .strip_box()
            .is_some_and(|b| b.contains(x as f32, y as f32))
        {
            return; // the strip's own buttons take their presses
        }
        if self.ct.strip_pick.is_some() {
            // A strip limit: a click on the plot is its price, the crosshair's at the press (the
            // magnet's included), computed now, never last frame's. Sent on release if it stays
            // a click; a drag pans instead and the pick stays armed. Elsewhere keep waiting.
            if region == Region::Plot {
                let at = self.price_at(x, y, window);
                self.pick_press = Some((ev.position, at));
                cx.stop_propagation();
            }
            return;
        }
        if let Some(index) = self.anchoring {
            // Anchored VWAP: this click picks the bar (off the bars: keep waiting).
            if region == Region::Plot
                && let Some(from) = self.store.try_read().and_then(|d| {
                    anchor_at(
                        bars_view(&self.derived, self.tick_key(), &d).bars,
                        &self.vp,
                        x,
                    )
                })
            {
                self.anchoring = None;
                self.run_menu(
                    Some(MenuCmd::Set(StudyAction::AnchorAt { index, from })),
                    cx,
                );
            }
            return;
        }
        // The − / size / + first: quick clicks on them are steps, never a double click.
        if region == Region::Plot
            && !ev.modifiers.control
            && !self.drawings.active()
            && self.resize_pressed(x, y, ev.modifiers.alt, typing, cx)
        {
            cx.stop_propagation();
            return;
        }
        // Drawings take a press only in drawing mode or on a drawing, never with Ctrl (chart
        // trading's); otherwise the chart pans and scales as usual.
        if region == Region::Plot {
            // A double click on a drawing opens its style editor (never with Ctrl/Cmd).
            let ctrl = ev.modifiers.control || ev.modifiers.platform;
            if ev.click_count >= 2
                && let Some(id) = self.drawings.double_click(x as f32, y as f32, ctrl)
                && self.open_style_editor(id, cx)
            {
                return;
            }
            // Shift+drag on empty plot measures (a quick second one too: never the view
            // reset). An order line, tag or handle keeps its own press, as does a drawing (and
            // its handles); Ctrl and Alt keep theirs. With no anchor this instant (the feed
            // holds the store, or no bars) the press does nothing rather than pan.
            if crate::chart_draw::shift_measures(
                ev.modifiers.shift,
                ctrl || ev.modifiers.alt,
                self.drawings.tool().is_some(),
                self.order_under(x, y),
                self.drawings.hit_at(x as f32, y as f32).is_some(),
            ) {
                let anchor = self.drawing_anchor(x, y);
                self.drawings.quick_down(x as f32, y as f32, anchor);
                cx.notify();
                return;
            }
            let anchor = self.drawing_anchor(x, y);
            // A placed measure moves or resizes by a drag on it (an order's tag keeps its
            // press; Alt and Ctrl keep theirs).
            if !ctrl
                && !ev.modifiers.alt
                && !ev.modifiers.shift
                && !self.order_under(x, y)
                && self.drawings.measure_down(x as f32, y as f32, anchor)
            {
                cx.notify();
                return;
            }
            if self.drawings.on_down(x as f32, y as f32, ctrl, anchor) {
                cx.notify();
                return;
            }
        }
        if ev.click_count >= 2 {
            // The user's own reset wins over a switch's waiting anchor.
            self.tf_anchor = None;
            match region {
                Region::Plot => self.vp.reset(self.series_len),
                Region::PriceAxis => self.vp.restore_auto_price(),
                // Each axis resets only itself: time = bar width and the live edge.
                Region::TimeAxis | Region::Sub => self.vp.reset_time(self.series_len),
                Region::Divider(_) => {
                    self.subpane_ratios = std::array::from_fn(crate::subpane::default_ratio)
                }
                Region::SubAxis => return,
            }
            cx.notify();
            return;
        }
        if region == Region::Plot && !ev.modifiers.control && self.order_pressed(x, y, cx) {
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
            Region::Divider(k) => Drag::Divider {
                y: f32::from(ev.position.y),
                k,
                n: self.split.map_or(0, |s| s.panes().len()),
                ratios: self
                    .split
                    .map_or(self.subpane_ratios, |s| s.ratios(self.above_axis)),
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
            let outcome = self
                .picker
                .key(ev, today, &crate::sources::pick(&self.feed, cx), cx);
            self.picked(outcome, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.study_menu.is_some() {
            // Esc steps back (a template name, an editor, the filter) and at the top closes
            // the menu (`MenuCmd::Close`), its state kept as a draft (the popover rule).
            self.menu_key(ev, cx);
            cx.stop_propagation();
            return;
        }
        if (self.tf_menu || self.trade.account_menu || self.ct.group_menu.is_some())
            && popover::closes_on_key(&k.key)
        {
            self.close_popover(Kind::OrderGroup, cx);
            self.close_popover(Kind::Timeframe, cx);
            self.close_popover(Kind::Account, cx);
            cx.stop_propagation();
            return;
        }
        if self.styling() {
            // The style editor takes its keys; Undo and Redo still work on a drawing (the
            // workspace leaves them to a pane taking text, so they run here).
            match editor_key(k) {
                EditorKey::Editor => {
                    let text = k.key_char.as_deref().filter(|_| !m.alt);
                    let (key, shift) = (k.key.clone(), m.shift);
                    self.style_edit(|e| e.key(&key, shift, text), cx);
                    cx.stop_propagation();
                    return;
                }
                // A study's styles aren't drawings: nothing of theirs to undo.
                EditorKey::Draw(_) if self.study_style.is_some() => {
                    cx.stop_propagation();
                    return;
                }
                EditorKey::Draw(d) => {
                    self.drawing_command(d, cx);
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                EditorKey::Pass => {}
            }
        }
        // Escape aborts an order drag: a tag, a handle or the position line (nothing sent).
        if (self.ct.drag.is_some() || self.ct.adding.is_some() || self.ct.pos_drag)
            && k.key == "escape"
        {
            self.ct.drag = None;
            self.ct.adding = None;
            self.ct.pos_drag = false;
            self.ct.add_from = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.anchoring.is_some() && k.key == "escape" {
            self.anchoring = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.ct.strip_pick.is_some() && k.key == "escape" {
            self.ct.strip_pick = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        // A working order's size being typed (Alt+Enter: one leg of a linked pair).
        if !(m.control || m.platform)
            && crate::trade_bar::resize_key(self, k.key.as_str(), m.alt, cx)
        {
            cx.stop_propagation();
            return;
        }
        // A size being typed takes the plain keys (digits, Enter, Esc), only while its field is
        // on screen (`qty_key`).
        if !(m.control || m.alt || m.platform)
            && crate::trade_bar::qty_key(self, k.key.as_str(), cx)
        {
            cx.stop_propagation();
            return;
        }
        // Drawings: a note being typed or a confirmation takes every key; Esc, Enter, Delete
        // go to them when they have a use for it (a tool, a measure, a selection).
        // A note takes what the layout types: AltGr arrives as Ctrl+Alt on Windows, Option
        // characters with Alt on macOS; plain Ctrl or Cmd shortcuts are never text.
        let note_text = !m.platform && (!m.control || m.alt);
        let typed = k.key_char.as_deref().filter(|_| note_text);
        if ((self.drawings.typing() && note_text)
            || (!m.control
                && !m.platform
                && self.tf_entry.is_none()
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
        // The picker covers the chart: its lists and menu close (their backdrops would take
        // the picker's clicks).
        for kind in PICKER_CLOSES {
            self.close_popover(kind, cx);
        }
        self.picker.open(query, cx);
    }

    fn picker_click(&mut self, row: usize, cx: &mut Context<Self>) {
        let today = pane::today(self.frozen, &self.store);
        let outcome = self
            .picker
            .click(row, today, &crate::sources::pick(&self.feed, cx));
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
        let feed = crate::sources::retarget(&self.feed, target, cx)?;
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
        // A limit pick belongs to the symbol it was armed on; so does a size being typed.
        self.ct.feed_changed();
        self.trade.qty_edit = None;
        self.trade.resize_edit = None;
        self.readout = None;
        self.labels.clear();
        // The same timeframe on the new market.
        self.derived = None;
        self.ticks = None;
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
        self.close_popover(Kind::Timeframe, cx);
        self.tf_entry = None;
        if tf != self.timeframe && tf.is_valid() {
            let anchor = self.view_anchor();
            self.apply_timeframe(tf);
            // No bars to anchor on yet (a rebuild still runs): the waiting anchor stays.
            self.tf_anchor = anchor.or(self.tf_anchor);
        }
        cx.notify();
    }

    /// Set the timeframe without moving the view (restore, a new feed).
    fn apply_timeframe(&mut self, tf: Timeframe) {
        if tf == self.timeframe || !tf.is_valid() {
            return;
        }
        self.timeframe = tf;
        self.ticks = None; // let go of the old series before asking for the new one
        self.ticks = wanted_tick_key(tf, self.tick_reset).map(|k| self.store.acquire_ticks(k));
        let capacity = self.store.read().series.capacity();
        self.derived = (tf.source() == BarSource::OneMinute && !tf.is_one_minute())
            .then(|| Derived::new(tf, capacity));
        // Indices change: re-baseline the trim count on the next paint.
        self.series_trimmed = u64::MAX;
        // Time labels are cached by bar time; their text depends on the timeframe.
        self.labels.clear();
        self.readout = None;
        self.cross = None;
        self.cross_bar = None;
    }

    /// The store's tick-bar series this chart shows, if it is on a tick timeframe.
    fn tick_key(&self) -> Option<TickKey> {
        self.ticks.as_ref().map(TickLease::key)
    }

    /// Trade-built bars (tick, volume, range, seconds) run continuously (no reset at the
    /// session open).
    pub fn tick_continuous(&self) -> bool {
        self.tick_reset == TickReset::Continuous
    }

    /// Whether the chart is on a tick timeframe.
    pub fn on_ticks(&self) -> bool {
        self.ticks.is_some()
    }

    /// Switch tick bars between resetting at the session open and running continuously
    /// (`Command::TickBarsContinuous`). Kept for the chart's next tick timeframe too.
    pub fn set_tick_continuous(&mut self, on: bool, cx: &mut Context<Self>) {
        let reset = if on {
            TickReset::Continuous
        } else {
            TickReset::Session
        };
        // A different series: the time in the plot's middle (or under the pointer) stays put.
        let anchor = self.view_anchor();
        if self.apply_tick_reset(reset) {
            // No bars to anchor on yet (a rebuild still runs): the waiting anchor stays.
            self.tf_anchor = anchor.or(self.tf_anchor);
        }
        cx.notify();
    }

    /// Set where tick bars restart their count; on a tick chart, hold the matching series.
    /// `true` if the series changed.
    fn apply_tick_reset(&mut self, reset: TickReset) -> bool {
        self.tick_reset = reset;
        let want = wanted_tick_key(self.timeframe, reset);
        if want == self.tick_key() {
            return false;
        }
        self.ticks = None;
        self.ticks = want.map(|k| self.store.acquire_ticks(k));
        self.series_trimmed = u64::MAX;
        self.readout = None;
        self.cross = None;
        self.cross_bar = None;
        true
    }

    /// The bar time under the pointer (else the plot's middle, unless following the live
    /// edge) and its x: kept in place across a change of bars.
    fn view_anchor(&self) -> Option<(UtcMillis, f64)> {
        let x = match self.hover {
            Some((x, _)) => f64::from(x),
            None if !self.vp.follow_live => self.vp.width * 0.5,
            None => return None,
        };
        let d = self.store.try_read()?;
        let view = bars_view(&self.derived, self.tick_key(), &d);
        let last = view.len().checked_sub(1)?;
        let i = self.vp.x_to_index(x).round().clamp(0.0, last as f64) as usize;
        Some((view.bars[i].time, x))
    }

    /// Open or close the timeframe list (header chip, pane menu, palette).
    pub fn toggle_timeframe_menu(&mut self, cx: &mut Context<Self>) {
        if self.tf_menu {
            self.close_popover(Kind::Timeframe, cx);
            return;
        }
        self.picker.close();
        self.close_popover(Kind::Studies, cx);
        self.close_popover(Kind::Account, cx);
        self.tf_entry = None;
        self.tf_menu = true;
        popover::opened(cx, Kind::Timeframe, |v: &mut Self, cx| {
            v.close_popover(Kind::Timeframe, cx)
        });
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

    /// View only: this chart shows no orders and sends none (the option chart's underlying,
    /// decision 0029).
    pub fn set_view_only(&mut self) {
        self.view_only = true;
        self.trade.account = None;
    }

    /// The auto-fit's smallest price range. Returns whether it changed.
    pub fn set_min_price_span(&mut self, span: f64) -> bool {
        self.vp.set_min_price_span(span)
    }

    /// Its price scale, once laid out: what the option chart's chain lines up with.
    pub fn locked_scale(&self) -> Option<crate::chain_lock::LockedScale> {
        // (Before a first fit its range says nothing: the chain stays a plain list.)
        let ready = f32::from(self.plot.size.height) > 1.0 && self.vp.price_known();
        ready.then(|| crate::chain_lock::LockedScale {
            vp: self.vp,
            plot_top: f32::from(self.plot.origin.y),
        })
    }

    /// Something of the chart's own is open over its plot: what's drawn over the chart gives way.
    pub fn overlay_open(&self) -> bool {
        self.takes_text() || self.tf_menu
    }

    /// No smallest price range yet: setting one refits, even under a hold.
    pub fn no_min_price_span(&self) -> bool {
        self.vp.min_price_span == 0.0
    }

    /// A scale the trader chose (a double-click on the plot or price axis goes back to the fit).
    pub fn set_price_range(&mut self, lo: f64, hi: f64) {
        self.vp.price_lo = lo;
        self.vp.price_hi = hi;
        self.vp.auto_price = false;
    }

    /// Hold the view against new data while something aligned to it is under the pointer. The
    /// trader's own zoom and pan still apply, and whatever is aligned follows them.
    pub fn hold_scale(&mut self, on: bool) {
        self.vp.price_held = on;
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

    /// The order strip shown on this chart.
    pub fn order_strip(&self) -> bool {
        self.order_strip
    }

    /// The order strip's box this frame (plot-local), if shown and it fits.
    fn strip_box(&self) -> Option<crate::chart_trade::StripBox> {
        // An unavailable pane draws its notice instead of the chart.
        if !self.order_strip || self.feed.unavailable.is_some() {
            return None;
        }
        crate::chart_trade::strip_box(
            f32::from(self.plot.size.width),
            f32::from(self.plot.size.height),
        )
    }

    /// The order strip over the plot's bottom-left corner: the shared size, BUY/SELL MKT
    /// filled in the side's color, BUY/SELL LMT outlined (as the DOM's buttons; filled while
    /// its price pick is armed). Dimmed while the chart is in no link group (no DOM's switch
    /// can send).
    fn order_strip_view(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::chart_trade::{
            STRIP_GAP, STRIP_H, STRIP_LMT_W, STRIP_MKT_W, STRIP_PAD, STRIP_QTY_W, StripOrder,
        };
        let b = self.strip_box()?;
        let dim = self.common.link.is_none();
        let pick = self.ct.strip_pick;
        let size = crate::trade_bar::pane_size(self, cx);
        let over = crate::dom_trade::over_max(size, crate::trade_bar::max_order_qty(self, cx));
        // − value +: the value is the field while typing, and "max N" over the maximum (the
        // buttons still say the size). The wheel steps it anywhere on the control.
        let step = |id: &'static str, text: &'static str, up: bool, cx: &mut Context<Self>| {
            div()
                .id(id)
                .w(px(crate::chart_trade::STRIP_STEP_W))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .border_1()
                .border_color(t.control_edge)
                .text_color(t.text_main)
                .cursor_pointer()
                .child(text)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |v, _, window, cx| {
                        cx.stop_propagation();
                        v.focus.focus(window, cx);
                        crate::trade_bar::step_size(v, up, cx);
                    }),
                )
        };
        let value = match (&self.trade.qty_edit, &over) {
            (Some(s), _) => format!("{s}_"),
            (None, Some(text)) => text.clone(),
            (None, None) => size.to_string(),
        };
        let size_control = div()
            .id("strip-size")
            .w(px(STRIP_QTY_W))
            .h_full()
            .flex()
            .flex_row()
            .gap(px(2.0))
            .on_scroll(cx.listener(|v: &mut Self, ev: &ScrollEvent, w, cx| {
                crate::trade_bar::scroll_size(v, ev, w, cx)
            }))
            .child(step("strip-size-down", "−", false, cx))
            .child(
                div()
                    .id("strip-size-value")
                    .w(px(crate::chart_trade::STRIP_VALUE_W))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .cursor_pointer()
                    .text_color(if over.is_some() { t.warn } else { t.text_main })
                    .child(value)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|v, _, window, cx| {
                            cx.stop_propagation();
                            v.focus.focus(window, cx);
                            crate::trade_bar::open_size_field(v);
                            cx.notify();
                        }),
                    ),
            )
            .child(step("strip-size-up", "+", true, cx));
        let mut buttons = vec![
            (
                StripOrder::BuyMarket,
                format!("BUY {size} MKT"),
                STRIP_MKT_W,
                Some(t.up),
                t.up,
            ),
            (
                StripOrder::SellMarket,
                format!("SELL {size} MKT"),
                STRIP_MKT_W,
                Some(t.down),
                t.down,
            ),
        ];
        if b.limits {
            buttons.push((
                StripOrder::BuyLimit,
                format!("BUY {size} LMT"),
                STRIP_LMT_W,
                (pick == Some(StripOrder::BuyLimit)).then_some(t.up),
                t.up,
            ));
            buttons.push((
                StripOrder::SellLimit,
                format!("SELL {size} LMT"),
                STRIP_LMT_W,
                (pick == Some(StripOrder::SellLimit)).then_some(t.down),
                t.down,
            ));
        }
        let mut strip = div()
            .id("order-strip")
            .occlude()
            .absolute()
            .left(px(b.x))
            .top(px(b.y))
            .w(px(b.w))
            .h(px(STRIP_H))
            .p(px(STRIP_PAD))
            .flex()
            .flex_row()
            .gap(px(STRIP_GAP))
            .bg(t.panel)
            .rounded_sm()
            // The strip's geometry is the pointer's (`chart_trade::strip_box`) and doesn't zoom;
            // neither does its text (docs/known-gaps.md).
            .text_size(px(11.0))
            .child(size_control);
        for (o, label, w, fill, line) in buttons {
            let el = div()
                .id(("strip", o as u64))
                .w(px(w))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .border_1()
                .border_color(line)
                .font_weight(FontWeight::BOLD)
                .whitespace_nowrap()
                .overflow_hidden()
                .cursor_pointer()
                .child(label);
            // Filled in the side's color, text in the background color (as the DOM's). Dimmed
            // (no link group: nothing can send): a dashed edge in the side's color and dim text,
            // unfilled, never faded (design.md §6). The edge stays 1 px here: the buttons are a
            // fixed 96 px and a 5-digit size's label needs the room. An armed pick on a dimmed
            // strip (unlinking doesn't drop it, and Esc must have something to cancel) is unfilled
            // too, its label in the side's color: armed, but never solid like a live one.
            let el = if dim { el.border_dashed() } else { el };
            let el = match fill {
                Some(c) if !dim => el.bg(c).text_color(t.bg),
                Some(_) => el.text_color(line),
                None if dim => el.text_color(t.text_dim),
                None => el.text_color(t.text_main),
            };
            strip = strip.child(el.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    // The chart takes focus, so Esc reaches an armed pick.
                    v.focus.focus(window, cx);
                    // Recorded for what's pressed next; its verdict can't refuse more than this.
                    let _ = crate::popover::second_press(ev.click_count, ev.position);
                    if crate::chart_trade::strip_press_acts(o, ev.click_count) {
                        v.strip_pressed(o, cx);
                    }
                }),
            ));
        }
        Some(strip.into_any_element())
    }

    /// The `ToggleOrderStrip` command.
    pub fn toggle_order_strip(&mut self, cx: &mut Context<Self>) {
        self.order_strip = !self.order_strip;
        self.ct.strip_pick = None;
        self.trade.qty_edit = None;
        cx.notify();
    }

    /// Restore saved view state (time zoom, indicators).
    pub fn restore(&mut self, state: &ChartState) {
        self.drawings.hidden = state.hide_drawings;
        self.marks.set_hidden(state.hide_strategy_marks);
        if state.magnet {
            self.crosshair_mode = CrosshairMode::Magnet;
        }
        if state.order_strip {
            self.order_strip = true;
        }

        // A saved timeframe wins; a pane without one keeps its initial (CLI) timeframe.
        if !state.timeframe.is_default() {
            self.apply_timeframe(state.timeframe.0);
        }
        // A saved tick mode wins (a chart already on this timeframe takes the matching series
        // too); a chart without one keeps its initial (CLI) mode.
        self.apply_tick_reset(restored_tick_reset(state, self.tick_reset));
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
        // Saved styles win; a pane without them keeps its initial (`--study-set`) ones.
        if !state.styles.is_empty() {
            self.set_study_styles(&state.styles);
        }
        self.subpane_ratios = crate::subpane::restore(state.subpane, &state.subpanes);
        // Saved indicators, then any given on the command line (`--vwap`) not among them.
        let configs = merge_indicators(state.indicators.clone(), self.studies.configs());
        self.set_indicator_configs(configs);
    }

    /// Study legends (VWAP …) and level-source rows, top left under the readout, unless the
    /// chart hides its study labels (render time).
    fn legend(&self, t: &Theme) -> Option<gpui::AnyElement> {
        if !self.visibility.draws_labels() {
            // The readout still shows over the plot's top-left.
            self.legend_band.set((8.0 + LEGEND_MAX_W, readout_bottom()));
            return None;
        }
        let studies = self.studies.legend(t, &self.instrument);
        // The footprint's line: its mode and why bars are candles (as of the last frame;
        // paint re-renders when that changes).
        let fp = self.footprint.legend();
        let fp = fp.map(|(label, note)| {
            // Capped at the legend's width (the clearance block assumes it), ending in "…".
            // The note (a state: why bars are candles, side-less volume) shrinks and truncates
            // itself: an ellipsis shows the cut (the label stays).
            div()
                .max_w(px(LEGEND_MAX_W))
                .whitespace_nowrap()
                .overflow_hidden()
                .flex()
                .flex_row()
                .gap_2()
                .px(px(8.0))
                .py(px(4.0))
                .bg(t.label_bg)
                .text_size(crate::zoom::ui(12.0))
                .child(div().flex_shrink_0().text_color(t.text_main).child(label))
                .when_some(note, |d, n| {
                    d.child(div().min_w_0().truncate().text_color(t.text_dim).child(n))
                })
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
        let row_h = crate::zoom::f(12.0 * 1.3 + 4.0);
        let bottom = crate::zoom::f(30.0 + boxes as f32 * 12.0 + 4.0) + text_rows as f32 * row_h;
        self.legend_band
            .set((8.0 + LEGEND_MAX_W, bottom.max(readout_bottom())));
        if studies.is_none() && rows.is_empty() && fp.is_none() {
            self.legend_band.set((8.0 + LEGEND_MAX_W, readout_bottom()));
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
                .px(crate::zoom::ui(8.0))
                .py(crate::zoom::ui(4.0))
                .bg(t.label_bg)
                .text_size(crate::zoom::ui(12.0))
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
        let in_sub = self.split.is_some_and(|s| {
            s.pane_at(y).is_some() && x >= 0.0 && x < f32::from(self.plot.size.width)
        });
        let on_strip = self.strip_box().is_some_and(|b| b.contains(x, y));
        let hover = ((self.plot.contains(&pos) || in_sub) && !on_strip).then_some((x, y));
        if hover != self.hover {
            self.hover = hover;
            cx.notify();
        }
    }

    fn on_mouse_exit(&mut self, _: &MouseExitEvent, _: &mut Window, cx: &mut Context<Self>) {
        let tag = self.ct.resize_hover.take().is_some();
        if self.hover.take().is_some() || tag {
            cx.notify();
        }
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.set_hover(ev.position, cx);
        self.trade.resize_armed = true;
        let (tx, ty) = self.local(ev.position);
        let typing = self.trade.resize_edit.as_ref().map(|(id, _)| id.0);
        let tag = self
            .ct
            .resize_target(tx as f32, ty as f32, typing)
            .map(|(id, _)| id);
        if tag != self.ct.resize_hover {
            self.ct.resize_hover = tag;
            cx.notify();
        }
        if let Some((from, _)) = self.pick_press {
            let d = ev.position - from;
            if ev.pressed_button != Some(MouseButton::Left) {
                self.pick_press = None;
            } else if !crate::chart_trade::is_click(f32::from(d.x), f32::from(d.y)) {
                // A drag: pan as any press on the plot would (the pick stays armed).
                self.pick_press = None;
                self.start_drag(Drag::Pan(from, false));
            }
        }
        if ev.pressed_button != Some(MouseButton::Left) && self.drawings.release_lost() {
            cx.notify();
        }
        if self.ct.drag.is_some() || self.ct.adding.is_some() || self.ct.pos_drag {
            if ev.pressed_button != Some(MouseButton::Left) {
                self.ct.drag = None;
                self.ct.adding = None;
                self.ct.pos_drag = false;
            } else {
                let (_, y) = self.local(ev.position);
                let tick = self.instrument.price.tick;
                if let Some(to) = lines_core::geometry::snap_price(&self.vp, y, tick) {
                    if let Some(d) = &mut self.ct.drag {
                        d.to = Price(to.0 + self.ct.grab);
                    }
                    if self.ct.pos_drag || self.ct.adding.is_some() {
                        self.ct.moved = self.ct.moved.max((y as f32 - self.ct.press_y).abs());
                    }
                    if self.ct.pos_drag {
                        let (qty, avg) = self.ct.position.unwrap_or((0, f64::NAN));
                        self.ct.adding =
                            lines_core::geometry::order_tags::position_drag_exit(qty, avg, to)
                                .map(|target| crate::chart_trade::AddExit { target, to });
                    } else if let Some(a) = &mut self.ct.adding {
                        a.to = to;
                    }
                }
            }
            cx.notify();
            return;
        }
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
            Some(Drag::Divider {
                y: y0,
                k,
                n,
                ratios,
            }) => {
                let dy = f32::from(ev.position.y) - *y0;
                let r = crate::subpane::drag_stack(self.above_axis, *n, *ratios, *k, dy);
                // Only the panes the divider moves (the top one: pane 0; another: its two).
                let moved = if *k == 0 { 0..1 } else { *k - 1..*k + 1 };
                self.subpane_ratios[moved.clone()].copy_from_slice(&r[moved]);
            }
            None => return,
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        // The release of a press that closed a popover does nothing here (no drawing finished).
        if popover::dismissing() {
            return;
        }
        if let Some((from, at)) = self.pick_press.take() {
            // A click while a strip limit's pick was armed. A press that left the chart before a
            // move reached it (released anywhere, through `on_mouse_up_out`) is measured here
            // too: past the click rule it was a drag, and nothing is sent (the pick stays).
            let d = ev.position - from;
            if !crate::chart_trade::is_click(f32::from(d.x), f32::from(d.y)) {
                cx.notify();
                return;
            }
            match (self.ct.strip_pick, at) {
                (Some(o), Ok(at)) => {
                    self.ct.strip_pick = None;
                    self.strip_send(o, at, cx);
                }
                // Market data busy at the press: say so, keep the pick (click again).
                (Some(_), Err(why)) => self.trade.flash(why),
                (None, _) => {}
            }
            cx.notify();
            return;
        }
        if let Some(k) = self.pane_closing.take() {
            let (x, y) = self.local(ev.position);
            if self.close_hit(x as f32, y as f32) == Some(k) {
                self.remove_lower(k);
            }
            cx.notify();
            return;
        }
        if let Some(id) = self.ct.closing.take() {
            let (x, y) = self.local(ev.position);
            let n = self.ct.n_tags;
            let still = lines_core::geometry::order_tags::hit(
                &self.ct.tags[..n],
                &self.ct.markers,
                &self.ct.handles,
                x as f32,
                y as f32,
            ) == Some(lines_core::geometry::order_tags::TagHit::Close(id));
            if still {
                self.cancel_order(id, cx);
            }
            cx.notify();
            return;
        }
        if let Some(d) = self.ct.drag.take() {
            // Released over the plot: move there. Outside it (another pane, the axes): abort.
            if self.plot.contains(&ev.position) {
                self.drop_order(d, cx);
            }
            cx.notify();
            return;
        }
        // An add drag (a handle or the position line): place, move the existing exit of that
        // kind, or nothing (a click, or released at the average) — `chart_trade::release_action`.
        if std::mem::take(&mut self.ct.pos_drag) || self.ct.adding.is_some() {
            // Released off the plot (another pane, the axes): abort, nothing sent.
            let adding = self
                .ct
                .adding
                .take()
                .filter(|_| self.plot.contains(&ev.position));
            self.ct.add_from = None;
            let existing = |target: bool| self.ct.existing_exit(target).map(|m| m.id);
            let action = crate::chart_trade::release_action(adding, self.ct.moved, existing);
            match action {
                crate::chart_trade::Release::Nothing => {}
                crate::chart_trade::Release::Place(a) => self.place_exit(a, cx),
                crate::chart_trade::Release::Move { order, to } => {
                    self.drop_order(crate::dom_orders::DragOrder { order, to }, cx)
                }
            }
            cx.notify();
            return;
        }
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

    /// A press on the hovered tag's − / size / +. Returns whether it was on one.
    fn resize_pressed(
        &mut self,
        x: f64,
        y: f64,
        alt: bool,
        typing: Option<u64>,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::order_resize::{ResizeHit, SizeStep};
        use crate::trade_bar::TradePane;
        let Some((id, Some(hit))) = self.ct.resize_target(x as f32, y as f32, typing) else {
            return false;
        };
        let id = lines_trading::OrderId(id);
        match hit {
            ResizeHit::Down => self.send_resize(id, SizeStep::Down, alt, cx),
            ResizeHit::Up => self.send_resize(id, SizeStep::Up, alt, cx),
            ResizeHit::Value => {
                crate::trade_bar::open_resize_field(self, id);
                cx.notify();
            }
        }
        true
    }

    /// Whether an order's tag, marker or handle, or the position line's tag, is at plot-local
    /// (`x`, `y`): what `order_pressed` would take.
    fn order_under(&self, x: f64, y: f64) -> bool {
        let n = self.ct.n_tags;
        lines_core::geometry::order_tags::hit(
            &self.ct.tags[..n],
            &self.ct.markers,
            &self.ct.handles,
            x as f32,
            y as f32,
        )
        .is_some()
            || (self.ct.position.is_some()
                && self
                    .ct
                    .pos_tag
                    .is_some_and(|t| t.contains(x as f32, y as f32)))
    }

    /// A press in the plot at plot-local (`x`, `y`) on an order tag, its `×`, a position
    /// handle or an edge marker (last frame's layout). Returns whether it was one of those.
    fn order_pressed(&mut self, x: f64, y: f64, cx: &mut Context<Self>) -> bool {
        use lines_core::geometry::order_tags::{TagHit, centre_on, handle_is_target, hit};
        let n = self.ct.n_tags;
        let Some(h) = hit(
            &self.ct.tags[..n],
            &self.ct.markers,
            &self.ct.handles,
            x as f32,
            y as f32,
        ) else {
            // The position line's tag (phase 2): drag it toward profit for a target, toward
            // loss for a stop; released, the same reduce-only exit as the handles.
            if self.ct.position.is_some()
                && self
                    .ct
                    .pos_tag
                    .is_some_and(|t| t.contains(x as f32, y as f32))
            {
                self.ct.pos_drag = true;
                self.ct.adding = None;
                self.ct.press_y = y as f32;
                self.ct.moved = 0.0;
                self.ct.add_from =
                    lines_core::geometry::snap_price(&self.vp, y, self.instrument.price.tick);
                cx.notify();
                return true;
            }
            return false;
        };
        match h {
            // A group tag lists its orders (no one order to drag, no × for many at one press).
            TagHit::Body(id) | TagHit::Close(id) if self.ct.group_of(id).is_some() => {
                let bottom = self.ct.tags[..n]
                    .iter()
                    .find(|b| b.id == id)
                    .map_or(y as f32 + tag_h_now(), |b| b.top + b.h);
                self.open_order_group(id, bottom, cx);
            }
            // Moving and cancelling need no arming, as on the DOM.
            TagHit::Body(id) => {
                if let Some(m) = self.ct.mark(id).copied() {
                    let tick = self.instrument.price.tick;
                    let at = lines_core::geometry::snap_price(&self.vp, y, tick);
                    self.ct.grab = at.map_or(0, |p| m.price.0 - p.0);
                    self.ct.drag = Some(crate::dom_orders::DragOrder {
                        order: m.id,
                        to: m.price,
                    });
                }
            }
            // × cancels on release, and only if the pointer is still on that ×.
            TagHit::Close(id) => {
                self.ct.closing = Some(id);
            }
            // Adding an exit is a new order: trading must be enabled (checked on release
            // too, with the price).
            TagHit::Handle(above) => {
                let qty = self.ct.position.map_or(0, |p| p.0);
                if let Some(to) =
                    lines_core::geometry::snap_price(&self.vp, y, self.instrument.price.tick)
                {
                    self.ct.adding = Some(crate::chart_trade::AddExit {
                        target: handle_is_target(qty, above),
                        to,
                    });
                    self.ct.add_from = Some(to);
                    self.ct.press_y = y as f32;
                    self.ct.moved = 0.0;
                }
            }
            TagHit::Edge(price) => {
                let (lo, hi) = centre_on(&self.vp, price);
                self.vp.price_lo = lo;
                self.vp.price_hi = hi;
                self.vp.auto_price = false;
            }
        }
        cx.notify();
        true
    }

    /// The chart's product (its price format, its money per unit).
    pub(crate) fn instrument(&self) -> &lines_core::Product {
        &self.instrument
    }

    /// The plot, as last drawn (window coordinates).
    pub(crate) fn plot_bounds(&self) -> Bounds<Pixels> {
        self.plot
    }

    /// Open the list of a group tag's orders (`crate::order_group_view`).
    fn open_order_group(&mut self, id: u64, tag_bottom: f32, cx: &mut Context<Self>) {
        let Some(g) = self.ct.group_of(id).copied() else {
            return;
        };
        let rows = self
            .ct
            .members(&g)
            .iter()
            .filter_map(|o| self.ct.mark(o.id).copied())
            .map(|mark| crate::chart_trade::GroupRow { mark, open: true })
            .collect::<Vec<_>>();
        let mut title = String::new();
        if let Some(r) = rows.first() {
            let _ = crate::chart_trade::write_group_title(
                &mut title,
                &r.mark,
                &g,
                &self.instrument.price,
            );
        }
        self.close_popover(Kind::Timeframe, cx);
        self.close_popover(Kind::Account, cx);
        self.close_popover(Kind::Studies, cx);
        self.ct.group_menu = Some(crate::chart_trade::GroupMenu {
            rows,
            account: self.trade.account.clone(),
            title,
            tag_bottom,
            confirm: None,
        });
        popover::opened(cx, Kind::OrderGroup, |v: &mut Self, cx| {
            v.close_popover(Kind::OrderGroup, cx)
        });
    }

    /// Cancel one order of a group's list, by its row: never through this frame's marks (a busy
    /// feed can leave them empty for a frame, and a press must not be swallowed). The core
    /// answers for an order that has finished.
    pub(crate) fn cancel_listed(&mut self, id: u64, cx: &mut Context<Self>) {
        let account = self.ct.group_menu.as_ref().and_then(|m| m.account.clone());
        if let (Some(account), Some(t)) = (account, crate::trading::Trading::try_global(cx)) {
            t.submit(
                lines_trading::Source::Chart,
                std::time::Instant::now(),
                lines_trading::Command::CancelOrder {
                    account,
                    order: lines_trading::OrderId(id),
                },
            );
        }
    }

    /// Cancel one of the chart's orders (its tag's ×): no switch needed, as on the DOM.
    fn cancel_order(&mut self, id: u64, cx: &mut Context<Self>) {
        if let (Some(m), Some(account), Some(t)) = (
            self.ct.mark(id).copied(),
            self.trade.account.clone(),
            crate::trading::Trading::try_global(cx),
        ) {
            t.submit(
                lines_trading::Source::Chart,
                std::time::Instant::now(),
                lines_trading::Command::CancelOrder {
                    account,
                    order: m.id,
                },
            );
        }
    }

    /// Release after dragging an order's tag: move it to the snapped price (unless unchanged),
    /// with a ghost line until the broker confirms (the DOM's path).
    fn drop_order(&mut self, d: crate::dom_orders::DragOrder, cx: &mut Context<Self>) {
        let issued = std::time::Instant::now();
        let Some(t) = crate::trading::Trading::try_global(cx) else {
            return;
        };
        let order = t.cache.read().order(d.order).cloned();
        let Some(o) = order else {
            return;
        };
        let Some(cmd) = crate::dom_trade::drag_command(&o, d.to) else {
            return; // it didn't move
        };
        // A stop never moves across the market (it would trigger at once): refused here, shown
        // as a rejected drag, nothing sent.
        if let Some(why) = crate::trade_bar::stop_move_check(&self.store, &o, d.to) {
            self.trade.flash(why);
            return;
        }
        {
            let id = t.submit(lines_trading::Source::Chart, issued, cmd);
            self.ct.ghosts.push(crate::dom_trade::Ghost {
                order: o.id,
                price: d.to,
                command: id,
                qty: None,
            });
        }
    }

    /// Release after dragging a position handle out: place that exit (reduce-only), or say
    /// why not.
    fn place_exit(&mut self, a: crate::chart_trade::AddExit, cx: &mut Context<Self>) {
        let issued = std::time::Instant::now();
        let (Some(account), Some(contract), Some((qty, _))) = (
            self.trade.account.clone(),
            crate::trade_bar::contract_of(&self.store),
            self.ct.position,
        ) else {
            return;
        };
        // A chart shows no trading switch (trade panel v2): the order goes to the window, which
        // sends it only when a DOM linked to this chart, on the same account and contract, has
        // trading enabled (the same switch as any new order).
        let inside = self
            .store
            .try_read()
            .map(|d| {
                (
                    d.book.best_bid().map(|b| b.0).or(d.bid),
                    d.book.best_ask().map(|a| a.0).or(d.ask),
                    d.last_trade,
                )
            })
            .unwrap_or((None, None, None));
        match crate::chart_trade::exit_command(&account, contract, qty, a.target, a.to, inside) {
            Ok(command) => cx.emit(ChartOrder {
                command,
                refused: crate::chart_trade::ENABLE_TO_ADD,
                resize: None,
                account,
                contract,
                link: self.common.link,
                issued,
            }),
            Err(crate::dom_trade::Refusal(why)) => self.trade.flash(why),
        }
    }

    /// An order strip button: market sends at once; a limit arms (or disarms) the pick, and the
    /// next click on the plot sends it at that price.
    fn strip_pressed(&mut self, o: crate::chart_trade::StripOrder, cx: &mut Context<Self>) {
        use crate::chart_trade::{LINK_TO_TRADE, StripOrder};
        match o {
            StripOrder::BuyLimit | StripOrder::SellLimit => {
                if self.common.link.is_none() {
                    self.trade.flash(LINK_TO_TRADE);
                } else {
                    self.ct.arm_pick(o);
                }
                cx.notify();
            }
            StripOrder::BuyMarket | StripOrder::SellMarket => self.strip_send(o, None, cx),
        }
    }

    /// The crosshair's price at plot-local (`x`, `y`), as paint would draw it there (the
    /// magnet's included); `Ok(None)` with no bars there.
    fn price_at(&self, x: f64, y: f64, window: &Window) -> PickPrice {
        let d = self
            .store
            .try_read()
            .ok_or("market data busy; click again")?;
        let bars = bars_view(&self.derived, self.tick_key(), &d).bars;
        Ok(crosshair(
            bars,
            &self.vp,
            self.instrument.price.tick,
            self.period_ms,
            window.scale_factor(),
            self.crosshair_mode,
            x,
            y,
        )
        .map(|c| c.price))
    }

    /// Send a strip order: the DOM's intent for it (`chart_trade::strip_command`), through the
    /// window like an exit (a linked DOM's switch), or say why not.
    fn strip_send(
        &mut self,
        o: crate::chart_trade::StripOrder,
        at: Option<Price>,
        cx: &mut Context<Self>,
    ) {
        self.send_entry(|ctx| crate::chart_trade::strip_command(o, at, ctx), cx);
    }

    /// Send an entry built from the chart's size and bracket (a strip order, a menu order):
    /// through the window like an exit (a linked DOM's switch), or say why not.
    fn send_entry(
        &mut self,
        build: impl FnOnce(
            &crate::dom_trade::EntryCtx,
        ) -> Result<lines_trading::Command, crate::dom_trade::Refusal>,
        cx: &mut Context<Self>,
    ) {
        use crate::chart_trade::{ENABLE_TO_ORDER, LINK_TO_TRADE};
        let issued = std::time::Instant::now();
        let built = if self.common.link.is_none() {
            Err(LINK_TO_TRADE)
        } else {
            crate::trade_bar::entry_ctx(self, cx).and_then(|ctx| {
                build(&ctx)
                    .map(|c| (c, ctx))
                    .map_err(|crate::dom_trade::Refusal(why)| why)
            })
        };
        match built {
            Ok((command, ctx)) => match crate::trade_bar::size_refusal(self, &command, cx) {
                Some(why) => self.trade.flash(why),
                None => cx.emit(ChartOrder {
                    command,
                    refused: ENABLE_TO_ORDER,
                    resize: None,
                    account: ctx.account,
                    contract: ctx.contract,
                    link: self.common.link,
                    issued,
                }),
            },
            Err(why) => self.trade.flash(why),
        }
        cx.notify();
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.set_hover(ev.position, cx);
        // Under the pointer now, and hovered since the pointer last moved.
        let (tx, ty) = self.local(ev.position);
        let typing = self.trade.resize_edit.as_ref().map(|(id, _)| id.0);
        let hovered = self
            .ct
            .resize_target(tx as f32, ty as f32, typing)
            .map(|(id, _)| id)
            .filter(|id| self.ct.resize_hover == Some(*id))
            .map(lines_trading::OrderId);
        if crate::trade_bar::wheel_resize(self, hovered, ev, window, cx) {
            return;
        }
        // The chart moves under the pointer: no tag is hovered until it moves.
        self.ct.resize_hover = None;
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
                        // Units: m h d w; trade-built t (ticks), v (volume), r (range), s (seconds).
                        && (c.as_bytes()[0].is_ascii_digit() || "mhdwtvrsMHDWTVRS".contains(*c))
                }) && entry.len() < 8
                {
                    entry.push_str(c);
                }
            }
        }
    }

    // ---- paint ---------------------------------------------------------------------

    fn paint_chart(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let t0 = Instant::now();
        // Screenshot fixture: the first group tag's list, once a frame has laid tags out.
        if self.pending_group_menu {
            if let Some(g) = self.ct.groups[..self.ct.n_groups]
                .iter()
                .find(|g| g.is_group())
                .copied()
            {
                self.pending_group_menu = false;
                let bottom = self.ct.tags[..self.ct.n_tags]
                    .iter()
                    .find(|b| b.id == g.id)
                    .map_or(tag_h_now(), |b| b.top + b.h);
                self.open_order_group(g.id, bottom, cx);
            }
            cx.notify();
        }
        self.common.paints += 1;
        let scale = window.scale_factor();
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let plot_w = (w - axis_w()).max(10.0);
        let above_axis = (h - axis_h()).max(10.0);
        // Lower panes (cumulative delta, Volume) take their shares below the plot, above the
        // time axis.
        let lower = self.studies.lower_count().min(crate::subpane::MAX_PANES);
        self.split = (self.visibility.draws_studies() && lower > 0)
            .then(|| {
                crate::subpane::stack_at_scale(above_axis, &self.subpane_ratios[..lower], scale)
            })
            .flatten();
        self.above_axis = above_axis;
        let plot_h = self.split.map_or(above_axis, |s| s.price_h);
        self.footprint.ensure_capacity(plot_w);
        self.chart = bounds;
        if self.styling() && self.editor_pane != (w, above_axis) {
            // The style editor is placed from the pane's size: place it again.
            cx.notify();
        }
        // The HUD's form follows the chart's width: rebuild it when the width crosses over.
        if self.latency_hud && hud_compact(w) != self.hud_compact {
            self.hud_at = None;
            cx.notify();
        }
        let plot = Bounds::new(bounds.origin, size(px(plot_w), px(plot_h)));
        if self.order_strip && plot.size != self.plot.size {
            // The strip is placed from the plot's size (render): place it again.
            cx.notify();
        }
        self.plot = plot;
        if self.ct.strip_pick.is_some() && !self.strip_box().is_some_and(|b| b.limits) {
            // Its button is gone (narrow plot): no pick without a way to see and cancel it.
            self.ct.strip_pick = None;
            cx.notify();
        }
        self.vp.set_size(plot_w as f64, plot_h as f64);
        // Only allocates when the window grows wider than ever before.
        self.scene.ensure_capacity(plot_w as f64);
        let marks = self.scene.candles.capacity();
        if self.tick_marks.capacity() < marks {
            self.tick_marks.reserve_exact(marks - self.tick_marks.len());
        }
        if self.font.is_none() {
            self.font = Some(window.text_style().font());
        }
        if !self.space_measured {
            self.measure_grid_space(window);
        }

        // Sample the feed. If it holds the lock this instant, redraw the previous scene.
        if let Some(d) = self.store.try_read() {
            // Receive → paint (latency HUD), only for frames that draw the data: one relaxed
            // load while hidden.
            if lines_core::latency::enabled() {
                self.paint_latency.on_paint(
                    self.store.chart_revision(),
                    self.store.chart_data_age_us(),
                    (lines_feed::store::mono_us() / 1000) as i64,
                );
            }
            // A higher timeframe: bring its bars up to date (only what changed).
            if let Some(dv) = &mut self.derived {
                dv.update(&d.series, lines_feed::catalog::sessions(d.product.id));
            }
            let view = bars_view(&self.derived, self.tick_key(), &d);
            // `None` while the tick series rebuilds (no bars yet: nothing to count from).
            let trimmed = match (self.tick_key(), &self.derived) {
                (Some(k), _) => d.ticks.bars(k).map(lines_core::TickBars::trimmed),
                (None, Some(dv)) => Some(dv.trimmed()),
                (None, None) => Some(d.series.trimmed()),
            };
            self.series_len = view.len();
            self.last_trade = d.last_trade;
            let fmt = self.instrument.price;
            let bars = view.bars;
            // The level labels' gutter, right of the newest bar: from the whole level set (never
            // the view), so it changes only with the levels, the label settings or the zoom,
            // never on a pan, a zoom of the time axis or a price move. The view settles and
            // the prices fit with it applied.
            let labels_on = self.visibility.draws_studies() && self.visibility.draws_labels();
            let (need, min_cut) = if labels_on {
                // Label widths were measured in render (`measure_labels`).
                self.levels.gutter_need()
            } else {
                (0.0, 0.0)
            };
            // 130 px at 100 % zoom; on a narrow plot a quarter of it, or what the shortest
            // useful cut needs, but never more than 40 % of it. Not even that: no labels at
            // all (`gutter_plan`).
            let plan =
                lines_core::levels::gutter_plan(need, min_cut, plot_w, crate::zoom::f(GUTTER_CAP));
            self.gutter = lines_core::levels::settle_gutter(
                &mut self.vp,
                bars.len(),
                self.gutter,
                if plan.fits { need } else { 0.0 },
                plan.cap,
            );
            self.levels.set_gutter(self.gutter, plot_w, plan.fits);
            keep_view(
                &mut self.vp,
                &mut self.series_trimmed,
                &mut self.tf_anchor,
                trimmed,
                bars,
                self.drag.is_some(),
            );
            self.vp.on_data(bars, fmt.tick);
            if self.visibility.draws_studies()
                && self.footprint.config().shown()
                && self.vp.auto_price
                && !self.vp.price_held
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
            // The inset again for the settled view (a new bar while following moves it).
            self.vp.right_inset =
                lines_core::levels::gutter_inset(f64::from(self.gutter), &self.vp, bars.len());
            if self.visibility.draws_studies() {
                self.levels.place(&d, &self.vp);
            }
            if display_tz::epoch() != self.zone_epoch {
                // Times read in the new zone: cached time labels and the readout go.
                // The epoch first: a change between the reads shows on the next frame.
                self.zone_epoch = display_tz::epoch();
                self.zone = display_tz::current();
                self.labels.clear();
                self.readout = None;
            }
            self.scene
                .build(bars, &self.vp, &fmt, scale, self.period_ms, &self.zone);
            // Tick bars: which drawn candles are partial or start a session, and where the
            // "tick bars from" label goes.
            self.tick_marks.clear();
            self.tick_from = None;
            // The feed's clock: a closed session's last bar is partial, not forming.
            let now = self.store.feed_clock();
            if let (Some(tb), Some(key)) = (view.trades, self.tick_key()) {
                let first = self.vp.visible_range(bars.len()).start;
                for k in 0..self.scene.candles.len() {
                    if self.tick_marks.len() == self.tick_marks.capacity() {
                        break;
                    }
                    let i = first + k;
                    let mut m = 0;
                    if tb.is_partial_at(i, now) || tb.is_forming_at(i, now) {
                        m |= MARK_PARTIAL;
                    }
                    if i > 0 && tb.starts_session(i) {
                        m |= MARK_SESSION;
                    }
                    self.tick_marks.push(m);
                }
                if let lines_feed::ticks::TickStatus::From(t) =
                    d.ticks.status(key, &d.series, &d.product)
                    && !bars.is_empty()
                {
                    let x = slot_x(&self.vp, 0.0, scale);
                    if x >= 0.0 && f64::from(x) < self.vp.width {
                        self.tick_from = Some((t, x));
                    }
                }
            }
            // Hit-test after the viewport update so the crosshair matches this frame. In the
            // sub-pane only the time follows the pointer (the plot's y is the top row).
            self.cross_in_sub = self.hover.and_then(|(_, y)| {
                let s = self.split?;
                let k = s.pane_at(y)?;
                Some((k, y - s.panes()[k].sub_top))
            });
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
            let hovered = self.cross.and_then(|c| c.bar);
            self.cross_partial = match (hovered, &self.derived, view.trades) {
                (Some(i), _, Some(tb)) => tb.is_partial_at(i, now),
                (Some(i), Some(dv), None) => dv.is_partial(i),
                _ => false,
            };
            self.cross_tick = hovered.zip(view.trades).and_then(|(i, tb)| {
                let state = if tb.is_partial_at(i, now) {
                    TickState::Partial
                } else if tb.is_forming_at(i, now) {
                    TickState::Forming
                } else {
                    TickState::Full
                };
                Some((tb.last_trade(i)?, tb.count(i), tb.kind().ticks(), state))
            });
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
            self.marks.prepare(
                view,
                &self.vp,
                f32::from(self.plot.size.width),
                f32::from(self.plot.size.height),
            );
            // The legend says why footprint bars are candles: re-render when that changes.
            let fb = self.footprint.fallbacks();
            if fb != self.fp_seen {
                self.fp_seen = fb;
                cx.notify();
            }
            // Drawings: projected for this timeframe; previews and drags follow the pointer.
            let magnet = self.magnet();
            // Replayed every frame (screenshots have no pointer), so the measure settles on
            // the frame's final viewport, never a first frame's before the price fit.
            if let Some((a, b)) = self.shift_drag {
                let axis = TimeAxis::new(view);
                let at = |(x, y): (f32, f32)| {
                    snap_anchor(&axis, &self.vp, fmt.tick, magnet, x.into(), y.into())
                };
                let (from, to) = (at(a), at(b));
                self.drawings.quick_down(a.0, a.1, from);
                self.drawings.on_up(b.0, b.1, to);
                if let Some((m0, m1)) = self.move_measure
                    && let (Some(f), Some(t)) = (at(m0), at(m1))
                {
                    self.drawings.screenshot_move_measure(f, t, &axis, fmt.tick);
                }
            }
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
        tm: &lines_core::geometry::order_tags::Metrics,
        window: &mut Window,
        cx: &mut App,
    ) -> lines_core::geometry::order_tags::TagBox {
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
        let s = tm.scale;
        let w_money = if shown {
            8.0 * s + self.pos_glyphs.width(money.as_str(), tone, true, window)
        } else {
            0.0
        };
        // Sized like the order tags (`tm`), so its handles (`order_tags::handles`) fit it.
        let (x, h) = (ox + (4.0 * s).round(), tm.tag_h);
        let top = oy + y - h * 0.5;
        let w = w_head + w_money + tm.text_pad;
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
            x + 6.0 * s,
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
                x + 6.0 * s + w_head + 8.0 * s,
                top,
                h,
                tone,
                true,
                window,
                cx,
            );
        }
        lines_core::geometry::order_tags::TagBox {
            id: u64::MAX,
            price: Price(avg.round() as i64),
            line_y: y,
            left: x - ox,
            top: top - oy,
            w,
            h,
            close_left: x - ox + w,
        }
    }

    /// Copy this frame's orders and position for the chart's account and contract; refused
    /// moves become the chart's refusal tag.
    fn prepare_orders(&mut self, cx: &App) {
        let Some(t) = crate::trading::Trading::try_global(cx).filter(|_| !self.view_only) else {
            self.ct.n_tags = 0;
            return;
        };
        let contract = crate::trade_bar::contract_of(&self.store);
        let refused = self
            .ct
            .prepare(&t.cache, self.trade.account.as_ref(), contract);
        // Screenshot fixture: the position line held dragged (a preview, never placed: it sets
        // only `adding`, never `pos_drag` or `moved`, so a release would see a click; and the
        // flag exists only with `--screenshot`, which has no pointer).
        if let (Some(y), Some((qty, avg))) = (self.drag_position, self.ct.position)
            && let Some(to) =
                lines_core::geometry::snap_price(&self.vp, y as f64, self.instrument.price.tick)
        {
            self.ct.adding = lines_core::geometry::order_tags::position_drag_exit(qty, avg, to)
                .map(|target| crate::chart_trade::AddExit { target, to });
        }
        let shown = cx
            .try_global::<crate::picker_view::AppSettings>()
            .map(|a| a.settings.pnl_display)
            .unwrap_or_default()
            .shown();
        for (p, why) in refused {
            let price = self.instrument.price.format::<32>(p);
            // P&L off: a refusal keeps its reason, never an amount.
            let why = if shown {
                std::borrow::Cow::Borrowed(why.as_str())
            } else {
                crate::pnl::redact_amounts(&why)
            };
            self.trade.flash(format!("{}: {why}", price.as_str()));
        }
    }

    /// An order's colour on the chart: its role's (target, stop), else its side's, as on the
    /// DOM.
    fn mark_colour(&self, m: &crate::dom_trade::OrderMark) -> Colour {
        let t = &self.theme;
        match crate::dom_trade::MarkRole::of(m) {
            crate::dom_trade::MarkRole::Target => t.order_target,
            crate::dom_trade::MarkRole::Stop => t.order_stop,
            crate::dom_trade::MarkRole::Entry if m.side == lines_trading::Side::Buy => t.up,
            crate::dom_trade::MarkRole::Entry => t.down,
        }
    }

    /// The chart's working and held orders (roadmap 8, phase 1): a line at each price (solid
    /// working, dashed held or pending), a tag at the right edge (`S1 · 5806.50 · -$175.00`,
    /// × cancels; drag to move), edge markers for orders off the plot (click: scroll there),
    /// the position tag's add-stop / add-target handles, the dragged order, ghosts and the
    /// exit being added, and the newest refusal. Laid out by
    /// `lines_core::geometry::order_tags`; allocation-free (fixed buffers, cached glyphs).
    #[allow(clippy::too_many_arguments)]
    fn draw_orders(
        &mut self,
        pos_tag: Option<lines_core::geometry::order_tags::TagBox>,
        (ox, oy): (f32, f32),
        (pw, ph): (f32, f32),
        hair: f32,
        scale: f32,
        tm: &lines_core::geometry::order_tags::Metrics,
        mode: PnlDisplay,
        window: &mut Window,
        cx: &mut App,
    ) {
        use crate::chart_trade::{
            TagText, money_per_unit, write_edge, write_exit_pnl, write_order_tag,
        };
        use crate::dom_trade::group_key;
        use crate::dom_trade::{MarkRole, MarkStyle};
        use lines_core::geometry::order_tags::{
            GroupIn, LineIn, MIXED_KEY, edge_markers, group_orders, handles, layout_tags, tag_band,
            tags_fit, wanted_handles,
        };
        // The order tags' own height and padding (their layout's, at the UI zoom), not the
        // price labels'.
        let order_tag_h = tm.tag_h;
        let s = tm.scale;
        use lines_core::Qty;
        let t = self.theme;
        let fmt = self.instrument.price;
        let mpu = money_per_unit(&self.instrument);
        let rect = |x: f32, y: f32, w: f32, h: f32| {
            Bounds::new(point(px(ox + x), px(oy + y)), size(px(w), px(h)))
        };
        let dashed = |window: &mut Window, y: f32, c: Colour, dash: f32| {
            let mut x = 0.0;
            while x < pw {
                window.paint_quad(fill(rect(x, y, dash.min(pw - x), hair), c));
                x += dash * 2.0;
            }
        };
        // Every order, for the edge markers.
        self.ct.lines.clear();
        for i in 0..self.ct.marks.len() {
            let m = self.ct.marks[i];
            self.ct.lines.push(LineIn {
                id: m.id.0,
                price: m.price,
                text_w: 0.0,
            });
        }
        // Edge markers (measured from their text), then the tags between them.
        let first = edge_markers(&self.vp, &self.ct.lines, pw, ph, [0.0, 0.0], tm);
        let mut edge_text = [FixedStr::<64>::new(), FixedStr::<64>::new()];
        let mut edge_w = [0.0f32; 2];
        for k in 0..2 {
            if let Some(e) = first[k]
                && let Some(m) = self.ct.mark(e.id).copied()
            {
                let _ = write_edge(&mut edge_text[k], &m, e.above, e.n, &fmt);
                let mut pnl = FixedStr::<48>::new();
                // One order's P&L, so only when it stands for one order.
                if e.n == 1
                    && write_exit_pnl(&mut pnl, &m, self.ct.position, fmt.tick, mpu, mode)
                        .ok()
                        .flatten()
                        .is_some()
                {
                    let _ = write!(edge_text[k], " · {}", pnl.as_str());
                }
                edge_w[k] = self
                    .pos_glyphs
                    .width(edge_text[k].as_str(), t.text_main, true, window);
            }
        }
        self.ct.markers = edge_markers(&self.vp, &self.ct.lines, pw, ph, edge_w, tm);
        let band = tag_band(&self.ct.markers, ph, tm);
        // The tags: orders of one kind that can't sit at their own levels share one (zooming in
        // splits them back), and every order on the plot is in exactly one.
        self.ct.on_plot.clear();
        for i in 0..self.ct.marks.len() {
            let m = self.ct.marks[i];
            let y = self.vp.price_to_y(m.price.0 as f64);
            if y.is_finite() && (0.0..=ph as f64).contains(&y) {
                self.ct.on_plot.push(GroupIn {
                    id: m.id.0,
                    price: m.price,
                    qty: m.qty.0 - m.filled.0,
                    key: group_key(&m),
                });
            }
        }
        self.ct.n_groups = group_orders(
            &self.vp,
            &mut self.ct.on_plot,
            ph,
            tags_fit(band, tm),
            tm,
            &mut self.ct.groups,
        );
        // Texts and widths, one per tag.
        self.ct.texts.clear();
        self.ct.tag_lines.clear();
        for k in 0..self.ct.n_groups {
            let g = self.ct.groups[k];
            let Some(m) = self.ct.mark(g.id).copied() else {
                continue;
            };
            let mut pnl = FixedStr::<48>::new();
            let mut text = FixedStr::<96>::new();
            let (money, at) = if g.is_group() {
                let _ = crate::chart_trade::write_group_tag(&mut text, &m, &g, &fmt);
                (None, g.average())
            } else {
                let _ = write_order_tag(&mut text, &m, &fmt);
                let money = write_exit_pnl(&mut pnl, &m, self.ct.position, fmt.tick, mpu, mode)
                    .ok()
                    .flatten();
                (money, m.price)
            };
            let mut w = self
                .pos_glyphs
                .width(text.as_str(), t.text_main, true, window);
            if money.is_some() {
                w += 8.0 * s
                    + self
                        .pos_glyphs
                        .width(pnl.as_str(), t.text_main, true, window);
            }
            self.ct.texts.push(TagText {
                id: g.id,
                text,
                pnl,
                money,
                mark: m,
            });
            self.ct.tag_lines.push(LineIn {
                id: g.id,
                price: at,
                text_w: w,
            });
        }
        self.ct.n_tags = layout_tags(
            &self.vp,
            &self.ct.tag_lines,
            pw,
            ph,
            band,
            scale,
            tm,
            &mut self.ct.tags,
        );
        // Lines across the plot: solid working, dashed held or pending. A group of one kind:
        // a band over its price range between two such lines (one line at one price).
        for k in 0..self.ct.n_groups {
            let g = self.ct.groups[k];
            let line =
                |window: &mut Window, m: &crate::dom_trade::OrderMark, p: Price, c: Colour| {
                    let y = self.vp.price_to_y(p.0 as f64) as f32;
                    if (0.0..=ph).contains(&y) {
                        match m.style {
                            MarkStyle::Working | MarkStyle::Partial => {
                                window.paint_quad(fill(rect(0.0, y, pw, hair), c.opacity(0.8)))
                            }
                            _ => dashed(window, y, c.opacity(0.8), 3.0),
                        }
                    }
                };
            if g.is_group() && g.key != MIXED_KEY {
                let Some(m) = self.ct.mark(g.id) else {
                    continue;
                };
                let c = self.mark_colour(m);
                let (top, bottom) = (
                    self.vp.price_to_y(g.hi.0 as f64) as f32,
                    self.vp.price_to_y(g.lo.0 as f64) as f32,
                );
                if bottom > top {
                    window.paint_quad(fill(rect(0.0, top, pw, bottom - top), c.opacity(0.14)));
                }
                line(window, m, g.hi, c);
                if g.lo != g.hi {
                    line(window, m, g.lo, c);
                }
            } else {
                for o in self.ct.members(&g) {
                    if let Some(m) = self.ct.mark(o.id) {
                        line(window, m, o.price, self.mark_colour(m));
                    }
                }
            }
        }
        // The order being dragged, moves awaiting the broker: dashed accent.
        let drag = self.ct.drag.map(|d| d.to);
        for p in drag.into_iter().chain(
            self.ct
                .ghosts
                .iter()
                .filter(|g| g.qty.is_none())
                .map(|g| g.price),
        ) {
            let y = self.vp.price_to_y(p.0 as f64) as f32;
            if (0.0..=ph).contains(&y) {
                dashed(window, y, t.accent, 5.0);
            }
        }
        // The exit being added: dashed in its role's colour, with its tag-to-be.
        if let (Some(a), Some((qty, _))) = (self.ct.adding, self.ct.position) {
            let y = self.vp.price_to_y(a.to.0 as f64) as f32;
            if (0.0..=ph).contains(&y) {
                let c = if a.target {
                    t.order_target
                } else {
                    t.order_stop
                };
                dashed(window, y, c, 5.0);
                let preview = crate::dom_trade::OrderMark {
                    id: lines_trading::OrderId(0),
                    side: if qty > 0 {
                        lines_trading::Side::Sell
                    } else {
                        lines_trading::Side::Buy
                    },
                    price: a.to,
                    style: MarkStyle::Pending,
                    qty: Qty(qty.abs()),
                    filled: Qty(0),
                    stop: !a.target,
                    role: lines_trading::order::Role::Single,
                    exit: true,
                };
                let mut pnl = FixedStr::<48>::new();
                let mut text = FixedStr::<96>::new();
                let _ = write_order_tag(&mut text, &preview, &fmt);
                let money =
                    write_exit_pnl(&mut pnl, &preview, self.ct.position, fmt.tick, mpu, mode)
                        .ok()
                        .flatten();
                let head_w = self
                    .pos_glyphs
                    .width(text.as_str(), t.text_main, true, window);
                let pnl_w = if money.is_some() {
                    8.0 * s
                        + self
                            .pos_glyphs
                            .width(pnl.as_str(), t.text_main, true, window)
                } else {
                    0.0
                };
                let w = head_w + pnl_w + tm.text_pad;
                let left = (pw - tm.tag_gap - w).max(0.0);
                let top = (y - order_tag_h * 0.5).clamp(0.0, (ph - order_tag_h).max(0.0));
                window.paint_quad(fill(rect(left, top, w, order_tag_h), t.label_bg));
                self.outline(rect(left, top, w, order_tag_h), c, hair, window);
                self.pos_glyphs.paint(
                    text.as_str(),
                    ox + left + 6.0 * s,
                    oy + top,
                    order_tag_h,
                    t.text_main,
                    true,
                    window,
                    cx,
                );
                if let Some(money) = money {
                    let tone = match crate::pnl::tone(money) {
                        crate::pnl::Tone::Up => t.up,
                        crate::pnl::Tone::Down => t.down,
                        crate::pnl::Tone::Flat => t.text_main,
                    };
                    self.pos_glyphs.paint(
                        pnl.as_str(),
                        ox + left + 6.0 * s + head_w + 8.0 * s,
                        oy + top,
                        order_tag_h,
                        tone,
                        true,
                        window,
                        cx,
                    );
                }
            }
        }
        // Tags.
        for k in 0..self.ct.n_tags {
            let b = self.ct.tags[k];
            let Some(tt) = self.ct.texts.iter().find(|x| x.id == b.id).copied() else {
                continue;
            };
            let m = tt.mark;
            let group = self.ct.group_of(b.id).copied();
            let mixed = group.is_some_and(|g| g.key == MIXED_KEY);
            let c = if mixed {
                t.text_dim
            } else {
                self.mark_colour(&m)
            };
            let pending = !mixed && m.style == MarkStyle::Pending;
            // Always on the label background (readable over candles); pending reads by its dim
            // text and outline only (no role bar), held by a dashed outline.
            window.paint_quad(fill(rect(b.left, b.top, b.w, order_tag_h), t.label_bg));
            if !mixed && m.style == MarkStyle::Held {
                self.dashed_outline(rect(b.left, b.top, b.w, order_tag_h), c, hair, window);
            } else {
                self.outline(rect(b.left, b.top, b.w, order_tag_h), c, hair, window);
            }
            // The role's shape, as on the DOM: a notch on the top edge for a target, on the
            // bottom edge for a stop; entries plain. Never colour alone.
            window.paint_quad(fill(rect(b.left, b.top, 2.0, order_tag_h), c));
            match MarkRole::of(&m) {
                _ if mixed => {}
                MarkRole::Target => {
                    window.paint_quad(fill(rect(b.left, b.top - 2.0, (8.0 * s).round(), 2.0), c))
                }
                MarkRole::Stop => window.paint_quad(fill(
                    rect(b.left, b.top + order_tag_h, (8.0 * s).round(), 2.0),
                    c,
                )),
                MarkRole::Entry => {}
            }
            window.paint_quad(fill(rect(b.close_left, b.top, hair, order_tag_h), c));
            let fg = if pending { t.text_dim } else { t.text_main };
            let head_w = self.pos_glyphs.width(tt.text.as_str(), fg, true, window);
            self.pos_glyphs.paint(
                tt.text.as_str(),
                ox + b.left + 6.0 * s,
                oy + b.top,
                order_tag_h,
                fg,
                true,
                window,
                cx,
            );
            // The P&L at its price, signed and in its tone (never colour alone).
            if let Some(money) = tt.money {
                let tone = match crate::pnl::tone(money) {
                    crate::pnl::Tone::Up => t.up,
                    crate::pnl::Tone::Down => t.down,
                    crate::pnl::Tone::Flat => t.text_main,
                };
                self.pos_glyphs.paint(
                    tt.pnl.as_str(),
                    ox + b.left + 6.0 * s + head_w + 8.0 * s,
                    oy + b.top,
                    order_tag_h,
                    tone,
                    true,
                    window,
                    cx,
                );
            }
            // × cancels; a group's ▾ lists its orders.
            self.pos_glyphs.paint(
                if group.is_some() { "▾" } else { "×" },
                ox + b.close_left + tm.close_w * 0.5 - 3.5 * s,
                oy + b.top,
                order_tag_h,
                t.text_main,
                true,
                window,
                cx,
            );
        }
        // Screenshot fixture: as if the first single-order tag were hovered (a frame later its
        // + knows whether it can grow).
        if self.ct.resize_hover.is_none()
            && crate::trading::Trading::try_global(cx).is_some_and(|t| t.start_order_controls)
        {
            self.ct.resize_hover = self.ct.tags[..self.ct.n_tags]
                .iter()
                .find(|b| self.ct.group_of(b.id).is_none())
                .map(|b| b.id);
            window.refresh();
        }
        // The hovered tag's − / size / +, left of it (a tag's height, the size twice that; never under 100 %).
        // While its size is typed the field stays up, wherever the pointer is.
        let shown = self
            .trade
            .resize_edit
            .as_ref()
            .map(|(id, _)| id.0)
            .or(self.ct.resize_hover)
            .filter(|h| self.ct.group_of(*h).is_none());
        if let Some(b) = shown
            .and_then(|h| self.ct.tags[..self.ct.n_tags].iter().find(|b| b.id == h))
            .copied()
            .filter(|b| b.left >= crate::chart_trade::resize_w(b))
            && let Some(m) = self.ct.mark(b.id).copied()
        {
            let (btn_w, value_w) = crate::chart_trade::resize_parts(&b);
            let left = b.left - crate::chart_trade::resize_w(&b);
            let mut value = lines_core::FixedStr::<16>::new();
            match &self.trade.resize_edit {
                Some((id, digits)) if id.0 == b.id => {
                    let _ = write!(value, "{digits}_");
                }
                _ => {
                    let _ = write!(value, "{}", (m.qty.0 - m.filled.0).max(0));
                }
            }
            let plus = if self.ct.resize_grows {
                t.text_main
            } else {
                t.text_dim
            };
            let parts = [
                (left, btn_w, "−", t.text_main),
                (left + btn_w, value_w, value.as_str(), t.text_main),
                (left + btn_w + value_w, btn_w, "+", plus),
            ];
            for (x, w, text, colour) in parts {
                let r = rect(x, b.top, w, order_tag_h);
                window.paint_quad(fill(r, t.panel));
                self.outline(r, t.control_edge, hair, window);
                self.pos_glyphs.paint(
                    text,
                    ox + x + 6.0 * s,
                    oy + b.top,
                    order_tag_h,
                    colour,
                    true,
                    window,
                    cx,
                );
            }
        }
        // Edge markers: orders off the plot, nearest first; click scrolls there.
        for (k, text) in edge_text.iter().enumerate() {
            let Some(e) = self.ct.markers[k] else {
                continue;
            };
            let c = self
                .ct
                .mark(e.id)
                .map_or(t.text_main, |m| self.mark_colour(m));
            window.paint_quad(fill(rect(e.left, e.top, e.w, order_tag_h), t.label_bg));
            self.outline(rect(e.left, e.top, e.w, order_tag_h), c, hair, window);
            self.pos_glyphs.paint(
                text.as_str(),
                ox + e.left + 6.0 * s,
                oy + e.top,
                order_tag_h,
                t.text_main,
                true,
                window,
                cx,
            );
        }
        // The position tag's handles: add a target or a stop while the position has none.
        // Live in the exit's colour with trading enabled; dim otherwise (a press says how).
        self.ct.handles = [None, None];
        self.ct.pos_tag = pos_tag;
        if let (Some(tag), Some((qty, _))) = (pos_tag, self.ct.position) {
            let (has_t, has_s) = self.ct.exits();
            self.ct.handles = handles(&tag, wanted_handles(qty, has_t, has_s), tm);
            for h in self.ct.handles.iter().flatten() {
                let target = lines_core::geometry::order_tags::handle_is_target(qty, h.above);
                let c = if target { t.order_target } else { t.order_stop };
                let r = rect(h.left, h.top, h.w, h.h);
                window.paint_quad(fill(r, t.label_bg));
                self.outline(r, c, hair, window);
                let label = if target { "+T" } else { "+S" };
                let lw = self.pos_glyphs.width(label, c, true, window);
                self.pos_glyphs.paint(
                    label,
                    ox + h.left + (h.w - lw) * 0.5,
                    oy + h.top,
                    h.h,
                    c,
                    true,
                    window,
                    cx,
                );
            }
        }
        // The newest refusal from this chart (a move, an exit, trading off), at the plot's
        // top left: inline and brief, never a dialog.
        if let Some(why) = self.trade.local_flash() {
            let w = self.pos_glyphs.width(why, t.on_warn, true, window) + tm.text_pad;
            window.paint_quad(fill(
                rect((4.0 * s).round(), (4.0 * s).round(), w, order_tag_h),
                t.warn,
            ));
            self.pos_glyphs.paint(
                why,
                ox + 10.0 * s,
                oy + 4.0 * s,
                order_tag_h,
                t.on_warn,
                true,
                window,
                cx,
            );
        }
    }

    /// A 1-device-pixel outline.
    fn outline(&self, b: Bounds<Pixels>, c: Colour, hair: f32, window: &mut Window) {
        let (x, y) = (f32::from(b.origin.x), f32::from(b.origin.y));
        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
        let r =
            |x: f32, y: f32, w: f32, h: f32| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
        for q in [
            r(x, y, w, hair),
            r(x, y + h - hair, w, hair),
            r(x, y, hair, h),
            r(x + w - hair, y, hair, h),
        ] {
            window.paint_quad(fill(q, c));
        }
    }

    /// A dashed 1-device-pixel outline (held orders).
    fn dashed_outline(&self, b: Bounds<Pixels>, c: Colour, hair: f32, window: &mut Window) {
        let (x, y) = (f32::from(b.origin.x), f32::from(b.origin.y));
        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
        let r =
            |x: f32, y: f32, w: f32, h: f32| Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
        let mut dx = 0.0;
        while dx < w {
            let seg = 3.0f32.min(w - dx);
            window.paint_quad(fill(r(x + dx, y, seg, hair), c));
            window.paint_quad(fill(r(x + dx, y + h - hair, seg, hair), c));
            dx += 6.0;
        }
        window.paint_quad(fill(r(x, y, hair, h), c));
        window.paint_quad(fill(r(x + w - hair, y, hair, h), c));
    }

    fn draw(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let t = self.theme;
        let position = self.position_for_draw(cx);
        self.prepare_orders(cx);
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
            // No bars: no price grid either (an empty chart has no scale to show).
            let grid = if self.series_len == 0 {
                &self.scene.price_lines[..0]
            } else {
                &self.scene.price_lines[..]
            };
            for l in grid {
                window.paint_quad(fill(rect(0.0, l.y, pw, hair), t.grid));
            }
            for l in &self.scene.time_lines {
                window.paint_quad(fill(rect(l.x, 0.0, hair, ph), t.grid));
            }
            // Paint order inside the plot: grid, volume profile, the levels' zones and VWAP
            // band fills (behind the candles: a heavy fill never covers a bar), candles,
            // `layers`, footprint cell fills, indicator studies (VWAP lines and band edges),
            // the levels' lines, the footprint's numbers on their cells' color, the levels' labels (in their gutter right of the
            // newest bar), drawings, strategy markers, the profile's label, then the
            // last-price line (above every study fill and line), crosshair.
            let (studies, labels) = (
                self.visibility.draws_studies(),
                self.visibility.draws_labels(),
            );
            if studies {
                self.profile.paint(plot, &self.vp, &t, window, cx);
                self.levels.paint_back(plot, &t, window);
                self.studies.paint_back((ox, oy), &self.vp, &t, window);
            }
            // Tick charts: a faint dashed line where each session's bars begin, behind the
            // bars (dashed: never mistaken for the crosshair's solid line).
            let half_slot = (self.vp.bar_px * 0.5) as f32;
            for (k, c) in self.scene.candles.iter().enumerate() {
                if self
                    .tick_marks
                    .get(k)
                    .is_some_and(|m| m & MARK_SESSION != 0)
                {
                    let x = snap(f64::from(c.x - half_slot), scale);
                    let mut y = 0.0;
                    while y < ph {
                        let d = DIVIDER_DASH.min(ph - y);
                        window.paint_quad(fill(rect(x, y, hair, d), t.session_divider));
                        y += DIVIDER_DASH + DIVIDER_GAP;
                    }
                }
            }
            let fp_first = self.footprint.first_bar();
            let fp_on = studies && self.footprint.is_active();
            for (k, c) in self.scene.candles.iter().enumerate() {
                // The footprint draws this bar as cells (only where it has trade data).
                if studies && self.footprint.covers(fp_first + k) {
                    continue;
                }
                // Beside footprint columns, a bar less than half on the plot is left out (its
                // candle would show as a stray stub at the edge).
                if fp_on && (c.x < 0.0 || c.x >= pw) {
                    continue;
                }
                if self
                    .tick_marks
                    .get(k)
                    .is_some_and(|m| m & MARK_PARTIAL != 0)
                {
                    // A partial or forming tick bar: a dashed box around it (shape, not color),
                    // on device pixels, its dashes two device pixels thick (WCAG 1.4.11).
                    let x0 = snap(f64::from(c.x - c.half_width - 3.0), scale);
                    let y0 = snap(f64::from(c.wick_top - 3.0), scale);
                    let x1 = snap(f64::from(c.x + c.half_width + 3.0), scale);
                    let y1 = snap(f64::from(c.wick_bottom + 3.0), scale);
                    dashed_box(
                        window,
                        rect(x0, y0, x1 - x0, y1 - y0),
                        2.0 * hair,
                        t.text_dim,
                    );
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
                // Footprint cells' fills, then the study and level lines over them (visible
                // across the cells), then the cells' numbers on tight boxes of their cell's
                // color: a line never runs through a digit.
                for l in &mut self.layers {
                    l.paint(plot, &self.vp, &t, window, cx);
                }
                self.footprint.paint(plot, &self.vp, &t, window, cx);
                self.studies.paint((ox, oy), &self.vp, &t, window);
                self.levels.paint_front(plot, &t, window);
                self.footprint.paint_numbers(plot, &self.vp, &t, window, cx);
                // In the gutter right of the newest bar (no bar under a label), under the
                // drawings and the strategy markers as study labels always were.
                if labels {
                    self.levels.paint_labels(plot, &t, window, cx);
                }
            }
            // Drawings: the trader's own marks, above the studies (shown unless hidden on
            // this chart; hiding studies or a clean chart leaves them).
            if !self.drawings.hidden {
                self.drawings.paint((ox, oy), hair, &t, window);
                self.paint_drawing_text((ox, oy), window, cx);
            }
            // Strategy markers: above studies and drawings, under the crosshair.
            self.marks.paint(plot, &t, window, cx);
            if labels {
                self.profile.paint_label(plot, &t, window, cx);
            }
            if let Some((from, x)) = self.tick_from {
                self.draw_tick_from(from, x, (ox, oy), ph, window, cx);
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
                hline(
                    window,
                    y,
                    0.0,
                    pw,
                    t.last.opacity(crate::theme::LAST_LINE_ALPHA),
                );
            }
            // The open position: a dashed line at its average price (never the last price's
            // solid violet).
            if let Some((_, avg, _, _)) = position {
                let y = self.vp.price_to_y(avg) as f32;
                let d = crate::theme::POSITION_DASH;
                let mut x = 0.0;
                while x < pw {
                    window.paint_quad(fill(rect(x, y, d.on.min(pw - x), hair), t.text_dim));
                    x += d.on + d.off;
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
        self.lower_close = [None; crate::subpane::MAX_PANES];
        if let Some(st) = self.split {
            for (k, s) in st.panes().iter().enumerate() {
                self.draw_subpane(k, *s, bounds, window, cx);
            }
        }

        // Axis borders.
        window.paint_quad(fill(rect(pw, 0.0, hair, ph), t.axis_line));
        window.paint_quad(fill(rect(0.0, ph, pw, hair), t.axis_line));

        let line_h = px(label_px() * 1.3);
        // The tags drawn over the price axis below; a label any of them would cover isn't
        // drawn at all (a half-covered number reads as another price).
        let last_y = self.last_trade.map(|p| last_tag_y(&self.vp, p, ph));
        // The synced price shows only with the synced time (both from the other chart).
        let sync_price = self
            .sync_price
            .filter(|_| self.cross.is_none() && self.sync_marker.is_some());
        // Drawings' price tags too (up to a few per frame, on the plot only, allocation-free).
        let mut drawing_tags = [None; MAX_DRAWING_TAGS];
        if !self.drawings.hidden {
            let mut lines = [0.0f32; MAX_DRAWING_TAGS];
            let mut n = 0;
            for (slot, (_, y, _)) in lines.iter_mut().zip(self.drawings.axis_tags(ph)) {
                *slot = y;
                n += 1;
            }
            let mut at = [0.0f32; MAX_DRAWING_TAGS];
            drawing_tag_ys(&lines[..n], last_y, ph, &mut at[..n]);
            for (slot, y) in drawing_tags.iter_mut().zip(&at[..n]) {
                *slot = Some(Span::around(*y, DRAWING_TAG_H * 0.5));
            }
        }
        let tags = [
            last_y.map(|y| Span::around(y, tag_h() * 0.5)),
            self.cross
                .filter(|_| self.cross_in_sub.is_none())
                .map(|c| Span::around(cross_tag_y(c.y, ph), tag_h() * 0.5)),
            sync_price.map(|(y, _)| Span::around(cross_tag_y(y, ph), tag_h() * 0.5)),
        ];
        // The time tag (own crosshair, else the synced time) on the time axis: time labels
        // it would cover aren't drawn either.
        let time_tag = match self.cross {
            Some(c) => c.time.map(|time| (c.x, time)),
            None => self.sync_marker,
        }
        .map(|(x, time)| {
            let (left, w) = self.time_tag_box(x, time, window);
            // Axis labels keep 8 px from it.
            Span {
                top: left - 8.0,
                bottom: left + w + 8.0,
            }
        });
        // Under footprint columns a time label is centered on its column.
        let centered = self.visibility.draws_studies() && self.footprint.is_active();
        // No bars, no prices: an empty chart never shows a made-up scale (design.md §7).
        let price_lines = if self.series_len == 0 {
            0
        } else {
            self.scene.price_lines.len()
        };
        for i in 0..price_lines {
            let l = self.scene.price_lines[i];
            if l.y < 0.0 || l.y > ph {
                continue;
            }
            let span = Span::around(l.y, label_px() * 0.65);
            if !label_clear_of_tags(span, &tags) || !label_clear_of_tags(span, &drawing_tags) {
                continue;
            }
            let origin = point(px(ox + pw + 8.0), px(oy + l.y - label_px() * 0.65));
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
        let zone = self.zone.clone();
        for i in 0..self.scene.time_lines.len() {
            let l = self.scene.time_lines[i];
            if l.x < 0.0 {
                continue;
            }
            let kind = match l.label {
                _ if tf.is_daily() => LabelKind::Date,
                TimeLabel::Time => LabelKind::Time,
                TimeLabel::TimeSeconds => LabelKind::TimeSeconds,
                TimeLabel::Date => LabelKind::Date,
                TimeLabel::DateTime => LabelKind::DateTime,
            };
            let text = |s: &mut FixedStr<32>| {
                if tf.is_daily() {
                    // Days and weeks are labeled by trade date, not the open's UTC clock.
                    let d = bar_date(tf, calendar, l.time);
                    let _ = write!(s, "{:02}-{:02}", d.month, d.day);
                } else {
                    let _ = l.label.write(l.time, &zone, s);
                }
            };
            let w = f32::from(self.label(kind, l.time.0, t.text_dim, window, text).width);
            let left = if centered { l.x - w * 0.5 } else { l.x + 4.0 };
            // Entirely on the time axis (not under the price axis), clear of the time tag.
            let span = Span {
                top: left,
                bottom: left + w,
            };
            if left < 0.0 || span.bottom > pw || !label_clear_of_tags(span, &[time_tag]) {
                continue;
            }
            let origin = point(px(ox + left), px(oy + self.above_axis + 4.0));
            self.paint_label(kind, l.time.0, origin, line_h, t.text_dim, window, cx, text);
        }
        // The axis's zone, once, in the corner under the price axis (not for days and weeks:
        // they are trade dates). A price label or tag reaching down into the corner wins: the
        // zone yields that frame (the crosshair's tag still names it).
        let zone_span = Span {
            top: self.above_axis + 4.0,
            bottom: self.above_axis + 4.0 + label_px() * 1.3,
        };
        let corner_clear = label_clear_of_tags(zone_span, &tags)
            && label_clear_of_tags(zone_span, &drawing_tags)
            && (0..price_lines).all(|i| {
                let y = self.scene.price_lines[i].y;
                y < 0.0 || y > ph || !Span::around(y, label_px() * 0.65).overlaps(zone_span)
            });
        self.corner_zone_w = None;
        if let Some(at) = self.scene.time_lines.last().map(|l| l.time)
            && !tf.is_daily()
            && corner_clear
        {
            let zone = self.zone.clone();
            let key = zone.offset_ms(at);
            let text = |s: &mut FixedStr<32>| {
                let _ = zone.write_label(at, s);
            };
            let w = f32::from(
                self.label(LabelKind::Zone, key, t.text_dim, window, text)
                    .width,
            );
            // Beside "manual" when the price scale is frozen, or not at all when both don't
            // fit (`corner_layout`; "manual" is drawn later from the same layout).
            let manual_w = (!self.vp.auto_price).then(|| self.manual_label_w(window));
            let (_, zone_x) = corner_layout(
                f32::from(bounds.size.width) - pw,
                manual_w,
                Some(w),
                crate::zoom::f(CORNER_GAP_100),
                crate::zoom::f(CORNER_MARGIN_100),
            );
            if let Some(zone_x) = zone_x {
                self.corner_zone_w = Some(w);
                let origin = point(px(ox + pw + zone_x), px(oy + self.above_axis + 4.0));
                self.paint_label(
                    LabelKind::Zone,
                    key,
                    origin,
                    line_h,
                    t.text_dim,
                    window,
                    cx,
                    text,
                );
            }
        }
        self.draw_drawing_tags(bounds, window, cx);
        if let (Some(p), Some(y)) = (self.last_trade, last_y) {
            window.paint_quad(fill(
                rect(pw + hair, y - tag_h() * 0.5, axis_w(), tag_h()),
                t.last,
            ));
            crate::layout_rec::reserved(
                window,
                (ox + pw + hair, oy + y - tag_h() * 0.5, axis_w(), tag_h()),
                "last-price-tag",
            );
            let origin = point(px(ox + pw + 8.0), px(oy + y - label_px() * 0.65));
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
        // Order and position tags at the UI zoom, one size for layout, paint and (through the
        // boxes they store) hit tests. Hit targets: never under their 100 % size.
        let tm = lines_core::geometry::order_tags::Metrics::for_ui_zoom(crate::zoom::get());
        let mut pos_tag = None;
        if let Some(pos) = position {
            let y = self.vp.price_to_y(pos.1) as f32;
            if (0.0..=ph).contains(&y) {
                pos_tag = Some(self.draw_position_tag(pos, y, (ox, oy), hair, &tm, window, cx));
            }
        }
        let mode = position.map_or_else(
            || {
                cx.try_global::<crate::picker_view::AppSettings>()
                    .map(|a| a.settings.pnl_display)
                    .unwrap_or_default()
            },
            |p| p.3,
        );
        self.draw_orders(
            pos_tag,
            (ox, oy),
            (pw, ph),
            hair,
            scale,
            &tm,
            mode,
            window,
            cx,
        );
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
            let tick = self.cross_tick;
            self.draw_readout(bar, partial, tick, bounds, window, cx);
        }
    }

    /// "manual"'s width as drawn (shaped once, cached).
    fn manual_label_w(&mut self, window: &mut Window) -> f32 {
        let accent = self.theme.accent;
        f32::from(
            self.label(LabelKind::Indicator, 0, accent, window, |s| {
                let _ = s.write_str(MANUAL_LABEL);
            })
            .width,
        )
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
        let w = self.manual_label_w(window);
        // The same layout the zone label took (`corner_layout`): beside it, or alone.
        let (x, _) = corner_layout(
            f32::from(bounds.size.width) - pw,
            Some(w),
            self.corner_zone_w,
            crate::zoom::f(CORNER_GAP_100),
            crate::zoom::f(CORNER_MARGIN_100),
        );
        let origin = point(
            px(ox + pw + x.unwrap_or(0.0)),
            px(oy + self.above_axis + 4.0),
        );
        self.paint_label(
            LabelKind::Indicator,
            0,
            origin,
            px(label_px() * 1.3),
            accent,
            window,
            cx,
            |s| {
                let _ = s.write_str(MANUAL_LABEL);
            },
        );
    }

    /// Lower pane `k`'s × under plot-local (`x`, `y`), as last drawn.
    fn close_hit(&self, x: f32, y: f32) -> Option<usize> {
        self.lower_close.iter().position(|b| {
            b.is_some_and(|(bx, by)| x >= bx && x < bx + CLOSE_BOX && y >= by && y < by + CLOSE_BOX)
        })
    }

    /// Remove lower pane `k`'s study (its ×).
    fn remove_lower(&mut self, k: usize) {
        use lines_indicators::IndicatorConfig as C;
        let Some((i, c)) = self.studies.lower_study(k) else {
            return;
        };
        let r = match c {
            C::CumDelta { .. } => StudyRef::Delta(i),
            C::Volume { .. } => StudyRef::Volume(i),
            C::External { .. } => StudyRef::External(i),
            C::Vwap { .. } => return,
        };
        let mut s = self.study_set();
        s.apply(&StudyAction::Remove(r));
        self.set_study_set(s);
    }

    /// Lower pane `k` (`crate::subpane`) below the plot: background, the divider, the time
    /// grid (shared with the plot), its study (`Studies::paint_lower`), its scale's labels on
    /// the axis, its title (with Volume's readout of the bar under the crosshair), and the
    /// crosshair's vertical line (and horizontal line when the pointer is in it).
    fn draw_subpane(
        &mut self,
        k: usize,
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
        // One scale for everything in the pane: bars, range labels, the newest tag and the
        // crosshair's value (all read `scale` below), clear of the title only when one is drawn.
        let title_h = self
            .studies
            .lower_title_h(k, self.visibility.draws_labels(), label_px());
        let scale = self.studies.lower_scale(k, s.sub_h, title_h);
        let cross_in = self.cross_in_sub.filter(|(j, _)| *j == k).map(|(_, y)| y);
        window.with_content_mask(Some(ContentMask { bounds: sub }), |window| {
            for l in &self.scene.time_lines {
                window.paint_quad(fill(rect(l.x, s.sub_top, hair, s.sub_h), t.grid));
            }
            if let Some(sc) = scale.filter(|sc| self.studies.lower_zero_line(k) && sc.contains(0.0))
            {
                window.paint_quad(fill(rect(0.0, s.sub_top + sc.y(0.0), pw, hair), t.grid));
            }
            if let Some(sc) = &scale {
                self.studies
                    .paint_lower(k, (ox, oy + s.sub_top), sc, &self.vp, &t, window);
            }
            let x = self.cross.map(|c| c.x).or(self.sync_marker.map(|(x, _)| x));
            if let Some(x) = x {
                window.paint_quad(fill(
                    rect(x - hair * 0.5, s.sub_top, hair, s.sub_h),
                    t.crosshair,
                ));
            }
            if let Some(y) = cross_in {
                window.paint_quad(fill(rect(0.0, s.sub_top + y, pw, hair), t.crosshair));
            }
        });
        // Its title at its own top-left, on the legend box (as the price plot's legends).
        let title = self
            .visibility
            .draws_labels()
            .then(|| {
                self.studies.lower_title(
                    k,
                    self.cross.and_then(|c| c.bar),
                    self.cross.and_then(|c| c.time),
                )
            })
            .flatten();
        if let Some(crate::studies::LowerTitle::Series {
            def,
            values,
            status,
            styles,
        }) = title
        {
            self.draw_series_title(s, def, &values, status, &styles, (ox, oy), window, cx);
        }
        if let Some(crate::studies::LowerTitle::Volume {
            bar,
            split,
            average,
        }) = title
        {
            self.draw_volume_title(s, bar, split, average, (ox, oy), window, cx);
        }
        if let Some(crate::studies::LowerTitle::Delta(reset, approx)) = title {
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
            let (bx, by, bh) = (8.0, s.sub_top + 4.0, label_px() * 1.6);
            window.paint_quad(fill(rect(bx, by, tw + nw + 16.0, bh), t.label_bg));
            let line_h = px(label_px() * 1.3);
            let ty = oy + by + bh * 0.5 - label_px() * 0.65;
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
        // Its × at the top-right: removes the study (a 20 px target).
        let (cbx, cby) = (pw - CLOSE_BOX - 2.0, s.sub_top + 2.0);
        if cbx > 8.0 {
            self.lower_close[k] = Some((cbx, cby));
            let close = |b: &mut FixedStr<32>| {
                let _ = b.write_str("×");
            };
            let w = f32::from(
                self.label(LabelKind::PaneClose, 0, t.text_dim, window, close)
                    .width,
            );
            let at = point(
                px(ox + cbx + (CLOSE_BOX - w) * 0.5),
                px(oy + cby + CLOSE_BOX * 0.5 - label_px() * 0.65),
            );
            self.paint_label(
                LabelKind::PaneClose,
                0,
                at,
                px(label_px() * 1.3),
                t.text_dim,
                window,
                cx,
                close,
            );
        }
        // The axis beside it and the time axis below: borders; the visible range's ends and the
        // newest value.
        window.paint_quad(fill(rect(pw, s.sub_top, hair, s.sub_h), t.axis_line));
        window.paint_quad(fill(rect(0.0, s.sub_top + s.sub_h, pw, hair), t.axis_line));
        let Some(sc) = scale else { return };
        let line_h = px(label_px() * 1.3);
        // Cumulative delta and volume are quantities: printed in the product's quantity format;
        // an external series' values are plain numbers. Cached by kind and `key` (a number's in
        // hundredths, in its own range so it never meets a quantity's).
        let qty = self.instrument.qty;
        let fmt = self.studies.lower_format(k);
        let number = fmt == crate::studies::ValueFormat::Number;
        let key = move |v: f64| {
            if number {
                (v * 100.0).round() as i64 ^ (1 << 60)
            } else {
                v.round() as i64
            }
        };
        let value = move |v: f64| {
            move |b: &mut FixedStr<32>| {
                if number {
                    crate::series_pane::write_value(v, b);
                } else {
                    let _ = qty.write(lines_core::Qty(v.round() as i64), b);
                }
            }
        };
        let marks = self.studies.lower_marks(k, &sc);
        // The newest value's tag, and range labels only where it leaves them whole (as the
        // price axis: a half-covered number reads as another value).
        let newest = self.studies.lower_last(k).map(|last| {
            let y = (s.sub_top + sc.y(last)).clamp(
                s.sub_top + tag_h() * 0.5,
                s.sub_top + s.sub_h - tag_h() * 0.5,
            );
            (last, y)
        });
        // The tag, then each label drawn: a later mark (the zero line's 0) that would cover one
        // is left out.
        let mut taken = [
            newest.map(|(_, y)| Span::around(y, tag_h() * 0.5)),
            None,
            None,
            None,
        ];
        for (i, v) in marks.into_iter().enumerate() {
            let Some(v) = v else { continue };
            let y = s.sub_top + sc.y(v);
            let span = Span::around(y, label_px() * 0.65);
            if !label_clear_of_tags(span, &taken) {
                continue;
            }
            taken[i + 1] = Some(span);
            let origin = point(px(ox + pw + 8.0), px(oy + y - label_px() * 0.65));
            self.paint_label(
                LabelKind::SubValue,
                key(v),
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
                rect(pw + hair, y - tag_h() * 0.5, axis_w(), tag_h()),
                t.text_main,
            ));
            let origin = point(px(ox + pw + 8.0), px(oy + y - label_px() * 0.65));
            self.paint_label(
                LabelKind::SubLast,
                key(last),
                origin,
                line_h,
                t.bg,
                window,
                cx,
                value(last),
            );
        }
        // The pointer in the sub-pane: its value on the axis, in the crosshair tag's style.
        if let Some(cy) = cross_in {
            let v = if number {
                (sc.value(cy) * 100.0).round() / 100.0
            } else {
                sc.value(cy).round()
            };
            let y = (s.sub_top + cy).clamp(
                s.sub_top + tag_h() * 0.5,
                s.sub_top + s.sub_h - tag_h() * 0.5,
            );
            window.paint_quad(fill(
                rect(pw + hair, y - tag_h() * 0.5, axis_w(), tag_h()),
                t.cross_tag,
            ));
            let origin = point(px(ox + pw + 8.0), px(oy + y - label_px() * 0.65));
            self.paint_label(
                LabelKind::SubCross,
                key(v),
                origin,
                line_h,
                t.on_cross_tag,
                window,
                cx,
                value(v),
            );
        }
    }

    /// An external series study's title at its pane's top-left, on the legend box: its name,
    /// then per line a short sample in its line type and color (the key to which line is
    /// which, never color alone) and its label and value at the crosshair (else now), in the
    /// line's color where that is AA on the box (else the theme's text); STALE or why there is
    /// nothing. Parts that don't fit are left out whole.
    #[allow(clippy::too_many_arguments)]
    fn draw_series_title(
        &mut self,
        s: crate::subpane::Split,
        def: &'static lines_feed::levels::SeriesStudyDef,
        values: &[Option<f64>; crate::series_pane::MAX_LINES],
        status: crate::series_pane::ExtStatus,
        styles: &[Option<lines_core::style::LineStyle>; crate::series_pane::MAX_LINES],
        (ox, oy): (f32, f32),
        window: &mut Window,
        cx: &mut App,
    ) {
        use crate::series_pane::ExtStatus;
        use lines_core::style::LineType;
        let t = self.theme;
        // A value's label key mixes the study, the line, its text color and the value in
        // hundredths (`None`: the name alone, a stale pane's key), so a study moved to another
        // pane or a restyled line never draws another's cached text.
        let value_key = |i: usize, c: Hsla, v: Option<f64>| -> i64 {
            let mut h = std::hash::DefaultHasher::new();
            std::hash::Hash::hash(&(def as *const _ as usize), &mut h);
            std::hash::Hash::hash(&i, &mut h);
            for f in [c.h, c.s, c.l, c.a] {
                std::hash::Hash::hash(&f.to_bits(), &mut h);
            }
            std::hash::Hash::hash(&v.map(|v| (v * 100.0).round() as i64), &mut h);
            std::hash::Hasher::finish(&h) as i64
        };
        let study = def as *const _ as usize as i64;
        let pw = f32::from(self.plot.size.width);
        let (bx, by, bh) = (8.0, s.sub_top + 4.0, label_px() * 1.6);
        let gap = 12.0;
        const SAMPLE: f32 = 14.0;
        let room = pw - bx - 16.0 - CLOSE_BOX;
        // Each line's drawn color (as styled; its theme role by default).
        let color_of = |i: usize| {
            let base = t.series[crate::series_pane::series_index(def.lines[i].color)];
            styles[i].map_or(base, |st| t.part_color(st.color, base, 100))
        };
        // (kind, key, text color, width incl. sample) of each part: a name, lines, a state.
        let mut parts: [(LabelKind, i64, Hsla, f32); 10] =
            [(LabelKind::SeriesName, 0, t.text_main, 0.0); 10];
        let mut n = 0;
        let name = move |b: &mut FixedStr<32>| {
            let _ = b.write_str(def.name);
        };
        let w = f32::from(
            self.label(LabelKind::SeriesName, study, t.text_main, window, name)
                .width,
        );
        parts[n] = (LabelKind::SeriesName, study, t.text_main, w);
        n += 1;
        let line_text = move |i: usize, v: Option<f64>| {
            move |b: &mut FixedStr<32>| {
                let _ = b.write_str(def.lines[i].label);
                if let Some(v) = v {
                    let _ = b.write_char(' ');
                    crate::series_pane::write_value(v, b);
                }
            }
        };
        let lines = def.lines.len().min(crate::series_pane::MAX_LINES);
        // The pane's lines as drawn: a title's color must read over each of them.
        let mut drawn = [t.bg; crate::series_pane::MAX_LINES];
        for (i, d) in drawn.iter_mut().enumerate().take(lines) {
            *d = color_of(i);
        }
        for (i, v) in values.iter().enumerate().take(lines) {
            // Stale with no value to show: the line's sample and name stay (the key to the
            // faded lines), without a number.
            if v.is_none() && status != ExtStatus::Stale {
                continue;
            }
            let v = *v;
            let text = t.pane_title_text(color_of(i), &drawn[..lines]);
            let key = value_key(i, text, v);
            let w = f32::from(
                self.label(
                    LabelKind::SeriesValue(i as u8),
                    key,
                    text,
                    window,
                    line_text(i, v),
                )
                .width,
            );
            parts[n] = (LabelKind::SeriesValue(i as u8), key, text, SAMPLE + 4.0 + w);
            n += 1;
        }
        let state = match status {
            ExtStatus::Live => None,
            ExtStatus::Stale => Some(("STALE", true)),
            ExtStatus::Missing(why) => Some((why, false)),
        };
        if let Some((text, stale)) = state {
            let c = if stale { t.on_warn } else { t.text_dim };
            // Each reason its own key (statics: their addresses differ).
            let key = text.as_ptr() as usize as i64;
            let w = f32::from(
                self.label(LabelKind::SeriesState, key, c, window, move |b| {
                    let _ = b.write_str(text);
                })
                .width,
            );
            parts[n] = (LabelKind::SeriesState, key, c, w);
            n += 1;
        }
        let mut used = 0.0;
        let mut shown = 0;
        for (i, p) in parts[..n].iter().enumerate() {
            let next = used + if i == 0 { p.3 } else { gap + p.3 };
            if next > room {
                break;
            }
            used = next;
            shown += 1;
        }
        if shown == 0 {
            return;
        }
        let rect = |x: f32, y: f32, w: f32, h: f32| {
            Bounds::new(point(px(ox + x), px(oy + y)), size(px(w), px(h)))
        };
        window.paint_quad(fill(rect(bx, by, used + 16.0, bh), t.label_bg));
        let line_h = px(label_px() * 1.3);
        let ty = oy + by + bh * 0.5 - label_px() * 0.65;
        let mid = by + bh * 0.5;
        let hair = 1.0 / window.scale_factor();
        let mut x = bx + 8.0;
        for (pi, &(kind, key, c, w)) in parts[..shown].iter().enumerate() {
            match kind {
                LabelKind::SeriesName => {
                    self.paint_label(
                        kind,
                        key,
                        point(px(ox + x), px(ty)),
                        line_h,
                        c,
                        window,
                        cx,
                        name,
                    );
                }
                LabelKind::SeriesValue(li) => {
                    let li = usize::from(li);
                    // The sample: the line's own type, in its color.
                    let lc = color_of(li);
                    let th = (2.0 * hair).max(1.0);
                    let lt = styles[li].map_or(def.lines[li].line, |st| st.line);
                    match lt {
                        LineType::Solid => {
                            window.paint_quad(fill(rect(x, mid - th * 0.5, SAMPLE, th), lc));
                        }
                        LineType::Dashed => {
                            for d in [0.0, 5.5, 11.0] {
                                window.paint_quad(fill(rect(x + d, mid - th * 0.5, 3.0, th), lc));
                            }
                        }
                        _ => {
                            for d in [0.0, 4.0, 8.0, 12.0] {
                                window.paint_quad(fill(rect(x + d, mid - 1.0, 2.0, 2.0), lc));
                            }
                        }
                    }
                    let at = point(px(ox + x + SAMPLE + 4.0), px(ty));
                    let text = line_text(li, values[li]);
                    self.paint_label(kind, key, at, line_h, c, window, cx, text);
                }
                _ => {
                    if state.is_some_and(|s| s.1) {
                        // STALE on the warn token: a state, said in words, on its own chip.
                        window.paint_quad(fill(rect(x - 3.0, by + 2.0, w + 6.0, bh - 4.0), t.warn));
                    }
                    let text = state.map_or("", |s| s.0);
                    self.paint_label(
                        kind,
                        key,
                        point(px(ox + x), px(ty)),
                        line_h,
                        c,
                        window,
                        cx,
                        move |b| {
                            let _ = b.write_str(text);
                        },
                    );
                }
            }
            x += w + if pi + 1 < shown { gap } else { 0.0 };
        }
    }

    /// Volume's title at its pane's top-left, on the legend box: "Volume" and the bar under
    /// the crosshair (else the newest): its total, average, and buy / sell / no-side volume
    /// when split. Each part a cached label; parts that don't fit the pane's width are left
    /// out whole (never a cut number).
    #[allow(clippy::too_many_arguments)]
    fn draw_volume_title(
        &mut self,
        s: crate::subpane::Split,
        bar: Option<lines_core::volume::VolumeBar>,
        split: bool,
        average: Option<u16>,
        (ox, oy): (f32, f32),
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        let qty = self.instrument.qty;
        // Dimmer than the total, AA on the legend box in every theme (the line itself is the
        // Average part's color).
        let avg_color = t.text_dim;
        // (part, value, color): the name and total first.
        let mut parts = [(VOL_NAME, 0i64, t.text_main); 6];
        let mut n = 1;
        if let Some(b) = bar {
            parts[0] = (VOL_TOTAL, b.total.0, t.text_main);
            if let (Some(_), Some(a)) = (average, b.avg) {
                parts[n] = (VOL_AVG, a.round() as i64, avg_color);
                n += 1;
            }
            if split {
                for (part, v) in [
                    (VOL_BUY, b.buy.0),
                    (VOL_SELL, b.sell.0),
                    (VOL_NO_SIDE, b.no_side().0),
                ] {
                    if part != VOL_NO_SIDE || v > 0 {
                        parts[n] = (part, v, t.text_main);
                        n += 1;
                    }
                }
            }
        }
        let text = |part: u8, v: i64| {
            move |b: &mut FixedStr<32>| {
                let _ = b.write_str(match part {
                    VOL_NAME => "Volume",
                    VOL_TOTAL => "Volume ",
                    VOL_AVG => "avg ",
                    VOL_BUY => "buy ",
                    VOL_SELL => "sell ",
                    _ => "no side ",
                });
                if part != VOL_NAME {
                    let _ = qty.write(lines_core::Qty(v), b);
                }
            }
        };
        let pw = f32::from(self.plot.size.width);
        let (bx, by, bh) = (8.0, s.sub_top + 4.0, label_px() * 1.6);
        let gap = 12.0;
        let room = pw - bx - 16.0 - CLOSE_BOX;
        // Widths first (shaping caches them), then the box, then the text.
        let mut widths = [0.0f32; 6];
        let mut used = 0.0;
        let mut shown = 0;
        for (i, &(part, v, c)) in parts[..n].iter().enumerate() {
            let w = f32::from(
                self.label(LabelKind::Volume(part), v, c, window, text(part, v))
                    .width,
            );
            let next = used + if i == 0 { w } else { gap + w };
            if next > room {
                break;
            }
            widths[i] = w;
            used = next;
            shown += 1;
        }
        if shown == 0 {
            return;
        }
        let rect = Bounds::new(
            point(px(ox + bx), px(oy + by)),
            size(px(used + 16.0), px(bh)),
        );
        window.paint_quad(fill(rect, t.label_bg));
        let line_h = px(label_px() * 1.3);
        let ty = oy + by + bh * 0.5 - label_px() * 0.65;
        let mut x = ox + bx + 8.0;
        for (i, &(part, v, c)) in parts[..shown].iter().enumerate() {
            self.paint_label(
                LabelKind::Volume(part),
                v,
                point(px(x), px(ty)),
                line_h,
                c,
                window,
                cx,
                text(part, v),
            );
            x += widths[i] + gap;
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

    /// Drawings' price tags (decision 0026: horizontal lines and rays, a zone's edges, with
    /// the option on). Their own shape, which no color can make look like the last price's or
    /// an order's filled tag: an outline in the drawing's color on the axis background, a
    /// notch from the plot's edge, shorter and inset; text in the drawing's color where that
    /// is AA, else the theme's text (`Theme::drawing_tag`). Under the last-price and
    /// crosshair tags, which are drawn after.
    fn draw_drawing_tags(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        if self.drawings.hidden {
            return;
        }
        let t = self.theme;
        let scale = window.scale_factor();
        let hair = 1.0 / scale;
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let (pw, ph) = (
            f32::from(self.plot.size.width),
            f32::from(self.plot.size.height),
        );
        let fmt = self.instrument.price;
        let mut tags = [(Price(0), 0.0f32, lines_core::drawings::Style::DEFAULT); MAX_DRAWING_TAGS];
        let mut n = 0;
        for (slot, tag) in tags.iter_mut().zip(self.drawings.axis_tags(ph)) {
            *slot = tag;
            n += 1;
        }
        let x0 = ox + pw + hair;
        let last_y = self.last_trade.map(|p| last_tag_y(&self.vp, p, ph));
        let mut lines = [0.0f32; MAX_DRAWING_TAGS];
        for (slot, tag) in lines.iter_mut().zip(&tags[..n]) {
            *slot = tag.1;
        }
        let mut at = [0.0f32; MAX_DRAWING_TAGS];
        drawing_tag_ys(&lines[..n], last_y, ph, &mut at[..n]);
        for (&(price, line_y, style), &y) in tags[..n].iter().zip(&at[..n]) {
            let color = t.style_color(style.line.color, &t.drawing, 100);
            let (border, text) = t.drawing_tag(color);
            // The box: kept inside the plot, off the last-price tag and off the other tags it
            // was stepped onto; the notch stays at the line's own level.
            let top = snap(f64::from(y - DRAWING_TAG_H * 0.5), scale);
            let mid = snap(f64::from(line_y), scale);
            // The notch: the line's level carried from the plot's edge to the tag (and along
            // its edge to the box when the box moved).
            window.paint_quad(fill(
                Bounds::new(
                    point(px(x0), px(oy + mid - hair)),
                    size(px(DRAWING_TAG_INSET), px(2.0 * hair)),
                ),
                border,
            ));
            let (box_top, box_bottom) = (top, top + DRAWING_TAG_H);
            if mid < box_top || mid > box_bottom {
                let (a, b) = if mid < box_top {
                    (mid - hair, box_top)
                } else {
                    (box_bottom, mid + hair)
                };
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(x0 + DRAWING_TAG_INSET - 2.0 * hair), px(oy + a)),
                        size(px(2.0 * hair), px(b - a)),
                    ),
                    border,
                ));
            }
            window.paint_quad(
                fill(
                    Bounds::new(
                        point(px(x0 + DRAWING_TAG_INSET), px(oy + top)),
                        size(px(axis_w() - DRAWING_TAG_INSET - 2.0), px(DRAWING_TAG_H)),
                    ),
                    t.bg,
                )
                .border_widths(px(1.0))
                .border_color(border),
            );
            self.paint_label(
                LabelKind::DrawingTag(crate::theme::rgb_of(text).to_u32()),
                price.0,
                point(px(ox + pw + 8.0), px(oy + y - LABEL_PX_100 * 0.65)),
                px(LABEL_PX_100 * 1.3),
                text,
                window,
                cx,
                |s| {
                    let _ = fmt.write(price, s);
                },
            );
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
        let line_h = px(label_px() * 1.3);
        let fmt = self.instrument.price;
        let y = cross_tag_y(y, ph);
        let top = snap(f64::from(y - tag_h() * 0.5), scale);
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox + pw + hair), px(oy + top)),
                size(px(axis_w()), px(tag_h())),
            ),
            t.cross_tag,
        ));
        self.paint_label(
            LabelKind::CrossPrice,
            price.0,
            point(px(ox + pw + 8.0), px(oy + y - label_px() * 0.65)),
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
            price_px: f64::from(label_px()) * 2.5,
            time_px: time + gap,
            date_time_px: date_time + gap,
        };
        self.space_measured = true;
    }

    /// The pointer at plot-local (`x`, `y`) as a drawing anchor: the bar's start and a price
    /// on the tick grid (or its O/H/L/C with the magnet).
    fn drawing_anchor(&self, x: f64, y: f64) -> Option<Anchor> {
        let d = self.store.try_read()?;
        let axis = TimeAxis::new(bars_view(&self.derived, self.tick_key(), &d));
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
        let line_h = px(label_px() * 1.3);
        let box_h = label_px() * 1.3 + NOTE_PAD;
        for i in 0..self.drawings.drawn().len() {
            let d = self.drawings.drawn()[i];
            if d.tool != Tool::Note {
                continue;
            }
            let Some(text) = self.drawings.note_text(d.id) else {
                continue;
            };
            // A note's text style (decision 0026): size, color, its box on or off. Re-shaped
            // only when the text or its style changes.
            let ts = d.style.text;
            // Text below AA on the chart gets its box anyway; below AA on the box it is drawn
            // in the theme's text color (`Theme::note_text`; the saved style is unchanged).
            let (boxed, color) =
                t.note_text(t.style_color(ts.color, &t.drawing, 100), ts.background);
            if self
                .note_lines
                .get(&d.id)
                .is_none_or(|(s, k, _)| s != text || *k != ts)
            {
                let text = text.to_owned();
                let line = self.shape_sized(&text, color, ts.size.px(), window);
                self.note_lines.insert(d.id, (text, ts, line));
            }
            let Some((_, _, line)) = self.note_lines.get(&d.id) else {
                continue;
            };
            let size_px = ts.size.px();
            let box_h = size_px * 1.3 + NOTE_PAD;
            let w = f32::from(line.width) + 2.0 * NOTE_PAD;
            let (x, y) = d.p0;
            if boxed {
                window.paint_quad(fill(
                    Bounds::new(point(px(ox + x + 2.0), px(oy + y)), size(px(w), px(box_h))),
                    t.panel,
                ));
            }
            let at = point(px(ox + x + 2.0 + NOTE_PAD), px(oy + y + NOTE_PAD * 0.5));
            let line_h = px(size_px * 1.3);
            let _ = line.paint(at, line_h, TextAlign::Left, None, window, cx);
            crate::layout_rec::line(window, line, at, line_h, "");
            self.drawings.set_note_box(i, (w + 2.0, box_h));
        }
        // Forget deleted notes' shaped text now and then (not every frame).
        if self.note_lines.len() > 64 {
            let d = &self.drawings;
            self.note_lines.retain(|id, _| d.note_text(*id).is_some());
        }
        if let Some((d, text)) = self.drawings.note_entry() {
            // The note being typed: re-shaped only when a key changed it.
            if self
                .note_entry_line
                .as_ref()
                .is_none_or(|(s, _)| s.strip_suffix('▏') != Some(text))
            {
                let shown = format!("{text}▏");
                // A note is a drawing: typed at the size it is drawn at, which doesn't zoom.
                let line = self.shape_sized(&shown, t.text_main, LABEL_PX_100, window);
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
            window.paint_quad(fill(b, t.panel));
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
        // The P&L display setting decides which amounts show; the cache is keyed on it too,
        // so changing the setting re-shapes every readout.
        let mode = cx
            .try_global::<crate::picker_view::AppSettings>()
            .map(|a| a.settings.pnl_display)
            .unwrap_or_default();
        let parts = cx
            .try_global::<crate::picker_view::AppSettings>()
            .map(|a| a.settings.measure)
            .unwrap_or_default()
            .parts(mode);
        // Every part off: no readout at all (never an empty panel).
        if !(parts.ticks || parts.points || parts.money || parts.bars || parts.time) {
            return;
        }
        if self
            .measure_lines
            .get(&key)
            .is_none_or(|(s, _)| *s != (stats, parts))
        {
            let mut s = FixedStr::<128>::new();
            let _ = write_measure(&stats, &self.instrument.price, parts, &mut s);
            let line = self.shape(s.as_str(), t.text_main, window);
            if self.measure_lines.len() > 64 {
                self.measure_lines.clear();
            }
            self.measure_lines.insert(key, ((stats, parts), line));
        }
        let Some((_, line)) = self.measure_lines.get(&key) else {
            return;
        };
        let box_h = label_px() * 1.3 + NOTE_PAD;
        let w = f32::from(line.width) + 2.0 * NOTE_PAD;
        let pw = f32::from(self.plot.size.width);
        let ph = f32::from(self.plot.size.height);
        let (x, y) = measure_label_at(m, (w, box_h), (pw, ph));
        // Opaque: no drawing stroke shows through the numbers.
        window.paint_quad(fill(
            Bounds::new(point(px(ox + x), px(oy + y)), size(px(w), px(box_h))),
            t.panel,
        ));
        let at = point(px(ox + x + NOTE_PAD), px(oy + y + NOTE_PAD * 0.5));
        let _ = line.paint(at, px(label_px() * 1.3), TextAlign::Left, None, window, cx);
        crate::layout_rec::line(window, line, at, px(label_px() * 1.3), "");
    }

    /// Takes typed text now (symbol search, a timeframe, a note, a menu's filter): the
    /// workspace leaves plain and Alt keys to it (`commands::runs_while_typing`).
    pub fn takes_text(&self) -> bool {
        self.picker.is_open()
            || self.tf_entry.is_some()
            || self.drawings.typing()
            || self.study_menu.is_some()
            || self.styling()
    }

    // ---- the drawing style editor (crate::style_editor) ------------------------------------

    /// Apply a style-editor input; "save as default" writes the tool's default to settings.
    fn style_edit(
        &mut self,
        f: impl FnOnce(&mut crate::style_editor::StyleEditor) -> crate::style_editor::Outcome,
        cx: &mut Context<Self>,
    ) {
        if let Some((r, mut e)) = self.study_style.take() {
            // A study's editor: each change restyles the study (a `StudyAction`); Esc steps
            // back to the studies menu, as it was left.
            match f(&mut e) {
                crate::style_editor::Outcome::Close => {
                    popover::closed(cx, Kind::StyleEditor);
                    self.toggle_studies_menu(cx);
                }
                crate::style_editor::Outcome::Changed => {
                    if let StudyRef::Levels(id) = &r {
                        // App-wide, by source: every chart of it (any product) takes it.
                        let edit = match self.levels.source_types(id) {
                            Some((types, label)) => {
                                crate::study_style::LevelEdit::of(&e, types, label)
                            }
                            None => crate::study_style::LevelEdit {
                                looks: crate::study_style::looks_of(&e),
                                types: &[],
                                label: lines_core::levels::LabelMode::Name,
                                label_default: lines_core::levels::LabelMode::Name,
                            },
                        };
                        // Only this source's entry changes, in the settings themselves (another
                        // chart's edit of another source stays).
                        if cx.has_global::<crate::picker_view::AppSettings>() {
                            crate::picker_view::update_settings(cx, |s| {
                                crate::study_style::set_level_style(&mut s.level_styles, id, &edit)
                            });
                            let app = crate::picker_view::settings(cx).level_styles.clone();
                            self.levels.set_styles(&app);
                        } else {
                            let levels = crate::study_style::with_level_style(
                                self.levels.styles(),
                                id,
                                &edit,
                            );
                            self.levels.set_styles(&levels);
                        }
                        crate::study_style::level_styles_changed();
                        self.seen_levels = u64::MAX;
                    } else {
                        let mut s = self.study_set();
                        s.apply(&StudyAction::SetStyle(
                            r.clone(),
                            crate::study_style::looks_of(&e),
                        ));
                        self.set_study_set(s);
                    }
                    self.study_style = Some((r, e));
                }
                _ => self.study_style = Some((r, e)),
            }
            cx.notify();
            return;
        }
        let (out, saved) = self.drawings.edit_style(f);
        if out == crate::style_editor::Outcome::Close {
            popover::closed(cx, Kind::StyleEditor);
        }
        if let Some((tool, style)) = saved {
            self.saved_styles[crate::chart_draw::tool_index(tool)] = style;
            crate::picker_view::update_settings(cx, |s| {
                crate::style_doc::save_default(&mut s.drawing_styles, tool.key(), &style);
            });
        }
        cx.notify();
    }

    /// A click on a style-editor control.
    pub fn style_click(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        self.style_edit(|e| e.click(row, col), cx);
    }

    /// A click on a style-editor tab.
    pub fn style_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        self.style_edit(
            |e| {
                e.select_tab(i);
                crate::style_editor::Outcome::Nothing
            },
            cx,
        );
    }

    /// A right click on the plot: an order's tag under it, else a drawing (which becomes the
    /// selection), is what the pane menu (opened by the workspace on the same click) is for.
    fn on_right_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.menu_object = None;
        self.menu_price = None;
        if self.region(ev.position) != Some(Region::Plot) {
            return;
        }
        let (x, y) = self.local(ev.position);
        // The order section's price, unless an object under the pointer gets its own menu.
        self.menu_price = self.price_at(x, y, window).ok().flatten();
        let mode = cx
            .try_global::<crate::picker_view::AppSettings>()
            .map(|a| a.settings.pnl_display)
            .unwrap_or_default();
        if let Some(o) = self.order_object_at(x as f32, y as f32, mode) {
            self.menu_object = Some(o);
            self.menu_price = None;
            return;
        }
        // The temporary measure, drawn over the drawings: its own menu.
        if self.drawings.measure_grip_at(x as f32, y as f32).is_some() {
            let s = crate::picker_view::settings(cx);
            self.menu_object = Some(crate::pane_menu::MenuObject::Measure {
                show: s.measure,
                pnl: s.pnl_display,
            });
            self.menu_price = None;
            return;
        }
        // Anything else of the orders' under the pointer (a group's tag, a marker, a handle),
        // or the order strip: no orders at its price.
        let on_strip = self
            .strip_box()
            .is_some_and(|b| b.contains(x as f32, y as f32));
        if on_strip || self.on_order_graphics(x as f32, y as f32) {
            self.menu_price = None;
        }
        let before = self.drawings.selected;
        self.drawings.right_click(x as f32, y as f32);
        if let Some(tool) = self.drawings.selected_tool() {
            self.menu_price = None;
            self.menu_object = Some(crate::pane_menu::MenuObject::Drawing {
                title: crate::pane_menu::capitalized(tool.label()),
            });
        }
        if self.drawings.selected != before {
            cx.notify();
        }
    }

    /// Whether any order graphic (a tag, a group's tag, a marker, a handle) is at plot-local
    /// (`x`, `y`), last frame's layout.
    fn on_order_graphics(&self, x: f32, y: f32) -> bool {
        let n = self.ct.n_tags;
        lines_core::geometry::order_tags::hit(
            &self.ct.tags[..n],
            &self.ct.markers,
            &self.ct.handles,
            x,
            y,
        )
        .is_some()
    }

    /// The single order whose tag (or its ×) is at plot-local (`x`, `y`), last frame's layout:
    /// its id, its tag's text, whether it's a stop. A group's tag has no one order.
    fn order_object_at(
        &self,
        x: f32,
        y: f32,
        mode: PnlDisplay,
    ) -> Option<crate::pane_menu::MenuObject> {
        use lines_core::geometry::order_tags::{TagHit, hit};
        let n = self.ct.n_tags;
        let id = match hit(&self.ct.tags[..n], &self.ct.markers, &self.ct.handles, x, y)? {
            TagHit::Body(id) | TagHit::Close(id) if self.ct.group_of(id).is_none() => id,
            _ => return None,
        };
        self.order_object(id, mode)
    }

    /// A working order shown on the chart, as its tag's menu would name it: a stop if there
    /// is one, it has the most rows (screenshots: `--context-menu order`).
    pub fn first_order_object(&self, mode: PnlDisplay) -> Option<crate::pane_menu::MenuObject> {
        let objects = self
            .ct
            .marks
            .iter()
            .filter(|m| self.ct.group_of(m.id.0).is_none())
            .filter_map(|m| self.order_object(m.id.0, mode));
        let mut first = None;
        for o in objects {
            if matches!(o, crate::pane_menu::MenuObject::Order { stop: true, .. }) {
                return Some(o);
            }
            first.get_or_insert(o);
        }
        first
    }

    /// An order's menu header: its tag's text and its P&L at its price (as the tag shows them).
    fn order_object(&self, id: u64, mode: PnlDisplay) -> Option<crate::pane_menu::MenuObject> {
        use crate::chart_trade::{money_per_unit, write_exit_pnl, write_order_tag};
        let m = self.ct.mark(id).copied()?;
        let fmt = self.instrument.price;
        let mut title = FixedStr::<96>::new();
        let _ = write_order_tag(&mut title, &m, &fmt);
        let mut pnl = FixedStr::<48>::new();
        let mpu = money_per_unit(&self.instrument);
        if write_exit_pnl(&mut pnl, &m, self.ct.position, fmt.tick, mpu, mode).is_ok()
            && !pnl.as_str().is_empty()
        {
            let _ = write!(title, " · {}", pnl.as_str());
        }
        Some(crate::pane_menu::MenuObject::Order {
            id,
            title: title.as_str().to_owned(),
            stop: crate::pane_menu::offers_breakeven(
                m.stop,
                m.side == lines_trading::Side::Sell,
                self.ct.position.map(|(qty, _)| qty),
            ),
        })
    }

    /// The last right click's price, taken by the menu it opened (once): a later menu (the
    /// menu key, the header's ⋯) never offers orders at an old price.
    pub fn take_menu_price(&mut self) -> Option<Price> {
        let p = self.menu_price.take();
        // The account its rows are offered on (`OrderCmd::Place` checks it again).
        self.menu_account = p.and(self.trade.account.clone());
        p
    }

    /// The menu's order section for a right click at `at` (`trading_on`: a linked DOM on this
    /// chart's account and contract has trading enabled; `chart_trade::order_section`).
    pub fn order_menu(
        &self,
        at: Option<Price>,
        trading_on: bool,
        cx: &App,
    ) -> Option<crate::pane_menu::ChartOrders> {
        use crate::trade_bar::TradePane;
        let option =
            crate::trade_bar::contract_of(&self.store).is_some_and(|k| k.option_key().is_some());
        // In no link group: no DOM's switch can send (the strip says the same).
        if at.is_some() && !option && !self.trade_unavailable() && self.common.link.is_none() {
            return Some(crate::pane_menu::ChartOrders::Note(
                crate::chart_trade::LINK_TO_TRADE.to_owned(),
            ));
        }
        crate::chart_trade::order_section(
            at,
            option,
            self.trade_unavailable(),
            trading_on && self.common.link.is_some(),
            crate::trade_bar::entry_ctx(self, cx),
            &self.instrument.price,
            self.instrument.qty,
        )
    }

    /// Screenshots (`--context-menu pane@X,Y`): the order section's price as a right click at
    /// window point `pos` gives it (no object under it). `false`: not yet (the chart hasn't
    /// laid out its plot, or has no price there yet): the screenshot asks again next frame.
    pub fn set_menu_point(&mut self, pos: Point<Pixels>, window: &Window) -> bool {
        if self.chart.size.width <= px(0.0) {
            return false;
        }
        self.menu_price = if self.region(pos) == Some(Region::Plot) {
            let (x, y) = self.local(pos);
            match self.price_at(x, y, window) {
                Ok(Some(p)) => Some(p),
                _ => return false,
            }
        } else {
            None
        };
        true
    }

    /// This chart's link group.
    pub fn link_group(&self) -> crate::layout::LinkGroup {
        self.common.link
    }

    /// The object the last right click was on, for its menu (once).
    pub fn take_menu_object(&mut self) -> Option<crate::pane_menu::MenuObject> {
        self.menu_object.take()
    }

    /// An order tag menu's row (`crate::pane_menu::object_items`): the tag's own paths.
    pub fn order_command(&mut self, c: crate::commands::OrderCmd, cx: &mut Context<Self>) {
        use crate::commands::OrderCmd;
        match c {
            OrderCmd::Cancel(id) => self.cancel_order(id, cx),
            OrderCmd::Place(o, offer) => {
                // The account it was offered on: a switch since refuses it, as a contract or a
                // size does.
                let offered = self.menu_account.take();
                if !crate::chart_trade::same_account(offered.as_ref(), self.trade.account.as_ref())
                {
                    self.trade.flash(crate::chart_trade::CHART_CHANGED);
                } else {
                    self.send_entry(|ctx| crate::chart_trade::menu_command(o, offer, ctx), cx)
                }
            }
            OrderCmd::Resize(id) => {
                crate::trade_bar::open_resize_field(self, lines_trading::OrderId(id));
            }
            // From the chart, by the pointer: no switch needed, as on the DOM.
            OrderCmd::Breakeven => crate::trade_bar::run_from(
                self,
                crate::commands::TradeCmd::Breakeven,
                std::time::Instant::now(),
                lines_trading::Source::Chart,
                cx,
            ),
        }
        cx.notify();
    }

    /// Whether a drawing shown on this chart is selected (the pane menu's "Drawing style…").
    pub fn drawing_selected(&self) -> bool {
        self.drawings.selection_shown()
    }

    /// Open the style editor on drawing `id`: the chart's other popovers close, and any other
    /// in the app (`popover::opened`).
    fn open_style_editor(&mut self, id: u64, cx: &mut Context<Self>) -> bool {
        if !self.drawings.open_editor(id) {
            return false;
        }
        // One popover: a study's style editor gives way to the drawing's.
        self.study_style = None;
        self.picker.close();
        self.anchoring = None;
        self.editor_scroll = Default::default();
        self.close_popover(Kind::Studies, cx);
        self.close_popover(Kind::Timeframe, cx);
        self.close_popover(Kind::Account, cx);
        popover::opened(cx, Kind::StyleEditor, |v: &mut Self, cx| {
            v.close_popover(Kind::StyleEditor, cx)
        });
        cx.notify();
        true
    }

    /// The open style editor over the window, with a backdrop under it (`crate::popover`):
    /// placed beside its drawing (a study's under the study legend), inside the pane; its
    /// warnings checked in every theme and palette.
    fn style_editor_layer(&mut self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        // A study's editor sits at the plot's left under the study legend, away from the
        // newest bars; a drawing's beside its drawing. Warnings name theme and palette.
        let (e, target, warnings) = match &self.study_style {
            Some((r, e)) => {
                let themes = Theme::all();
                let chart = crate::study_style::ChartCtx::of(&self.study_set(), r, &themes);
                let warnings = crate::study_style::warnings(r, e, &themes, &chart);
                (e.clone(), None, warnings)
            }
            None => {
                let e = self.drawings.editor()?.clone();
                let id = self.drawings.editor_target()?;
                let ctxs: Vec<_> = Theme::all().iter().map(Theme::style_ctx).collect();
                let warnings = e.warnings(&ctxs);
                (e, Some(self.drawings.extent(id)), warnings)
            }
        };
        // Inside the plot area: never over the time axis, the status bar or a pane below (until
        // it is dragged: then where it was dragged, `popover::floating`).
        let area = (f32::from(self.chart.size.width), self.above_axis);
        self.editor_pane = area;
        let w = crate::style_editor_view::width(area.0);
        let names = crate::style_editor_view::names(&e, t);
        let h = crate::style_editor_view::content_height(&e, &warnings, w, names);
        let at = match target {
            Some(extent) => crate::style_editor_view::place(area, extent, h),
            None => {
                // The volume profile's editor stays clear of the profile (anchored at the
                // plot's left), its POC included.
                let pc = self.profile.config();
                let left_clear = match &self.study_style {
                    Some((StudyRef::Profile, _))
                        if pc.mode != crate::profile::ProfileMode::Off && !pc.hidden =>
                    {
                        (area.0 - axis_w()).max(0.0) * pc.width()
                    }
                    _ => 0.0,
                };
                crate::style_editor_view::place_study(area, self.legend_band.get().1, left_clear, h)
            }
        };
        // A key that moved the focus scrolls it into view. Until the layout it needs exists
        // (the body's first, the focused row's controls) the follow stays pending (a few
        // frames at most); each follow renders the pane once more (a cached view), so the
        // scrollbar, drawn from the offset at render, shows where it scrolled.
        if self.editor_scroll.follow(&e) {
            cx.notify();
        }
        let title = e.title.clone();
        let editor =
            crate::style_editor_view::render(&e, t, &warnings, at, h, &self.editor_scroll, cx);
        let dismiss: popover::OnDismiss = Box::new(
            cx.listener(|v, _: &MouseDownEvent, _, cx| v.close_popover(Kind::StyleEditor, cx)),
        );
        Some(
            popover::floating(popover::Panel::StyleEditor, title, t, editor, dismiss)
                .into_any_element(),
        )
    }

    /// The app-wide level styles (settings `level_styles`, decision 0026): promote what this
    /// chart saved per source before they were app-wide (once, where a source has none), and
    /// take the current ones when they moved (another chart's style editor, a promotion).
    fn sync_level_styles(&mut self, cx: &mut Context<Self>) {
        self.promote_levels(cx);
        if !cx.has_global::<crate::picker_view::AppSettings>() {
            return;
        }
        let generation = crate::study_style::level_styles_generation();
        if generation != self.level_styles_gen {
            self.level_styles_gen = generation;
            let app = crate::picker_view::settings(cx).level_styles.clone();
            self.levels.set_styles(&app);
            self.seen_levels = u64::MAX;
        }
    }

    /// Promote this chart's own per-source level styles (from before they were app-wide) to
    /// the app-wide ones: on restore (so a chart in a background tab, never shown, loses
    /// nothing), when a template or the defaults apply, else at the next render. Writes the
    /// settings only when a source has no app-wide entry yet. Without the settings (tests),
    /// the chart draws them itself and keeps saving them.
    pub fn promote_levels(&mut self, cx: &mut App) {
        if self.pending_levels.is_empty() {
            return;
        }
        if !cx.has_global::<crate::picker_view::AppSettings>() {
            let mut levels = self.levels.styles().clone();
            if self.pending_levels.clone().promote(&mut levels) {
                self.levels.set_styles(&levels);
            }
            return;
        }
        // Settings that are never saved (screenshots, a file that couldn't be backed up):
        // promoted for this run, and still saved with the chart (`promote_into`). Settings are
        // written only when a source has no app-wide entry yet.
        let writable = cx.global::<crate::picker_view::AppSettings>().writable();
        let mut pending = std::mem::take(&mut self.pending_levels);
        if pending.adds_to(&crate::picker_view::settings(cx).level_styles) {
            crate::picker_view::update_settings(cx, |s| {
                pending.promote_into(&mut s.level_styles, writable);
            });
            crate::study_style::level_styles_changed();
        } else if writable {
            // Every source already has an app-wide style: the chart's own are dropped.
            pending = Default::default();
        }
        self.pending_levels = pending;
    }

    /// Pick up per-tool defaults saved since this chart last looked (on any chart).
    fn refresh_tool_styles(&mut self, cx: &App) {
        // Only the tools whose saved default changed: this chart's session restyles of the
        // other tools stay.
        let saved = saved_tool_styles(cx);
        for (i, s) in saved.iter().enumerate() {
            if *s != self.saved_styles[i] {
                self.drawings.defaults[i] = *s;
            }
        }
        self.saved_styles = saved;
    }

    /// A drawing command (`Command::Draw`) on this chart.
    pub fn drawing_command(&mut self, cmd: DrawCmd, cx: &mut Context<Self>) {
        let d = &mut self.drawings;
        match cmd {
            DrawCmd::Tool(tool) => {
                self.picker.close();
                self.tf_menu = false;
                self.tf_entry = None;
                self.close_popover(Kind::StyleEditor, cx);
                self.refresh_tool_styles(cx);
                self.drawings.set_tool(tool);
            }
            DrawCmd::Style => match d.selected.filter(|_| d.selection_shown()) {
                Some(id) => {
                    self.open_style_editor(id, cx);
                }
                None => {
                    let why = d.style_unavailable();
                    if cx.has_global::<crate::workspace::Workspace>() {
                        crate::workspace::notice(why.to_owned(), cx);
                    }
                }
            },
            DrawCmd::MeasurePart(p) => {
                crate::picker_view::update_settings(cx, |s| s.measure.toggle(p));
            }
            other => d.run(other),
        }
        cx.notify();
    }

    /// Whether a temporary measure is shown (screenshots' `--context-menu measure`).
    #[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
    pub fn measure_shown(&self) -> bool {
        self.drawings.measure_shown().is_some()
    }

    pub fn drawings_hidden(&self) -> bool {
        self.drawings.hidden
    }

    pub fn product_symbol(&self) -> &'static str {
        self.instrument.symbol
    }

    /// Rebuild the strategy markers when the instances, the fills or the contract moved (the
    /// poll, never paint). True when they changed.
    fn poll_marks(&mut self) -> bool {
        let Some(source) = self.strategies.clone() else {
            return false;
        };
        let Some(trading) = self.trading_cache.clone() else {
            return false;
        };
        let Some(contract) = self.store.try_read().and_then(|d| d.contract) else {
            return false;
        };
        let Some(state) = trading.try_read() else {
            return false; // busy this instant: the next poll
        };
        // The display zone's epoch too: a marker's text names its time in that zone.
        let key = (
            source.revision().wrapping_add(display_tz::epoch() << 48),
            state.fills_seen,
            contract,
        );
        let product = &self.instrument;
        self.marks.update(key, || {
            crate::strategy_marks::build(
                &source.snapshot(),
                state.fills(),
                contract,
                product,
                |o| state.order(o).map(|o| (o.role, o.parent)),
            )
        })
    }

    /// The hovered strategy marker's text, beside it.
    fn strategy_mark_tip(&self, t: &Theme) -> Option<AnyElement> {
        // Never over what the trader is aiming at or typing in: any drag, a drawing in
        // progress, a price or bar being picked, the picker or an editor.
        if self.drag.is_some()
            || self.ct.drag.is_some()
            || self.ct.adding.is_some()
            || self.ct.pos_drag
            || self.ct.strip_pick.is_some()
            || self.anchoring.is_some()
            || self.drawings.active()
            || self.takes_text()
        {
            return None;
        }
        let (x, y) = self.hover?;
        let m = self.marks.hit(x, y)?;
        let (left, top, w) = crate::strategy_marks::tip_place(
            x,
            y,
            f32::from(self.plot.size.width),
            f32::from(self.plot.size.height),
        );
        Some(
            div()
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(w))
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(t.panel)
                .border_1()
                .border_color(t.axis_line)
                .text_color(t.text_main)
                .text_size(crate::zoom::ui(12.0))
                .child(m.head.clone())
                .child(m.text.clone())
                .into_any_element(),
        )
    }

    /// Show or hide the strategy markers on this chart (saved with it).
    pub fn toggle_strategy_marks(&mut self, cx: &mut Context<Self>) {
        self.marks.set_hidden(!self.marks.hidden);
        cx.notify();
    }

    pub fn strategy_marks_hidden(&self) -> bool {
        self.marks.hidden
    }

    /// The banner for drawing mode, a temporary measure or the delete-all confirmation.
    fn drawing_banner(&self, t: &Theme) -> Option<AnyElement> {
        let d = &self.drawings;
        let text = if d.confirm_clear {
            clear_drawings_question(d.count(), self.instrument.symbol)
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
                        .text_size(crate::zoom::ui(12.0))
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
        let zone = self.zone.clone();
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
                // Trade-built bars start at a trade (or a seconds slot): to the second.
                Timeframe::Ticks(_)
                | Timeframe::Volume(_)
                | Timeframe::Range(_)
                | Timeframe::Seconds(_) => {
                    let _ = time.write_crosshair_label(1_000, &zone, s);
                }
                _ => {
                    let _ = time.write_crosshair_label(period, &zone, s);
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
        let line_h = px(label_px() * 1.3);
        let text = self.time_tag_text(time);
        let (left, box_w) = self.time_tag_box(x, time, window);
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox + left), px(oy + self.above_axis + hair)),
                size(px(box_w), px(axis_h() - hair)),
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
        tick: TickInfo,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        if self
            .readout
            .as_ref()
            .is_none_or(|(b, p, k, _)| *b != bar || *p != partial || *k != tick)
        {
            let mut s = FixedStr::<160>::new();
            let _ = write_ohlcv(&bar, &self.instrument.price, self.instrument.qty, &mut s);
            if let Some((last, count, n, state)) = tick {
                // A tick bar: when it ran (first to last trade) and how many trades it holds
                // ("partial (41 of 233 trades)" when cut short or still forming).
                let _ = s.write_str("  ");
                let _ = write_tick_span(bar.time, last, count, n, state, &self.zone, &mut s);
            } else if partial {
                // Cut short by a session break (the close, or a holiday halt; tick bars: the
                // session's last bar).
                let _ = s.write_str("  partial");
            }
            let line = self.shape(s.as_str(), t.text_main, window);
            self.readout = Some((bar, partial, tick, line));
        }
        let Some((_, _, _, line)) = &self.readout else {
            return;
        };
        let scale = window.scale_factor();
        let (ox, oy) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let box_h = label_px() * 1.6;
        let (left, top) = (snap(6.0, scale), snap(6.0, scale));
        let box_w = snap(f64::from(f32::from(line.width) + 12.0), scale);
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox + left), px(oy + top)),
                size(px(box_w), px(box_h)),
            ),
            t.label_bg,
        ));
        let origin = point(
            px(ox + left + 6.0),
            px(oy + top + box_h * 0.5 - label_px() * 0.65),
        );
        let _ = line.paint(
            origin,
            px(label_px() * 1.3),
            TextAlign::Left,
            None,
            window,
            cx,
        );
        crate::layout_rec::line(window, line, origin, px(label_px() * 1.3), "");
    }

    /// "tick bars from 11-05 09:32 ET" beside the first tick bar, at the plot's
    /// bottom: the bars begin after their session's open, so earlier trading has none.
    fn draw_tick_from(
        &mut self,
        from: UtcMillis,
        x: f32,
        (ox, oy): (f32, f32),
        ph: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        let zone = self.zone.clone();
        let text = |s: &mut FixedStr<32>| {
            let _ = lines_feed::ticks::write_from(from, &zone, s);
        };
        let w = f32::from(
            self.label(LabelKind::TickFrom, from.0, t.text_dim, window, text)
                .width,
        );
        let scale = window.scale_factor();
        let pw = f32::from(self.plot.size.width);
        let box_h = label_px() * 1.6;
        let box_w = w + 12.0;
        let left = snap(f64::from((x + 6.0).min(pw - box_w).max(0.0)), scale);
        let top = snap(f64::from(ph - box_h - 6.0), scale);
        window.paint_quad(fill(
            Bounds::new(
                point(px(ox + left), px(oy + top)),
                size(px(box_w), px(box_h)),
            ),
            t.label_bg,
        ));
        self.paint_label(
            LabelKind::TickFrom,
            from.0,
            point(
                px(ox + left + 6.0),
                px(oy + top + box_h * 0.5 - label_px() * 0.65),
            ),
            px(label_px() * 1.3),
            t.text_dim,
            window,
            cx,
            text,
        );
    }

    fn shape(&self, text: &str, color: Hsla, window: &mut Window) -> ShapedLine {
        self.shape_sized(text, color, label_px(), window)
    }

    fn shape_sized(
        &self,
        text: &str,
        color: Hsla,
        size_px: f32,
        window: &mut Window,
    ) -> ShapedLine {
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
            px(size_px),
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
        if needs_shaping(&self.labels, (kind, key), color) {
            let mut s = FixedStr::<32>::new();
            text(&mut s);
            // A drawing's tag sits in a box that doesn't zoom (`DRAWING_TAG_H`).
            let size = if matches!(kind, LabelKind::DrawingTag(_)) {
                LABEL_PX_100
            } else {
                label_px()
            };
            let line = self.shape_sized(s.as_str(), color, size, window);
            self.labels.insert((kind, key), (color, line));
        }
        &self.labels[&(kind, key)].1
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
        let owner = if kind == LabelKind::LastPrice {
            "last-price-tag"
        } else {
            ""
        };
        crate::layout_rec::line(window, line, origin, line_h, owner);
    }

    /// The chart's header. `account`: its trading account chip (with the PAPER badge), shown
    /// with the contract and the timeframe chip on the left; the right end is the hover
    /// controls' gutter.
    fn header(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
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
            // Feed busy this instant; refreshes next frame. Its height stays, or the plot would
            // move for a frame, and the option chart's rows with it.
            return div().h(crate::zoom::ui(crate::pane::header_height(
                crate::layout::PaneKind::Chart,
            )));
        };
        let i = &d.product;
        let f = i.price;
        // The active contract is always shown (decision 0013); unknown is labelled, not hidden.
        let _ = match d.contract_label() {
            Some(c) => write!(left, "{c}"),
            None => write!(left, "{} (contract unknown)", i.symbol),
        };
        // In a run with several sources, the one serving this pane (decision 0030).
        crate::sources::write_header_tag(&mut left, &self.feed, cx);
        // Last, bid, ask: a missing value shows as a dimmed dash (docs/design.md §7), never as
        // a bare label that reads like a layout bug.
        let quote = |label: &str, p: Option<Price>| {
            let (text, missing) = quote_text(label, p, &f);
            div()
                .text_color(if missing { t.text_dim } else { t.text_main })
                .child(text)
        };
        // Each item is dropped whole when it doesn't fit (it wraps onto a hidden second line,
        // never cut); bid and ask are one item, so the quote never shows one side alone.
        let item = |d: gpui::Div, label: String| {
            d.ml_2()
                .flex_shrink_0()
                .relative()
                .children(crate::layout_rec::text_marker(&label))
        };
        let mut quotes = vec![item(
            div().child(quote("last", d.last_trade)),
            "last".to_owned(),
        )];
        if d.capabilities.bbo {
            quotes.push(item(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(quote("bid", d.bid))
                    .child(quote("ask", d.ask)),
                "bid / ask".to_owned(),
            ));
        }
        if let Some(n) = pane::adopted_note(&self.feed) {
            let _ = write!(right, "{n}  ·  ");
        }
        // Every state but live has a short word beside the contract that never gives way;
        // the status text (which does) then names only the provider.
        let word = pane::status_word(&d.status);
        let _ = write!(right, "{}", d.provider);
        if word.is_none() {
            let _ = write!(right, "  ·  {}", d.status.label());
        }
        // History back-fill is shown as state, not a spinner (docs/design.md).
        if d.orderflow_loading {
            // Stored trades are replaying into the profile, footprint and delta (bars first).
            right.push_str("  ·  loading order flow…");
        }
        if let Some(h) = d.history.label() {
            let _ = write!(right, "  ·  {h}");
        }
        // Tick bars being rebuilt say so here; bars that begin late say so at their first bar
        // in the plot (here too while there are none yet).
        let tick_status = self
            .tick_key()
            .map(|k| d.ticks.status(k, &d.series, &d.product));
        let tick_bars = self.tick_key().and_then(|k| d.ticks.bars(k));
        if let Some(st) = tick_status
            && (st == lines_feed::ticks::TickStatus::Building
                || tick_bars.is_none_or(|b| b.is_empty()))
            && let Some(l) = st.label(&display_tz::current())
        {
            let _ = write!(right, "  ·  {l}");
        }
        // The forming tick bar's trades to go, beside the timeframe in the chip (none once its
        // session has closed: it can't take another trade).
        let now = self.store.feed_clock();
        let to_go = tick_bars.and_then(|b| b.to_go(now));
        if self.frozen {
            // Frozen clock: only data-derived time is shown, so screenshots are reproducible.
            // On the display zone's clock, like every other time, and short ("16:58:59 ET": the
            // axis has the date) so a narrow header keeps its zone.
            if let Some(mt) = d.market_time {
                let zone = display_tz::current();
                let _ = right.write_str("  ·  ");
                let _ = zone.write_clock(mt, &mut right);
                let _ = right.write_char(' ');
                let _ = zone.write_label(mt, &mut right);
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
        if self.latency_hud {
            // The latency HUD (roadmap 6b), with the performance stats, after the feed's state
            // and failures (a narrow header truncates the HUD first, never those).
            let _ = write!(right, "  ·  {}", self.hud_text);
            if let Some((c, false)) = &self.hud_clock {
                let _ = write!(right, "  ·  {c}");
            }
        }
        let list = self.timeframe_list(&t, to_go, cx);
        // What fits whole beside the hover controls' gutter (from last frame's width); the
        // rest is dropped whole, never cut: typing a timeframe and the palette still change
        // the timeframe, and the palette the account.
        let room = f32::from(self.chart.size.width)
            - pane::controls_gutter(crate::layout::PaneKind::Chart);
        let acct = crate::trade_bar::chart_account(self, cx);
        // Header text widths as the UI font shapes them (text_sm).
        let font = window.text_style().font();
        let size = gpui::rems(0.875).to_pixels(window.rem_size());
        let measure = |s: &str| -> f32 {
            let run = TextRun {
                len: s.len(),
                font: font.clone(),
                color: t.text_main,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window.text_system().shape_line(
                SharedString::from(s.to_owned()),
                size,
                &[run],
                None,
            );
            f32::from(line.width)
        };
        let fit = pane::chart_header_fit(
            room,
            self.common.link.is_some(),
            &left,
            word,
            &pane::chip_text(self.timeframe, self.tick_continuous(), to_go),
            acct.as_ref().map(|(n, p)| (n.as_str(), *p)),
            self.tf_menu,
            &measure,
        );
        let desc_w = measure(i.description).ceil() + 1.0;
        let account = crate::trade_bar::chart_chip(self, &t, fit.account, cx);
        let timeframe = fit.timeframe.then(|| {
            let dismiss: popover::OnDismiss = Box::new(
                cx.listener(|v, _: &MouseDownEvent, _, cx| v.close_popover(Kind::Timeframe, cx)),
            );
            crate::dropdown::dropdown(self.timeframe_chip(&t, to_go, cx), list).on_dismiss(dismiss)
        });
        bar().child(
            pane::header_content()
                .child(
                    // Order of importance, left to right: link, contract, account (PAPER/LIVE
                    // first), status word, timeframe (`pane::chart_header_fit`); the status
                    // text gives way first, then the quotes and the description.
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .items_center()
                        .min_w_0()
                        .children(if fit.link { chip } else { None })
                        // The contract never gives way; its description does, with "…".
                        .child(
                            div()
                                .text_color(t.text_main)
                                .flex_shrink_0()
                                .relative()
                                .children(crate::layout_rec::text_marker(&left))
                                .child(left),
                        )
                        .children(account)
                        // Whole unless nothing else is left to give way: then "…", never cut.
                        .children(word.map(|w| {
                            let d = div().text_color(t.text_main);
                            if fit.status {
                                d.flex_shrink_0()
                            } else {
                                d.min_w_0().truncate()
                            }
                            .relative()
                            .children(crate::layout_rec::text_marker(w))
                            .child(w)
                        }))
                        .children(timeframe)
                        // The description truncates with "…" down to 48 px, then drops whole
                        // (wraps onto a hidden second line) rather than show a bare "…".
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .min_w_0()
                                // Its whole text's width, shrinking from there (a wrapping
                                // row's own width otherwise comes from the 48 px basis).
                                .flex_basis(px(desc_w))
                                .h(px(quote_row_h()))
                                .line_height(px(quote_row_h()))
                                .items_center()
                                .overflow_hidden()
                                .child(div().w(px(0.0)).h(px(quote_row_h())))
                                .child(
                                    div()
                                        .flex_basis(px(48.0))
                                        .flex_grow(1.0)
                                        .min_w_0()
                                        .text_color(t.text_main)
                                        .truncate()
                                        .relative()
                                        .children(crate::layout_rec::text_marker(i.description))
                                        .child(i.description),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .min_w_0()
                                .h(px(quote_row_h()))
                                .line_height(px(quote_row_h()))
                                .items_center()
                                .overflow_hidden()
                                // A zero-width first item: the first quote can wrap away too
                                // (a line's first item never wraps).
                                .child(div().w(px(0.0)).h(px(quote_row_h())))
                                .children(quotes),
                        )
                        .children(pane::stale_badge(&self.store, &t))
                        .children(self.clock_warning(&t)),
                )
                // Status text takes only what the left group leaves (it gives way first), at
                // the right end, cut with "…"; it never runs over the chips.
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(t.text_dim)
                                .relative()
                                .children(crate::layout_rec::text_marker(&right))
                                .child(right),
                        ),
                ),
        )
    }
}

// ---- timeframe controls (roadmap 9) ---------------------------------------------------

impl ChartView {
    /// The latency HUD's clock warning (beyond 250 ms), as a badge beside the quotes: never
    /// truncated away with the right-hand text. The ⚠ and its words carry it, not color.
    fn clock_warning(&self, t: &Theme) -> Option<gpui::AnyElement> {
        let (text, true) = self.hud_clock.as_ref()? else {
            return None;
        };
        Some(
            div()
                .px_1()
                .border_1()
                .border_color(t.warn)
                .text_color(t.text_main)
                .child(text.clone())
                .into_any_element(),
        )
    }

    /// The header's timeframe chip ("5m ▾"; a forming tick bar's trades to go in quiet text:
    /// "233t · 41 left ▾"; "∞" on continuous tick bars): opens the list.
    fn timeframe_chip(
        &self,
        t: &Theme,
        to_go: Option<u32>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let accent = t.accent;
        div()
            .id("timeframe")
            .flex()
            .flex_row()
            .px_1()
            .rounded_sm()
            .border_1()
            .border_color(if self.tf_menu { accent } else { t.axis_line })
            .text_color(t.text_main)
            .cursor_pointer()
            .hover(move |s| s.border_color(accent))
            .relative()
            .child(pane::chip_timeframe(self.timeframe, self.tick_continuous()))
            .children(to_go.map(|n| div().text_color(t.text_dim).child(pane::chip_to_go(n))))
            .child(" ▾")
            .children(
                crate::layout_rec::on()
                    .then(|| {
                        crate::layout_rec::bordered(
                            lines_core::layout_lint::Kind::Chip,
                            pane::chip_text(self.timeframe, self.tick_continuous(), to_go),
                            0,
                            "chart-header",
                            1.0,
                        )
                    })
                    .flatten(),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, _, _, cx| {
                    cx.stop_propagation();
                    v.toggle_timeframe_menu(cx);
                }),
            )
    }

    /// The timeframe list, while open: under the chip (`dropdown`).
    fn timeframe_list(
        &self,
        t: &Theme,
        to_go: Option<u32>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.tf_menu {
            return None;
        }
        let (accent, lit) = (t.accent, t.ghost_selected);
        let mut list = div()
            .id("timeframe-list")
            .block_mouse_except_scroll()
            .min_w(px(150.0))
            .flex()
            .flex_col()
            .py_1()
            .bg(t.panel)
            .border_1()
            .border_color(accent)
            .rounded_sm()
            .text_size(crate::zoom::ui(12.0))
            .children(
                crate::layout_rec::on()
                    .then(|| {
                        crate::layout_rec::bordered(
                            lines_core::layout_lint::Kind::Popup {
                                trigger: pane::chip_text(
                                    self.timeframe,
                                    self.tick_continuous(),
                                    to_go,
                                ),
                            },
                            "timeframe list",
                            crate::layout_rec::POPUP,
                            "",
                            1.0,
                        )
                    })
                    .flatten(),
            );
        // Two columns, time bars and tick bars: the list stays as short as the time column.
        let mut columns = [
            div().flex().flex_col().min_w(px(110.0)),
            div().flex().flex_col().min_w(px(110.0)),
        ];
        for (i, tf) in Timeframe::ALL.into_iter().enumerate() {
            let on = tf == self.timeframe;
            // Time-based left; trade-built (tick, seconds, volume, range bars) right.
            let col = usize::from(tf.bar_kind().is_some());
            let name = pane::timeframe_name(tf);
            columns[col] = std::mem::replace(&mut columns[col], div()).child(
                div()
                    .id(("timeframe", i as u64))
                    .px_2()
                    .py(px(2.0))
                    .cursor_pointer()
                    .text_color(t.text_main)
                    .hover(move |s| s.bg(lit))
                    .font_weight(if on {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .child(if on {
                        format!("✓ {name}")
                    } else {
                        format!("   {name}")
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
        let [time, ticks] = columns;
        list = list.child(div().flex().flex_row().child(time).child(ticks));
        Some(
            list.child(
                div()
                    .px_2()
                    .pt_1()
                    .text_color(t.text_dim)
                    .text_size(crate::zoom::ui(11.0))
                    .child("Or type 5, 1h, 500t, 500v, 10r, 30s + Enter"),
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
                "1 2 3 5 10 15 30 (m)  ·  1h 2h 4h  ·  1d  ·  1w  ·  100t 233t 500t (1 to 100000)"
                    .to_owned(),
                false,
            ),
        };
        Some(
            div()
                .absolute()
                .bottom(px(axis_h() + 8.0))
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
                .text_size(crate::zoom::ui(12.0))
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

/// A chart asks its window to send an order it built (an exit from a position handle or line, or
/// an order strip button); the window sends it only when a linked DOM (same account and
/// contract) has trading enabled, else shows `refused` on the chart.
pub struct ChartOrder {
    pub command: lines_trading::Command,
    pub refused: &'static str,
    /// A working order's resize: its order, price and new total, so the chart tracks the answer.
    pub resize: Option<(lines_trading::OrderId, Price, lines_core::Qty)>,
    /// The chart's link group: the DOMs whose switch may send it.
    pub link: crate::layout::LinkGroup,
    pub account: lines_trading::AccountId,
    pub contract: lines_core::ContractKey,
    pub issued: std::time::Instant,
}

impl EventEmitter<ChartOrder> for ChartView {}

impl ChartView {
    /// The window refused a chart order (no linked DOM with trading enabled): say so on the
    /// chart.
    pub fn exit_refused(&mut self, why: &'static str, cx: &mut Context<Self>) {
        self.trade.flash(why);
        cx.notify();
    }

    /// A resize was sent: its refusal shows as a rejected drag does; only the newest counts.
    pub fn track_resize(
        &mut self,
        order: lines_trading::OrderId,
        price: Price,
        qty: lines_core::Qty,
        command: lines_trading::CommandId,
    ) {
        self.ct
            .ghosts
            .retain(|g| g.order != order || g.qty.is_none());
        self.ct.ghosts.push(crate::dom_trade::Ghost {
            order,
            price,
            command,
            qty: Some(qty),
        });
    }
}

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
            order_strip: self.order_strip,
            hide_drawings: self.drawings.hidden,
            hide_strategy_marks: self.marks.hidden,
            // Saved on tick charts, and on any chart set to run them continuously.
            tick_continuous: (self.on_ticks() || self.tick_continuous())
                .then_some(self.tick_continuous()),
            timeframe: crate::wsdoc::TimeframeDoc(self.timeframe),
            footprint: self.footprint.config(),
            styles: self.study_styles(),
            subpane: crate::subpane::saved(&self.subpane_ratios).0,
            subpanes: crate::subpane::saved(&self.subpane_ratios).1,
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
        self.marks.clear();
        self.trade.feed_changed(&self.store, follow, cx);
        cx.notify();
    }

    fn on_hidden_tick(&mut self) {
        // A hidden chart paints nothing: its wait to be shown is not receive → paint.
        if self.latency_hud {
            self.paint_latency.hidden(self.store.chart_revision());
        }
    }

    fn on_tick(&mut self) {
        if self.perf_stats && !self.frozen {
            self.rates_changed |= self.update_rates();
        }
        // The latency HUD refreshes once a second (render rebuilds its text).
        if self.latency_hud
            && !self.frozen
            && self
                .hud_at
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            self.rates_changed = true;
        }
    }

    /// Charts don't show depth: a depth batch that leaves the quote alone doesn't repaint.
    fn revision(&self) -> u64 {
        // The display zone's epoch too: a new zone redraws the axis.
        self.store
            .chart_revision()
            .wrapping_add(display_tz::epoch() << 48)
    }

    /// New indicator values (they arrive from the indicator thread after the store's), or new
    /// performance stats while shown (once a second). Hidden studies never ask for a repaint.
    fn changed(&mut self) -> bool {
        // A group's list follows its orders, on this tick, from the cache itself (a busy cache
        // waits for the next one; a frame's marks can be briefly empty), and closes if the
        // chart's account changed.
        let mut group = self.ct.account_changed(self.trade.account.as_ref());
        if self.ct.group_menu.is_some() && self.group_plot != self.plot {
            self.group_plot = self.plot;
            group = true;
        }
        if let Some(menu) = &mut self.ct.group_menu
            && let Some(s) = self.trade.try_cache()
        {
            group |= menu.refresh(|o| s.order(o).and_then(crate::dom_trade::order_mark).is_some());
        }
        // Kept current while hidden, but only asks for a repaint while shown.
        let marks = self.poll_marks() && !self.marks.hidden;
        // Drawings changed on any chart of this product (shown here).
        let rates = group
            | marks
            | std::mem::take(&mut self.rates_changed)
            | self.trade.changed()
            | (self.drawings.changed() && !self.drawings.hidden);
        if !self.visibility.draws_studies() {
            return rates;
        }
        let r = self.studies.revision();
        let l = self.levels.revision();
        // A live series pane turns STALE on the wall clock alone (no new data comes to say
        // so): look again every 5 s while one is shown.
        let series_tick = (self.studies.series_live() && lines_feed::levels::runtime::is_live())
            .then(|| lines_feed::now_utc().0 / 5_000);
        let tick = series_tick.is_some_and(|t| t != self.series_tick);
        if let Some(t) = series_tick {
            self.series_tick = t;
        }
        let changed = r != self.seen_studies || l != self.seen_levels || tick;
        self.seen_studies = r;
        self.seen_levels = l;
        changed || rates
    }
}

/// The delete-all confirmation: how many, for which symbol, and the key that undoes it.
fn clear_drawings_question(n: usize, symbol: &str) -> String {
    let plural = if n == 1 { "" } else { "s" };
    format!(
        "Delete all {n} drawing{plural} for {symbol}?  Enter: delete  ·  Esc: keep  ({} undoes)",
        crate::commands::hint(Command::Draw(DrawCmd::Undo))
    )
}

/// Centre of a drawing's axis tag for its line at plot-local `y`: inside the plot, and moved
/// one tag off the last-price tag (centre `last`) when that would cover it, to the line's side
/// if there is room, else the other.
fn drawing_tag_y(y: f32, last: Option<f32>, plot_h: f32) -> f32 {
    let at = cross_tag_y(y, plot_h);
    let Some(last) = last else {
        return at;
    };
    let reach = (tag_h() + DRAWING_TAG_H) * 0.5 + 1.0;
    if (at - last).abs() >= reach {
        return at;
    }
    let fits = |v: f32| v >= DRAWING_TAG_H * 0.5 && v <= plot_h - DRAWING_TAG_H * 0.5;
    let (above, below) = (last - reach, last + reach);
    let (first, second) = if y <= last {
        (above, below)
    } else {
        (below, above)
    };
    if fits(first) {
        first
    } else if fits(second) {
        second
    } else {
        at
    }
}

/// Centres of the drawings' axis tags (`out[i]` for the line at `lines[i]`, plot-local):
/// each as [`drawing_tag_y`], and a tag stepped off the last-price tag that lands on another
/// drawing's tag steps again, away from the last price, until it is clear (kept where it was
/// when that would leave the plot). Tags left on their own lines are never moved (two lines a
/// tick apart keep overlapping tags, as without a last price). Allocation-free.
fn drawing_tag_ys(lines: &[f32], last: Option<f32>, plot_h: f32, out: &mut [f32]) {
    let n = lines.len().min(out.len());
    for i in 0..n {
        out[i] = drawing_tag_y(lines[i], last, plot_h);
    }
    let Some(last) = last else {
        return;
    };
    let gap = DRAWING_TAG_H + 1.0;
    let fits = |v: f32| v >= DRAWING_TAG_H * 0.5 && v <= plot_h - DRAWING_TAG_H * 0.5;
    for i in 0..n {
        if out[i] == cross_tag_y(lines[i], plot_h) {
            continue; // on its own line
        }
        let away = if out[i] < last { -1.0 } else { 1.0 };
        // Settled: every tag left on its line, and the stepped ones before this one.
        let clash = |v: f32, out: &[f32]| {
            (0..n).any(|j| {
                j != i
                    && (j < i || out[j] == cross_tag_y(lines[j], plot_h))
                    && (out[j] - v).abs() < gap
            })
        };
        let mut v = out[i];
        for _ in 0..n {
            if !clash(v, out) {
                break;
            }
            v += away * gap;
        }
        if fits(v) && !clash(v, out) {
            out[i] = v;
        }
    }
}

/// Centre of the last-price tag (plot-local y). `max` last: the plot can be shorter than a
/// label (tiny windows).
fn last_tag_y(vp: &Viewport, p: Price, plot_h: f32) -> f32 {
    (vp.price_to_y(p.0 as f64) as f32)
        .min(plot_h - label_px())
        .max(0.0)
}

/// Centre of the crosshair's price tag, kept fully inside the plot's height.
/// Where a key goes while the style editor is open.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum EditorKey {
    /// The editor's (plain, Shift and Alt keys: moves, picks, hex digits, Esc).
    Editor,
    /// Undo or Redo: still the drawings' (one step each, the editor follows).
    Draw(DrawCmd),
    /// Another Ctrl/Cmd shortcut: on to the chart's other handling.
    Pass,
}

/// A restored chart's studies: the saved ones, then each initial one (`--vwap`) the saved
/// ones don't already have, ignoring styles (a saved styled VWAP ETH and `--vwap eth` are one
/// study, kept with its saved styles).
fn merge_indicators(
    saved: Vec<lines_indicators::IndicatorConfig>,
    initial: Vec<lines_indicators::IndicatorConfig>,
) -> Vec<lines_indicators::IndicatorConfig> {
    use lines_indicators::IndicatorConfig as C;
    let mut out = saved;
    for c in initial {
        // One Volume per chart: a saved one wins over the command line's.
        let volume = |x: &C| matches!(x, C::Volume { .. });
        if !out
            .iter()
            .any(|s| s.same_but_style(&c) || volume(s) && volume(&c))
        {
            out.push(c);
        }
    }
    out
}

fn editor_key(k: &gpui::Keystroke) -> EditorKey {
    let m = k.modifiers;
    if !m.control && !m.platform {
        return EditorKey::Editor;
    }
    match crate::commands::command_for(k) {
        Some(crate::commands::Command::Draw(d @ (DrawCmd::Undo | DrawCmd::Redo))) => {
            EditorKey::Draw(d)
        }
        _ => EditorKey::Pass,
    }
}

/// Each tool's style for new drawings from the settings ("Save as default for ..."; tools
/// without one: the default style). No settings global (tests): all defaults.
fn saved_tool_styles(cx: &App) -> crate::chart_draw::ToolStyles {
    let mut out = [lines_core::drawings::Style::DEFAULT; Tool::ALL.len()];
    if cx.has_global::<crate::picker_view::AppSettings>() {
        let saved = &crate::picker_view::settings(cx).drawing_styles;
        for (i, tool) in Tool::ALL.iter().enumerate() {
            if let Some(e) = saved.get(tool.key()) {
                out[i] = e.doc.to_style();
            }
        }
    }
    out
}

fn cross_tag_y(y: f32, plot_h: f32) -> f32 {
    y.min(plot_h - tag_h() * 0.5).max(tag_h() * 0.5)
}

/// The header's quote row: one line, its line height set to this (`text_sm` fits).
const QUOTE_ROW_H_100: f32 = 20.0;
/// [`QUOTE_ROW_H_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn quote_row_h() -> f32 {
    crate::zoom::f(QUOTE_ROW_H_100)
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
    studies.prepare(bars, d.profiles.footprint(), &*levels, vp, max_columns);
    // Placed by price before the scene (the labels' gutter needs them); its VWAP-relative
    // columns now, on the final time axis.
    levels.columns(bars, vp);
}

/// The level labels' gutter's widest (logical px at 100 % zoom).
const GUTTER_CAP: f32 = 130.0;

/// Where a measure's readout goes (plot-local top-left): on the far side of the measured
/// range from the live edge (left-aligned at the range's older end), above the box, or below
/// it when there's no room above; kept inside the plot. So it never covers the newest bars
/// to the right of the range.
fn measure_label_at(
    m: lines_core::drawings::Projected,
    (w, h): (f32, f32),
    (pw, ph): (f32, f32),
) -> (f32, f32) {
    let (left, right) = (m.p0.0.min(m.p1.0), m.p0.0.max(m.p1.0));
    let (top, bottom) = (m.p0.1.min(m.p1.1), m.p0.1.max(m.p1.1));
    // Never past the range's newer (right) end: a readout wider than the range grows left,
    // over older bars; kept inside the plot.
    let x = left.min(right - w).min(pw - w).max(0.0);
    let y = if top - h - 4.0 >= 0.0 {
        top - h - 4.0
    } else {
        (bottom + 4.0).min(ph - h).max(0.0)
    };
    (x, y)
}

/// A hovered tick bar: its last trade time, trade count, the chart's count and its state
/// (`None` on time bars).
/// A trade-built bar under the crosshair: last trade, trade count, the tick bars' size (none
/// for other kinds), state.
type TickInfo = Option<(UtcMillis, u32, Option<u32>, TickState)>;

/// A tick bar's state for the readout.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TickState {
    Full,
    /// Closed short by a session break.
    Partial,
    /// The newest bar, still short of its count.
    Forming,
}

/// `tick_marks`: the candle is a partial or forming tick bar.
const MARK_PARTIAL: u8 = 1;
/// `tick_marks`: the candle takes its session's first trade (a divider before it).
const MARK_SESSION: u8 = 2;

/// A trade-built bar's span and size for the readout: "14:32:05 to 14:32:41  500 trades"
/// (the display zone, as the time axis); short tick bars "… partial (41 of 233 trades)" or "… forming (41
/// of 233 trades)"; other kinds (`n`: none) "… partial (41 trades)". Allocation-free.
#[allow(clippy::too_many_arguments)]
pub fn write_tick_span(
    first: UtcMillis,
    last: UtcMillis,
    count: u32,
    n: Option<u32>,
    state: TickState,
    zone: &DisplayTz,
    out: &mut impl std::fmt::Write,
) -> std::fmt::Result {
    let (_, _, _, h0, m0, s0) = zone.civil(first);
    let (_, _, _, h1, m1, s1) = zone.civil(last);
    write!(out, "{h0:02}:{m0:02}:{s0:02} to {h1:02}:{m1:02}:{s1:02}  ")?;
    let word = match state {
        TickState::Full => "",
        TickState::Partial => "partial (",
        TickState::Forming => "forming (",
    };
    out.write_str(word)?;
    match n {
        Some(n) if state != TickState::Full => {
            let plural = if n == 1 { "" } else { "s" };
            write!(out, "{count} of {n} trade{plural}")?;
        }
        _ => {
            let plural = if count == 1 { "" } else { "s" };
            write!(out, "{count} trade{plural}")?;
        }
    }
    if state == TickState::Full {
        Ok(())
    } else {
        out.write_str(")")
    }
}

/// The tick series a chart on `tf` holds with `reset` (none on time bars).
fn wanted_tick_key(tf: Timeframe, reset: TickReset) -> Option<TickKey> {
    tf.bar_kind().map(|kind| TickKey { kind, reset })
}

/// The tick mode a restored chart takes: the saved one; with none saved (an older file, or a
/// time chart that never set it), `current` (the CLI's `--tick-continuous`) stays.
fn restored_tick_reset(state: &ChartState, current: TickReset) -> TickReset {
    match state.tick_continuous {
        Some(true) => TickReset::Continuous,
        Some(false) => TickReset::Session,
        None => current,
    }
}

/// One paint's view bookkeeping across a change of bars:
/// - bars dropped from the front (`trimmed`, counted since `baseline`) shift indices: a view
///   off the live edge stays on the same bars. `trimmed` is `None` while a tick series
///   rebuilds; the series that then lands sets a new baseline (its own trim count is not a
///   shift of this view).
/// - after a timeframe or tick-mode change (`anchor`: a time and its x), the bar holding that
///   time goes where it was, once there are bars: while a rebuild shows none the anchor is
///   kept for the bars that land ("switching keeps the pointer's time in place").
fn keep_view(
    vp: &mut Viewport,
    baseline: &mut u64,
    anchor: &mut Option<(UtcMillis, f64)>,
    trimmed: Option<u64>,
    bars: &[Bar],
    dragging: bool,
) {
    match trimmed {
        None => *baseline = u64::MAX,
        Some(trimmed) => {
            if trimmed > *baseline && (!vp.follow_live || vp.held) {
                vp.right_index -= (trimmed - *baseline) as f64;
            }
            *baseline = trimmed;
        }
    }
    if !bars.is_empty()
        && let Some((t, x)) = anchor.take()
    {
        // Live following resumes if the newest bar is then in view.
        let i = bars.partition_point(|b| b.time <= t).saturating_sub(1);
        vp.right_index += i as f64 - vp.x_to_index(x);
        if !dragging {
            vp.held = true;
            vp.set_held(false, bars.len());
        }
    }
}

/// B1: a switch to tick bars that rebuild keeps the pointer's time in place. While the
/// rebuild shows no bars the anchor waits; the landed series (which already trimmed bars) is
/// not taken as a trim of the old one.
#[cfg(test)]
#[test]
fn a_rebuilding_switch_keeps_the_pointers_time() {
    let minute = |i: i64| Bar {
        time: UtcMillis(1_800_000_000_000 + i * 60_000),
        open: Price(1),
        high: Price(2),
        low: Price(1),
        close: Price(2),
        volume: lines_core::Qty(1),
    };
    // A 1m chart scrolled back: 1000 bars, bar 600 under the pointer at x = 400.
    let one_min: Vec<Bar> = (0..1000).map(minute).collect();
    let mut vp = Viewport::default();
    vp.set_size(800.0, 600.0);
    vp.follow_live = false;
    vp.right_index = 600.0 + 400.0 / vp.bar_px;
    let mut baseline = 0;
    let mut anchor = None;
    keep_view(
        &mut vp,
        &mut baseline,
        &mut anchor,
        Some(0),
        &one_min,
        false,
    );
    let x = 400.0;
    assert_eq!(vp.x_to_index(x).round() as usize, 600);
    let t = one_min[600].time;
    // The switch: the anchor and an unknown baseline (`apply_timeframe`).
    anchor = Some((t, x));
    baseline = u64::MAX;
    // Rebuilding: no bars, no trim count. The anchor is kept.
    for _ in 0..3 {
        keep_view(&mut vp, &mut baseline, &mut anchor, None, &[], false);
    }
    assert_eq!(anchor, Some((t, x)), "kept while rebuilding");
    // Lands: 5 bars per minute, from minute 100 on, with 64 already trimmed.
    let ticks: Vec<Bar> = (0..4500)
        .map(|k| Bar {
            time: UtcMillis(minute(100).time.0 + k * 12_000),
            ..minute(0)
        })
        .collect();
    keep_view(&mut vp, &mut baseline, &mut anchor, Some(64), &ticks, false);
    assert_eq!(anchor, None);
    assert_eq!(baseline, 64);
    let i = vp.x_to_index(x).round() as usize;
    assert!(
        ticks[i].time <= t && t < ticks[i + 1].time,
        "the bar holding the pointer's time is under the pointer: {i}"
    );
    // A later trim of the tick series shifts the held view by just that much.
    let before = vp.right_index;
    keep_view(
        &mut vp,
        &mut baseline,
        &mut anchor,
        Some(64 + 16),
        &ticks,
        false,
    );
    assert_eq!(vp.right_index, before - 16.0);
}

/// The session divider's dashes and gaps (logical px).
const DIVIDER_DASH: f32 = 4.0;
const DIVIDER_GAP: f32 = 4.0;

/// A dashed outline of `r`, `hair` thick (marks a partial bar by shape).
fn dashed_box(window: &mut Window, r: Bounds<Pixels>, hair: f32, color: Hsla) {
    const ON: f32 = 3.0;
    const OFF: f32 = 2.0;
    let (x0, y0) = (f32::from(r.origin.x), f32::from(r.origin.y));
    let (w, h) = (f32::from(r.size.width), f32::from(r.size.height));
    let q = |x: f32, y: f32, w: f32, h: f32| {
        Bounds::new(point(px(x), px(y)), size(px(w.max(hair)), px(h.max(hair))))
    };
    let mut x = 0.0;
    while x < w {
        let d = ON.min(w - x);
        window.paint_quad(fill(q(x0 + x, y0, d, hair), color));
        window.paint_quad(fill(q(x0 + x, y0 + h - hair, d, hair), color));
        x += ON + OFF;
    }
    let mut y = 0.0;
    while y < h {
        let d = ON.min(h - y);
        window.paint_quad(fill(q(x0, y0 + y, hair, d), color));
        window.paint_quad(fill(q(x0 + w - hair, y0 + y, hair, d), color));
        y += ON + OFF;
    }
}

/// "manual" and the zone never overlap in the corner. The widths are the axis font's at 100 %
/// ("manual" about 38 px, "ET" 13, "+0545" 31); a uniform zoom scales everything alike, so
/// the cases that matter are the corner's width against the words: the usual axis, a pane so
/// narrow its corner is smaller than the axis, and the widest zone label.
#[cfg(test)]
#[test]
fn the_corner_never_overlaps_manual_and_the_zone() {
    let (manual, et, wide) = (38.0_f32, 13.0_f32, 31.0_f32);
    let (gap, margin) = (CORNER_GAP_100, CORNER_MARGIN_100);
    for corner in [AXIS_W_100, AXIS_W_100 * 0.75, 50.0, 30.0] {
        for zone in [et, wide] {
            let (mx, zx) = corner_layout(corner, Some(manual), Some(zone), gap, margin);
            let mx = mx.expect("manual always shows");
            if let Some(zx) = zx {
                assert!(
                    mx + manual + gap <= zx,
                    "no overlap in {corner}: {mx} vs {zx}"
                );
                assert!(zx + zone <= corner - margin + 0.01, "zone inside {corner}");
                assert!(mx >= margin - 0.01, "manual inside {corner}");
            }
        }
    }
    // The usual axis: ET fits beside "manual"; the widest zone yields and "manual" stays.
    let (mx, zx) = corner_layout(AXIS_W_100, Some(manual), Some(et), gap, margin);
    assert!(mx.is_some() && zx.is_some());
    let (mx, zx) = corner_layout(AXIS_W_100, Some(manual), Some(wide), gap, margin);
    assert!(mx.is_some() && zx.is_none(), "+0545 yields to manual");
    // A corner too small for both: manual alone.
    let (mx, zx) = corner_layout(50.0, Some(manual), Some(et), gap, margin);
    assert!(mx.is_some() && zx.is_none());
    // Alone, each is centred.
    assert_eq!(
        corner_layout(78.0, None, Some(14.0), gap, margin),
        (None, Some(32.0))
    );
    assert_eq!(
        corner_layout(78.0, Some(38.0), None, gap, margin),
        (Some(20.0), None)
    );
}

#[cfg(test)]
#[test]
fn tick_span_text() {
    let a = UtcMillis::parse_iso("2026-11-05T14:32:05.250Z").unwrap();
    let b = UtcMillis::parse_iso("2026-11-05T14:32:41Z").unwrap();
    let text = |count, state| {
        let mut s = FixedStr::<64>::new();
        write_tick_span(a, b, count, Some(233), state, &DisplayTz::utc(), &mut s).unwrap();
        s.as_str().to_owned()
    };
    assert_eq!(
        text(233, TickState::Full),
        "14:32:05 to 14:32:41  233 trades"
    );
    assert_eq!(text(1, TickState::Full), "14:32:05 to 14:32:41  1 trade");
    assert_eq!(
        text(41, TickState::Partial),
        "14:32:05 to 14:32:41  partial (41 of 233 trades)"
    );
    assert_eq!(
        text(41, TickState::Forming),
        "14:32:05 to 14:32:41  forming (41 of 233 trades)"
    );
}

/// A restored chart holds the series its saved mode asks for, also when it is already on that
/// timeframe (the old code kept the session series), and goes back to session resets when the
/// saved mode is off. N8: a chart saved with no mode (an older file, a time chart) keeps the
/// mode it started with (`--tick-continuous`): absent is not "off".
#[cfg(test)]
#[test]
fn restored_tick_mode_picks_the_series() {
    let mut state: ChartState = serde_json::from_str(r#"{ "bar_px": 8.0 }"#).unwrap();
    assert_eq!(state.tick_continuous, None);
    for cli in [TickReset::Session, TickReset::Continuous] {
        assert_eq!(
            restored_tick_reset(&state, cli),
            cli,
            "absent keeps {cli:?}"
        );
    }
    state.tick_continuous = Some(true);
    assert_eq!(
        restored_tick_reset(&state, TickReset::Session),
        TickReset::Continuous
    );
    let tf = Timeframe::Ticks(233);
    let held = wanted_tick_key(tf, TickReset::Session);
    let want = wanted_tick_key(tf, restored_tick_reset(&state, TickReset::Session));
    assert_ne!(held, want, "same timeframe, other mode: re-acquired");
    assert_eq!(want.unwrap().reset, TickReset::Continuous);
    state.tick_continuous = Some(false);
    assert_eq!(
        wanted_tick_key(tf, restored_tick_reset(&state, TickReset::Continuous)),
        held,
        "saved off wins over the CLI"
    );
    assert_eq!(
        wanted_tick_key(Timeframe::Minutes(5), TickReset::Continuous),
        None
    );
}

/// The bars a chart shows: its timeframe's (derived, or the store's tick bars), or the
/// store's 1-minute bars.
fn bars_view<'a>(
    derived: &'a Option<Derived>,
    ticks: Option<TickKey>,
    d: &'a ChartData,
) -> BarsView<'a> {
    match (derived, ticks) {
        (_, Some(k)) => d.ticks.view(k),
        (Some(dv), None) => dv.view(),
        (None, None) => BarsView::fixed(d.series.bars(), d.series.period_ms()),
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

/// The OHLCV readout box's bottom (it sits 6 px from the top, `label_px() × 1.6` tall), plus
/// a gap: the footprint keeps its numbers below it.
const READOUT_BOTTOM_100: f32 = 6.0 + LABEL_PX_100 * 1.6 + 4.0;
/// [`READOUT_BOTTOM_100`] at the UI zoom (`crate::zoom`).
#[inline]
fn readout_bottom() -> f32 {
    crate::zoom::f(READOUT_BOTTOM_100)
}

/// Widest the level-source legend grows (logical px); longer text ends in an ellipsis.
const LEGEND_MAX_W: f32 = 360.0;

/// Charts narrower than this show the latency HUD's p99s only (no sparklines).
const HUD_COMPACT_BELOW_PX: f32 = 1_200.0;

/// The latency HUD's compact form for a chart `w` px wide (0: not laid out yet).
fn hud_compact(w: f32) -> bool {
    // In 100 % units (`crate::zoom`): the header's text grows with the zoom.
    w > 0.0 && w / crate::zoom::get() < HUD_COMPACT_BELOW_PX
}

impl Render for ChartView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_level_styles(cx);
        // Text shaped at another UI zoom is the wrong size: shaped again on demand.
        let zoom = crate::zoom::generation();
        if zoom != self.zoom_generation {
            self.zoom_generation = zoom;
            self.labels.clear();
            self.readout = None;
            self.note_lines.clear();
            self.note_entry_line = None;
            self.measure_lines.clear();
        }
        // A group's list closes when none of its orders is left.
        if self
            .ct
            .group_menu
            .as_ref()
            .is_some_and(|m| m.open_count() == 0)
        {
            self.close_popover(Kind::OrderGroup, cx);
        }
        // A size being typed closes when the chart loses focus (a chart has no switch to drop).
        let focus = self.focus.clone();
        crate::trade_bar::PaneTrade::watch_focus(self, &focus, window, cx);
        let t = self.theme;
        let entity = cx.entity();
        // The workspace refreshes every window when this preference changes.
        self.perf_stats = cx
            .try_global::<crate::workspace::Workspace>()
            .is_some_and(|w| w.show_perf_stats);
        self.latency_hud = cx
            .try_global::<crate::workspace::Workspace>()
            .is_some_and(|w| w.show_latency_hud);
        self.paint_latency.prepare(
            self.latency_hud && !self.frozen,
            self.store.chart_revision(),
        );
        self.refresh_hud(cx);
        // This frame's level-source snapshot: the legend below and the levels painted later
        // in this frame both describe it.
        let market_time = self.store.try_read().and_then(|d| d.market_time);
        self.levels.snapshot(market_time);
        // The shown sources' label texts, measured once each (kept by text) for the labels'
        // gutter, here in render rather than in paint (a hidden pane isn't rendered).
        if self.visibility.draws_studies() && self.visibility.draws_labels() {
            let theme = self.theme;
            self.levels
                .measure_labels(|t| crate::levels_layer::measure(t, &theme, window));
        }
        let today = pane::today(self.frozen, &self.store);
        let overlay = self.picker.overlay(
            &t,
            &crate::sources::pick_if(self.picker.is_open(), &self.feed, cx),
            today,
            Self::picker_click,
            cx,
        );
        let offer = self.common.offer_switch;
        if let Some(body) = pane::unavailable_body(&self.feed, &t, offer, cx) {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(t.bg)
                .child(self.header(window, cx))
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
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg)
            .child(self.header(window, cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .size_full()
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_down))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_exit(cx.listener(Self::on_mouse_exit))
                    .on_scroll(cx.listener(Self::on_scroll))
                    .when_some(
                        self.drawings
                            .measure_grip()
                            .zip(self.drawings.measure_shown())
                            .map(|(g, (p, _))| match g {
                                crate::chart_draw::Grip::Move
                                    if self.drawings.measure_dragging() =>
                                {
                                    gpui::CursorStyle::ClosedHand
                                }
                                g => g.cursor(p.p0, p.p1),
                            }),
                        |d, c| d.cursor(c),
                    )
                    .when(
                        self.drawings.active() && self.drawings.measure_grip().is_none(),
                        |d| d.cursor_crosshair(),
                    )
                    .when(
                        !self.drawings.active()
                            && self.drawings.measure_grip().is_none()
                            && self.drawings.hover_hit().is_some(),
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
                    .when_some(self.order_strip_view(&t, cx), |d, s| d.child(s))
                    .when_some(
                        crate::order_group_view::layer(self, window, &t, cx),
                        |d, m| d.child(m),
                    )
                    .when_some(
                        self.study_menu.as_ref().map(|m| {
                            let items = self.menu_items(m, cx);
                            let menu = crate::study_menu_view::render(m, &items, &t, cx);
                            // Over the window with a backdrop under it (`crate::popover`).
                            let dismiss: popover::OnDismiss =
                                Box::new(cx.listener(|v, _: &MouseDownEvent, _, cx| {
                                    v.close_popover(Kind::Studies, cx)
                                }));
                            popover::floating(popover::Panel::Studies, "Studies", &t, menu, dismiss)
                        }),
                        |d, m| d.child(m),
                    )
                    .when(self.anchoring.is_some(), |d| {
                        d.child(crate::study_menu_view::anchor_banner(&t))
                    })
                    .when_some(self.ct.strip_pick, |d, o| {
                        d.child(crate::study_menu_view::pick_banner(
                            &t,
                            if o == crate::chart_trade::StripOrder::BuyLimit {
                                "Click a price for the buy limit · Esc cancels"
                            } else {
                                "Click a price for the sell limit · Esc cancels"
                            },
                        ))
                    })
                    .when_some(self.style_editor_layer(&t, cx), |d, e| d.child(e))
                    .when_some(overlay, |d, overlay| d.child(overlay))
                    .when_some(self.timeframe_entry(&t), |d, e| d.child(e))
                    .when_some(self.drawing_banner(&t), |d, b| d.child(b))
                    .when_some(self.strategy_mark_tip(&t), |d, b| d.child(b)),
            )
    }
}

/// The price a strip limit's pick press found: `Ok(None)` with no bars there, `Err` with the
/// market data busy.
type PickPrice = Result<Option<Price>, &'static str>;

/// The chart's popovers the symbol picker closes when it opens: all of them (their window-wide
/// backdrops would take the picker's clicks). Pane menus belong to the window.
const PICKER_CLOSES: [Kind; 5] = [
    Kind::Timeframe,
    Kind::Account,
    Kind::Studies,
    Kind::StyleEditor,
    Kind::OrderGroup,
];

impl crate::picker_view::PickerHost for ChartView {
    fn picker_ui(&mut self) -> &mut crate::picker_view::PickerUi {
        &mut self.picker
    }
}

/// Whether a cached label must be shaped (again): none is cached under `key`, or the one cached
/// was shaped in another color (a theme switch, a restyled study: text keeps the color it was
/// shaped in).
fn needs_shaping<K: Eq + std::hash::Hash, T>(
    cache: &HashMap<K, (Hsla, T)>,
    key: K,
    color: Hsla,
) -> bool {
    cache
        .get(&key)
        .is_none_or(|(shaped_in, _)| *shaped_in != color)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A theme switch reshapes chart labels: a label cached in one theme's color is shaped
    /// again when asked for in another theme's, and kept when the color is the same.
    #[test]
    fn labels_are_shaped_again_in_a_new_color() {
        let (dark, light) = (
            Theme::dark(crate::theme::Palette::Standard),
            Theme::light(crate::theme::Palette::Standard),
        );
        let mut cache: HashMap<(LabelKind, i64), (Hsla, ())> = HashMap::new();
        let key = (LabelKind::Price, 601_000);
        assert!(needs_shaping(&cache, key, dark.text_dim));
        cache.insert(key, (dark.text_dim, ()));
        assert!(!needs_shaping(&cache, key, dark.text_dim));
        assert!(needs_shaping(&cache, key, light.text_dim));
    }

    /// The question names the symbol and then the undo key (they were once swapped: "for
    /// Ctrl+Z? … (ES undoes)").
    #[test]
    fn the_delete_all_question_names_the_symbol_then_the_undo_key() {
        let q = clear_drawings_question(3, "ES");
        let undo = crate::commands::hint(Command::Draw(DrawCmd::Undo));
        assert!(q.starts_with("Delete all 3 drawings for ES?"), "{q}");
        assert!(q.ends_with(&format!("({undo} undoes)")), "{q}");
        assert!(clear_drawings_question(1, "NQ").starts_with("Delete all 1 drawing for NQ?"));
    }

    /// N4: a restored chart keeps a saved styled study and doesn't add the same study from
    /// the command line again (styles aside); a different one is added.
    #[test]
    fn restore_merges_initial_studies_ignoring_styles() {
        use lines_indicators::{IndicatorConfig, Reset, VwapSpec};
        let vwap = |reset: Reset| IndicatorConfig::Vwap {
            spec: VwapSpec {
                reset,
                ..VwapSpec::default()
            },
            color: 0,
            display: Default::default(),
            style: Default::default(),
        };
        let mut styled = vwap(Reset::Eth);
        styled
            .style_mut()
            .insert("line".into(), serde_json::json!({"color": "#f719cb"}));
        let out = merge_indicators(vec![styled.clone()], vec![vwap(Reset::Eth)]);
        assert_eq!(
            out,
            vec![styled.clone()],
            "one VWAP ETH, with its saved styles"
        );
        // Restyled through the config's own fields (line color index, width): still the same.
        let mut recolored = vwap(Reset::Eth);
        if let IndicatorConfig::Vwap { color, display, .. } = &mut recolored {
            *color = 3;
            display.width = 4;
        }
        let out = merge_indicators(vec![recolored.clone()], vec![vwap(Reset::Eth)]);
        assert_eq!(out, vec![recolored], "color and width are styles");
        let out = merge_indicators(vec![styled.clone()], vec![vwap(Reset::Rth)]);
        assert_eq!(out, vec![styled, vwap(Reset::Rth)]);
        // One Volume per chart: a saved one wins over the command line's, whatever its options.
        let volume = |color_by_candle| IndicatorConfig::Volume {
            spec: lines_indicators::VolumeSpec {
                color_by_candle,
                ..Default::default()
            },
            display: Default::default(),
            style: Default::default(),
        };
        let out = merge_indicators(vec![volume(false)], vec![volume(true), vwap(Reset::Eth)]);
        assert_eq!(out, vec![volume(false), vwap(Reset::Eth)]);
        let out = merge_indicators(vec![], vec![volume(true)]);
        assert_eq!(out, vec![volume(true)]);
    }
    use lines_core::{Aggressor, Capabilities, Month, Qty};
    use lines_feed::catalog;
    use lines_feed::provider::MarketEvent;

    use crate::profile::{ProfileConfig, ProfileMode};

    /// Opening the symbol picker closes every popover a chart owns, the style editor
    /// included (its backdrop would eat the picker's clicks).
    #[test]
    fn opening_the_picker_closes_every_chart_popover() {
        for kind in [
            Kind::Studies,
            Kind::Timeframe,
            Kind::Account,
            Kind::Menu,
            Kind::StyleEditor,
            Kind::OrderGroup,
        ] {
            // Exhaustive: a new kind fails to compile here until it's decided.
            let chart_owned = match kind {
                Kind::Studies
                | Kind::Timeframe
                | Kind::Account
                | Kind::StyleEditor
                | Kind::OrderGroup => true,
                Kind::Menu => false,
            };
            assert_eq!(PICKER_CLOSES.contains(&kind), chart_owned, "{kind:?}");
        }
    }

    /// Undo while the style editor is open: the key the chart keeps for itself
    /// (`editor_key`) runs the drawings' undo, and the open editor shows the style as it
    /// was (re-synced), still open on its drawing.
    #[test]
    fn undo_with_the_style_editor_open_resyncs_it() {
        use lines_core::drawings::{Anchor, Drawing, Style, Tool};
        let es = catalog::get(catalog::ES).unwrap();
        let at = Anchor {
            time: UtcMillis(1_800_000_000_000),
            price: Price(600_000),
        };
        let mut line = Drawing::new(Tool::HLine, at, at, Style::DEFAULT);
        line.id = 1;
        let mut d = crate::chart_draw::ChartDrawings::new(
            crate::drawings_store::ProductDrawings::ephemeral(es, vec![line]),
        );
        assert!(d.open_editor(1));
        d.edit_style(|e| e.click(2, 3)); // Width 4
        let width = |d: &mut crate::chart_draw::ChartDrawings| {
            d.editor()
                .unwrap()
                .drawing_style(&Style::DEFAULT)
                .line
                .width
        };
        assert_eq!(width(&mut d), 4);
        let k = |s: &str| gpui::Keystroke::parse(Box::leak(s.to_owned().into_boxed_str())).unwrap();
        let ctrl = if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        };
        let was = Style::DEFAULT.line.width;
        for (key, want) in [(format!("{ctrl}-z"), was), (format!("{ctrl}-y"), 4)] {
            let EditorKey::Draw(cmd) = editor_key(&k(&key)) else {
                panic!("{key} goes to the drawings");
            };
            d.run(cmd);
            assert!(d.editing(), "still open");
            assert_eq!(width(&mut d), want, "{key}: the editor follows");
        }
    }

    /// A drawing's axis tag the last-price tag would cover moves one tag off it, to the
    /// line's side (the other near an edge); clear of it, it stays on its line.
    #[test]
    fn drawing_tags_step_off_the_last_price_tag() {
        let ph = 400.0;
        assert_eq!(drawing_tag_y(100.0, Some(300.0), ph), 100.0);
        assert_eq!(drawing_tag_y(100.0, None, ph), 100.0);
        let reach = (TAG_H_100 + DRAWING_TAG_H) * 0.5 + 1.0;
        // Just above the last price: above its tag; just below: below.
        assert_eq!(drawing_tag_y(198.0, Some(200.0), ph), 200.0 - reach);
        assert_eq!(drawing_tag_y(203.0, Some(200.0), ph), 200.0 + reach);
        // At the top edge there's no room above: below.
        let y = drawing_tag_y(4.0, Some(9.0), ph);
        assert_eq!(y, 9.0 + reach);
        // Never overlapping, never out of the plot.
        for line in [0.0f32, 5.0, 150.0, 199.0, 200.0, 201.0, 395.0, 400.0] {
            for last in [9.0f32, 200.0, 390.0] {
                let y = drawing_tag_y(line, Some(last), ph);
                assert!((y - last).abs() >= reach - 0.01, "{line} {last}: {y}");
                assert!(y >= DRAWING_TAG_H * 0.5 && y <= ph - DRAWING_TAG_H * 0.5);
            }
        }
    }

    /// A tag stepped off the last-price tag never lands on another drawing's tag: it steps
    /// again, away from the last price; tags on their own lines stay there.
    #[test]
    fn stepped_drawing_tags_clear_the_other_tags() {
        let ph = 400.0;
        let reach = (TAG_H_100 + DRAWING_TAG_H) * 0.5 + 1.0;
        let gap = DRAWING_TAG_H + 1.0;
        // A line at the last price (steps above, the line's side: y <= last) and another
        // drawing's line exactly where it would land.
        let lines = [200.0 - reach, 200.0];
        let mut at = [0.0; 2];
        drawing_tag_ys(&lines, Some(200.0), ph, &mut at);
        assert_eq!(at[0], 200.0 - reach, "on its own line: unmoved");
        assert_eq!(
            at[1],
            200.0 - reach - gap,
            "stepped twice, away from the last price"
        );
        // Several lines crowding the last price from both sides: no stepped tag overlaps any
        // other tag or the last-price tag, and all stay inside the plot.
        let lines = [
            196.0,
            199.0,
            201.0,
            204.0,
            200.0 - reach,
            200.0 + reach,
            180.0,
        ];
        let mut at = [0.0; 7];
        drawing_tag_ys(&lines, Some(200.0), ph, &mut at);
        for i in 0..lines.len() {
            assert!((at[i] - 200.0).abs() >= reach - 0.01, "{at:?}");
            assert!(at[i] >= DRAWING_TAG_H * 0.5 && at[i] <= ph - DRAWING_TAG_H * 0.5);
            if at[i] != lines[i] {
                for j in 0..lines.len() {
                    assert!(
                        j == i || (at[j] - at[i]).abs() >= gap - 0.01,
                        "{i} {j}: {at:?}"
                    );
                }
            }
        }
        // No last price: every tag on its line.
        drawing_tag_ys(&lines, None, ph, &mut at);
        assert_eq!(at, lines);
    }

    /// While the style editor is open it takes plain, Shift and Alt keys (moves, hex digits,
    /// Esc closes it), and Undo/Redo still reach the drawings (the workspace leaves them to a
    /// pane taking text); other Ctrl/Cmd shortcuts pass on.
    #[test]
    fn style_editor_keys_route_undo_redo_and_esc() {
        let k = |s: &str| gpui::Keystroke::parse(Box::leak(s.to_owned().into_boxed_str())).unwrap();
        let ctrl = if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        };
        assert_eq!(
            editor_key(&k(&format!("{ctrl}-z"))),
            EditorKey::Draw(DrawCmd::Undo)
        );
        assert_eq!(
            editor_key(&k(&format!("{ctrl}-y"))),
            EditorKey::Draw(DrawCmd::Redo)
        );
        assert_eq!(
            editor_key(&k(&format!("{ctrl}-shift-z"))),
            EditorKey::Draw(DrawCmd::Redo)
        );
        for plain in ["escape", "a", "shift-tab", "alt-s", "enter", "left"] {
            assert_eq!(editor_key(&k(plain)), EditorKey::Editor, "{plain}");
        }
        assert_eq!(editor_key(&k(&format!("{ctrl}-shift-p"))), EditorKey::Pass);
        // Esc reaches the editor and closes it.
        let mut e = crate::style_editor::StyleEditor::for_drawing(
            lines_core::drawings::Tool::Trend,
            &lines_core::drawings::Style::DEFAULT,
        );
        assert_eq!(
            e.key("escape", false, None),
            crate::style_editor::Outcome::Close
        );
        // The workspace leaves Undo to a pane taking text: this route is the only one.
        assert!(!crate::commands::runs_while_typing(
            &k(&format!("{ctrl}-z")),
            Command::Draw(DrawCmd::Undo)
        ));
    }

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
                style: Default::default(),
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

    /// The measure's readout sits at the measured range's older (left) end, above it, so it
    /// never covers the newer bars to its right; below when there's no room above.
    #[test]
    fn measure_readout_stays_off_the_live_edge() {
        let m = |p0: (f32, f32), p1: (f32, f32)| lines_core::drawings::Projected {
            id: 1,
            tool: lines_core::drawings::Tool::Measure,
            style: Default::default(),
            p0,
            p1,
            handles: [p0, p1],
            n_handles: 2,
            prices: [Price(0); 2],
        };
        let plot = (1000.0, 600.0);
        // Drawn right to left or left to right: the label starts at the left end.
        for (a, b) in [
            ((300.0, 400.0), (900.0, 200.0)),
            ((900.0, 200.0), (300.0, 400.0)),
        ] {
            let (x, y) = measure_label_at(m(a, b), (250.0, 20.0), plot);
            assert_eq!((x, y), (300.0, 176.0));
            assert!(
                x + 250.0 <= 900.0,
                "inside the range, not past its newer end"
            );
        }
        // At the top of the plot: below the box.
        assert_eq!(
            measure_label_at(m((300.0, 10.0), (900.0, 300.0)), (250.0, 20.0), plot),
            (300.0, 304.0)
        );
        // Narrower than its readout (a few bars at the live edge): the readout ends at the
        // range's newer end and grows left, never over the bars after it.
        assert_eq!(
            measure_label_at(m((900.0, 400.0), (990.0, 200.0)), (250.0, 20.0), plot).0,
            740.0
        );
        assert_eq!(
            measure_label_at(m((20.0, 400.0), (60.0, 200.0)), (250.0, 20.0), plot).0,
            0.0
        );
    }

    /// The measure shows what the user chose and the P&L display setting allows: off hides
    /// ticks and money whatever the choice.
    #[test]
    fn measure_follows_pnl_display() {
        use crate::chart_draw::MeasureShow;
        use lines_core::drawings::MeasureParts;
        let p = |ticks, money| MeasureParts {
            ticks,
            money,
            ..MeasureParts::ALL
        };
        let all = MeasureShow::default();
        assert_eq!(all.parts(PnlDisplay::Off), p(false, false));
        assert_eq!(all.parts(PnlDisplay::Ticks), p(true, false));
        assert_eq!(all.parts(PnlDisplay::Currency), p(false, true));
        assert_eq!(all.parts(PnlDisplay::Both), p(true, true));
        let mut some = all;
        some.toggle(crate::chart_draw::MeasurePart::Pnl);
        some.toggle(crate::chart_draw::MeasurePart::Points);
        let got = some.parts(PnlDisplay::Both);
        assert!(got.ticks && !got.money && !got.points && got.bars && got.time);
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
