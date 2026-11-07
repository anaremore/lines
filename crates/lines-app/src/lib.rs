//! Lines as a library: the `lines` binary is [`run`] with no extensions. Official builds
//! (decision 0020) call the same entry point with their own sources registered through
//! [`Extensions`]; nothing in this crate names them.

mod bench;
mod brackets;
mod chart_draw;
mod chart_trade;
mod chart_view;
mod commands;
#[cfg(feature = "screenshot")]
mod demo_broker;
mod diagnostics;
mod dom_orders;
mod dom_trade;
mod dom_trail;
mod dom_view;
mod drawings_store;
mod dropdown;
mod feeds;
mod footprint;
#[cfg(feature = "screenshot")]
mod gallery;
mod icons;
mod journal_view;
mod latency_hud;
mod layout;
mod layout_rec;
mod levels_layer;
mod logs;
mod menu;
mod notice;
mod pane;
mod pane_columns;
mod pane_window;
mod persist;
mod picker;
mod picker_view;
mod pnl;
mod popover;
mod profile;
mod redact;
#[cfg(feature = "screenshot")]
mod screenshot;
mod settings_schema;
mod settings_view;
mod strategy_host;
mod strategy_panel;
mod strategy_view;
mod studies;
mod study_menu;
mod study_menu_view;
mod study_style;
mod style_doc;
mod style_editor;
mod style_editor_view;
mod subpane;
mod tabstate;
mod tape_view;
mod text;
mod theme;
mod tooltip;
mod trade_bar;
mod trading;
mod versioned;
mod workspace;
mod wsdoc;
mod zipw;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::builder::PossibleValue;
use clap::{CommandFactory, FromArgMatches, Parser, ValueEnum};
use gpui::App;
use lines_core::UtcMillis;
use lines_feed::cqg::mock::MockConfig;
use lines_feed::extension::ExtensionSource;
pub use lines_feed::extension::{BoxFuture, DynTransport, SourceExtension, Timing};
pub use lines_feed::levels::LevelSource;
use lines_feed::registry::FeedRegistry;
use lines_feed::{FeedConfig, Source, catalog};
pub use lines_trading::extension::{AccountKind, AccountSource, BrokerExtension, ExtensionAccount};
use profile::{ProfileConfig, ProfileMode, ProfileSession};

use crate::dom_view::DomOptions;
use crate::feeds::Feeds;
use crate::layout::Preset;
use crate::picker_view::AppSettings;
use crate::tape_view::TapeOptions;
use crate::workspace::{PaneEnv, Workspace};
use crate::wsdoc::{TargetDoc, WorkspaceDoc};

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SourceKind {
    /// In-process mock of the CQG WebAPI protocol.
    Mock,
    /// Real CQG WebAPI (needs credentials). Only in builds with `--features vendor-cqg`.
    #[cfg(feature = "vendor-cqg")]
    Cqg,
    /// Coinbase Exchange public feed (free, 24/7; dev tool).
    Coinbase,
    /// Kraken public feeds: spot (BTC/USD.KR, ETH/USD.KR) and perpetuals (BTC-PERP.KR,
    /// ETH-PERP.KR). Free, 24/7, book verified against Kraken's checksum.
    Kraken,
    /// A `.lfr` recording (provider, product and contract come from the file).
    Replay,
}

/// What the entry point adds to the built-in sources and brokers (decision 0020). Empty for
/// the public `lines` binary.
#[derive(Default, Clone)]
pub struct Extensions {
    sources: Vec<Arc<dyn SourceExtension>>,
    level_sources: Vec<Arc<dyn LevelSource>>,
    brokers: Vec<Arc<dyn BrokerExtension>>,
    strategies: Vec<Arc<dyn lines_strategy::StrategyExtension>>,
}

impl Extensions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a market-data source, selected with `--source <name>`.
    pub fn with_source(mut self, source: Arc<dyn SourceExtension>) -> Self {
        self.sources.push(source);
        self
    }

    pub fn sources(&self) -> &[Arc<dyn SourceExtension>] {
        &self.sources
    }

    /// Add an external level source (decision 0022): its levels show on the charts of the
    /// products it serves.
    pub fn with_level_source(mut self, source: Arc<dyn LevelSource>) -> Self {
        self.level_sources.push(source);
        self
    }

    pub fn level_sources(&self) -> &[Arc<dyn LevelSource>] {
        &self.level_sources
    }

    /// Add a broker (decision 0004): its capabilities, its accounts and the `Broker` the trading
    /// core drives. Names must be unique and not a built-in broker's.
    pub fn with_broker(mut self, broker: Arc<dyn BrokerExtension>) -> Self {
        self.brokers.push(broker);
        self
    }

    pub fn brokers(&self) -> &[Arc<dyn BrokerExtension>] {
        &self.brokers
    }

    /// Add a strategy (decision 0025): `--backtest <name>` runs it over a
    /// replay. Names must be unique.
    pub fn with_strategy(mut self, strategy: Arc<dyn lines_strategy::StrategyExtension>) -> Self {
        self.strategies.push(strategy);
        self
    }

    pub fn strategies(&self) -> &[Arc<dyn lines_strategy::StrategyExtension>] {
        &self.strategies
    }

    /// Check the registrations (broker and strategy names) before anything runs.
    pub fn validate(&self) -> Result<()> {
        lines_trading::extension::validate(&self.brokers)?;
        lines_strategy::extension::validate(&self.strategies)
    }
}

/// The `--source` value, resolved.
#[derive(Clone)]
enum SourceChoice {
    Builtin(SourceKind),
    Extension(ExtensionSource),
}

impl SourceChoice {
    fn parse(name: &str, ext: &Extensions) -> Option<Self> {
        if let Some(e) = ext.sources.iter().find(|e| e.name() == name) {
            return Some(SourceChoice::Extension(ExtensionSource(e.clone())));
        }
        SourceKind::from_str(name, true)
            .ok()
            .map(SourceChoice::Builtin)
    }
}

/// `--bracket-units`: bracket distances in ticks or points (`crate::brackets::BracketUnits`).
#[derive(Copy, Clone, Debug, ValueEnum)]
enum BracketUnitsArg {
    Ticks,
    Points,
}

impl BracketUnitsArg {
    #[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
    fn units(self) -> brackets::BracketUnits {
        match self {
            BracketUnitsArg::Ticks => brackets::BracketUnits::Ticks,
            BracketUnitsArg::Points => brackets::BracketUnits::Points,
        }
    }
}

/// `--pnl`: the P&L display (`crate::pnl::PnlDisplay`).
#[derive(Copy, Clone, Debug, ValueEnum)]
enum PnlArg {
    Off,
    Ticks,
    Currency,
    Both,
}

impl PnlArg {
    #[cfg_attr(not(feature = "screenshot"), allow(dead_code))]
    fn mode(self) -> pnl::PnlDisplay {
        match self {
            PnlArg::Off => pnl::PnlDisplay::Off,
            PnlArg::Ticks => pnl::PnlDisplay::Ticks,
            PnlArg::Currency => pnl::PnlDisplay::Currency,
            PnlArg::Both => pnl::PnlDisplay::Both,
        }
    }
}

/// Up/down color preset.
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum PaletteKind {
    /// Mint up, rose down.
    Standard,
    /// Blue up, orange down (red-green color blindness).
    #[value(alias = "cb", alias = "colourblind")]
    Colorblind,
}

/// Which candle bodies are hollow.
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum HollowKind {
    Up,
    Down,
}

/// A one-tab layout for this run (`--workspace` for more).
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum LayoutKind {
    /// Chart beside the DOM, time & sales under the DOM.
    Trading,
    /// The chart alone.
    Chart,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum ThemeKind {
    Dark,
    Light,
}

/// Lines: futures charting terminal.
#[derive(Parser, Debug)]
#[command(version)]
struct Cli {
    /// Where market data comes from.
    #[arg(long, default_value = "mock")]
    source: String,
    /// Show this product (followed to its front month: ES, NQ, MES, MNQ, ZN, BTC/USD,
    /// ETH/USD, BTC/USD.KR, ETH/USD.KR, BTC-PERP.KR, ETH-PERP.KR) or contract (ESZ26, MNQH2027) in every pane, for this run only (the saved
    /// workspace is left alone).
    #[arg(long)]
    instrument: Option<String>,
    /// Record every raw feed frame to this .lfr file.
    #[arg(long)]
    record: Option<PathBuf>,
    /// Recording to play back (with --source replay).
    #[arg(long)]
    replay: Option<PathBuf>,
    /// Replay speed: 1 = real time, 10 = 10x, 0 = as fast as possible.
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// Replay: stop at this UTC time (e.g. 2026-11-06T14:05:00Z). Default: end of file.
    #[arg(long, value_parser = parse_time)]
    at: Option<UtcMillis>,
    /// Mock: updates per second.
    #[arg(long, default_value_t = 5_000)]
    rate: u32,
    /// Mock: updates per frame.
    #[arg(long, default_value_t = 50)]
    batch: u32,
    /// Mock: drop the connection every N seconds (exercise reconnect + back-fill).
    #[arg(long)]
    disconnect_every: Option<u64>,
    /// CQG WebAPI endpoint. Credentials: LINES_CQG_USER / LINES_CQG_PASSWORD.
    #[cfg(feature = "vendor-cqg")]
    #[arg(long, default_value = lines_feed::cqg::DEMO_URL)]
    cqg_url: String,
    #[cfg(feature = "vendor-cqg")]
    #[arg(long, default_value = "CQG")]
    private_label: String,
    #[cfg(feature = "vendor-cqg")]
    #[arg(long, default_value = "WebAPITest")]
    client_app_id: String,
    /// Don't use the local market-data store. By default live feeds (Coinbase, CQG) keep
    /// trades and best bid/ask per contract in the OS data directory (`LINES_DATA_DIR`
    /// overrides), show the cached bars at startup and fill the rest from the feed.
    #[arg(long)]
    no_store: bool,
    /// Also store order-book depth for these products (e.g. ES,NQ). Off by default: large.
    #[arg(long, value_delimiter = ',')]
    store_depth: Vec<String>,
    /// Local store size budget in GiB; the oldest trade dates are removed beyond it.
    #[arg(long, default_value_t = 4.0)]
    store_budget_gb: f64,
    /// Use the local store under DIR (its `store-v1/`) for any source, the mock, replays and
    /// screenshots included (evidence and tests with a synthetic stored session). Without it
    /// those never store. The run writes into DIR as a live feed would: give a screenshot a
    /// fresh copy each time for repeatable images.
    #[arg(long, value_name = "DIR")]
    store_dir: Option<PathBuf>,
    /// At startup, replay the local store's trades of the newest N stored sessions into the
    /// volume profile, footprint, cumulative delta, VWAP, DOM volume and time & sales
    /// (0 = off). Default 2: the current and the previous session.
    #[arg(long, value_name = "N", default_value_t = lines_feed::disk::orderflow::DEFAULT_SESSIONS)]
    orderflow_sessions: u32,
    /// Run the feed without a window for N seconds and print stats (useful for recording).
    #[arg(long)]
    headless: Option<u64>,
    /// Color theme (default: Settings › Appearance, else dark). For this run only.
    #[arg(long, value_enum)]
    theme: Option<ThemeKind>,
    /// Up/down color preset (`--palette cb` for the colorblind-safe one; default: Settings ›
    /// Appearance, else standard). Rising candle bodies are hollow in every preset, so
    /// direction never relies on color alone.
    #[arg(long, value_enum)]
    palette: Option<PaletteKind>,
    /// Which direction's candle bodies are hollow, the other filled (default: Settings ›
    /// Appearance, else up).
    #[arg(long, value_enum)]
    hollow: Option<HollowKind>,
    /// A one-tab layout for this run only (the saved workspace is left alone). Screenshots
    /// default to `trading`.
    #[arg(long, value_enum)]
    layout: Option<LayoutKind>,
    /// Open a workspace: a saved one by name, or a built-in (`pair` = NQ and ES side by side,
    /// `trading`, `chart`, `1x2`, `2x2`, `1+2`, `6c2d`). Screenshots take a built-in or a
    /// workspace `.json` file (they never read user settings). Default: the autosaved
    /// workspace (first run: `pair`).
    #[arg(long)]
    workspace: Option<String>,
    /// Benchmark: open the workspace (default `6c2d`: 6 charts + 2 DOMs on two contracts)
    /// in a hidden window for this many seconds while the feeds run, drawing a frame at most
    /// every 8 ms when something changed, then print frames/s, frame-time percentiles, CPU
    /// and memory and exit.
    #[arg(long, conflicts_with = "screenshot")]
    bench: Option<u64>,
    /// Benchmark: hide the workspace's DOMs (they must paint nothing).
    #[arg(long, requires = "bench")]
    bench_hide_doms: bool,
    /// Benchmark: fail (exit 3) if any frame is drawn after the warm-up, i.e. something
    /// repaints without new data or input. Use with a replay that finishes in the warm-up.
    #[arg(long, requires = "bench")]
    bench_expect_idle: bool,
    /// DOM price grouping in ticks per row (G cycles 1, 2, 5, 10, 25 in the app; default:
    /// Settings › DOM, else 1).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    dom_grouping: Option<u32>,
    /// Start DOMs with the price trail on (a DOM's saved setting wins; screenshots and the
    /// benchmark). In the app: the DOM's pane menu or the palette.
    #[arg(long)]
    dom_trail: bool,
    /// Time & sales: highlight prints of at least this size (in the product's units, e.g.
    /// 20 or 0.5). Default: 10 contracts, or 1 unit of a fractional product.
    #[arg(long)]
    large_print: Option<String>,
    /// Window size in logical pixels, WIDTHxHEIGHT (screenshots and the benchmark: default
    /// 1280x800; the app: saved per window, else 85% of the primary display).
    #[arg(long, value_parser = parse_size)]
    size: Option<(f32, f32)>,
    /// Chart volume profile (Alt+V cycles it in the app): the current session, the visible
    /// range of bars, or a composite of the last --profile-sessions sessions.
    #[arg(long, value_enum, default_value_t = ProfileModeArg::Off)]
    profile: ProfileModeArg,
    /// Volume profile session type (Alt+R): full electronic session or regular hours.
    #[arg(long, value_enum, default_value_t = ProfileSessionArg::Eth)]
    profile_session: ProfileSessionArg,
    /// Sessions in a composite volume profile.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u8).range(2..=30))]
    profile_sessions: u8,
    /// Value area, percent of the profile's volume.
    #[arg(long, default_value_t = 70, value_parser = clap::value_parser!(u8).range(1..=100))]
    profile_va: u8,
    /// Split volume profile rows by aggressor side (Alt+B).
    #[arg(long)]
    profile_split: bool,
    /// Volume profile grouping in ticks per row (Alt+G); default: automatic.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    profile_grouping: Option<u32>,
    /// Screenshot mode (decision 0009): replay --replay up to --at at full speed, render one
    /// frame at --size/--theme with a frozen clock, write this PNG, exit.
    /// Requires building with `--features screenshot`.
    #[arg(long, requires = "replay")]
    screenshot: Option<PathBuf>,
    /// Screenshot: required DPI scale. GPUI on Windows cannot override the monitor's DPI,
    /// so the run fails if the actual scale differs (rather than silently differing).
    #[arg(long)]
    scale: Option<f32>,
    /// Screenshot: draw the crosshair as if the pointer were at X,Y (logical pixels from the
    /// top-left corner of the plot area, i.e. below the header).
    #[arg(long, requires = "screenshot", value_parser = parse_point)]
    cursor: Option<(f32, f32)>,
    /// Screenshot: draw the position line held dragged to plot-local Y: the stop or target it
    /// would set, with its P&L (a preview; nothing is placed).
    #[arg(long, requires = "screenshot")]
    drag_position: Option<f32>,
    /// Screenshot: zoom the auto-fitted price range by this factor around the plot's center
    /// (as Ctrl+wheel would), which switches the price axis to manual scale.
    #[arg(long, requires = "screenshot", value_parser = parse_zoom)]
    price_zoom: Option<f64>,
    /// Screenshot: render with the symbol picker open on this query (e.g. "nq").
    #[arg(long, requires = "screenshot")]
    picker: Option<String>,
    /// Screenshot: a scripted paper session on the recording's contract, run at `--at`
    /// before the frame (e.g. "buy 2 mkt; ack; buy 1 @ 5999.50; ack; sell 1 stop 5998.00").
    /// `ack` lets acks arrive; orders after the last `ack` stay pending. See
    /// `trading::parse_script`.
    #[arg(long, requires = "screenshot")]
    paper: Option<String>,
    /// Screenshot: DOMs start with trading enabled ("Trading: Enabled"; in the app the switch
    /// is saved per pane). `--arm` is the old name.
    #[arg(long = "enable-trading", alias = "arm", requires = "screenshot")]
    arm: bool,
    /// Screenshot: DOMs show bracket distances in ticks or points.
    #[arg(long, value_enum, requires = "screenshot")]
    bracket_units: Option<BracketUnitsArg>,
    /// Screenshot: DOMs open their bracket-preset editor.
    #[arg(long, requires = "screenshot")]
    bracket_editor: bool,
    /// Screenshot: draw the focused pane's header controls as if the pointer were over the
    /// pane (no OS pointer involved).
    #[arg(long, requires = "screenshot")]
    hover_header: bool,
    /// Screenshot: also render the Settings window at this section (appearance, charts, dom,
    /// trading, accounts, data, keys, diagnostics) into `<out stem>-settings.png`.
    #[arg(long, requires = "screenshot")]
    settings: Option<String>,
    /// Screenshot: the Settings window's search text.
    #[arg(long, requires = "settings")]
    settings_search: Option<String>,
    /// Screenshot: change settings in the window first, as the trader would (`KEY=VALUE`:
    /// a number, `on`/`off`, or a choice's position from 0). A loosened guardrail shows its
    /// confirmation. Repeatable.
    #[arg(long, requires = "settings")]
    settings_set: Vec<String>,
    /// Screenshot: a synthetic read-only broker (observe mode) with a SIM and a LIVE account;
    /// the panes trade its SIM account, long 2 with a working order on the recording's contract.
    #[arg(long, requires = "screenshot")]
    demo_broker: bool,
    /// Screenshot: a fixed set of synthetic strategy instances (armed, paused, halted at the
    /// daily loss limit, not started) on the paper account and the recording's contract, for
    /// the strategy panel until the strategy host runs in the app.
    #[arg(long, requires = "screenshot")]
    strategies_demo: bool,
    /// Screenshot: how P&L shows (the app's setting, Ctrl+Alt+P cycles it in the app).
    #[arg(long, requires = "screenshot", value_enum)]
    pnl: Option<PnlArg>,
    /// Screenshot: the chart without study legends and tags (Ctrl+Shift+L in the app).
    #[arg(long, requires = "screenshot")]
    hide_study_labels: bool,
    /// Screenshot: the chart with its studies hidden (Ctrl+Shift+H in the app).
    #[arg(long, requires = "screenshot")]
    hide_studies: bool,
    /// Magnet crosshair on the first chart: the price snaps to the hovered bar's
    /// open/high/low/close (Ctrl+Shift+G in the app).
    #[arg(long)]
    magnet: bool,
    /// Screenshot: start the chart with these studies (a `StudySet` JSON file, as a template
    /// or the defaults for new charts hold them).
    #[arg(long, requires = "screenshot")]
    study_set: Option<PathBuf>,
    /// Screenshot: render with the studies menu open: `open`, or `edit:STUDY` for that
    /// study's editor, or `style:STUDY[:TAB]` for the style editor on its parts (tab 0 =
    /// first). STUDY: `vwap:N`, `delta:N`, `profile`, `footprint`, `levels:ID`.
    #[arg(long, requires = "screenshot")]
    studies_menu: Option<String>,
    /// Screenshot: no key-hint bar (F1 in the app; Ctrl+Shift+C = this + no study labels).
    #[arg(long, requires = "screenshot")]
    hide_hints: bool,
    /// External levels (decision 0022): record every level source's responses into this
    /// directory (`<source>-<PRODUCT>.lfr`), so a replay shows the same levels.
    #[arg(long, conflicts_with = "replay_levels")]
    record_levels: Option<PathBuf>,
    /// External levels: replay the level recordings in this directory on the chart's replay
    /// clock instead of fetching (use with --replay).
    #[arg(long)]
    replay_levels: Option<PathBuf>,
    /// Backtest a strategy (decision 0025) over --replay (a .lfr recording or a stored
    /// contract or day) with the levels recorded in --replay-levels, on the paper broker, and
    /// exit. Needs --backtest-config. The public build has only the example, `level-touch`
    /// (synthetic, no edge): it reads the `demo` level source, so it needs a build with
    /// `demo-levels` run with --demo-levels, and a `demo-ES.lfr` levels recording.
    #[arg(long, requires_all = ["replay", "backtest_config"])]
    backtest: Option<String>,
    /// Backtest settings (JSON): {"params": {...}, "limits": {"max_position": 1,
    /// "max_daily_loss": 500, "max_orders_per_min": 10, "hours": {"tz": "America/New_York",
    /// "windows": [{"start": "09:30", "end": "16:00"}]}}}, optionally "starting_balance",
    /// "paper" (fill model, latency, fees) and "timer_ms".
    #[arg(long)]
    backtest_config: Option<PathBuf>,
    /// Run a strategy (decision 0025) through the strategy host on this run's feed (a live
    /// source or a replay) with the paper broker, headless, for --headless seconds; this
    /// command line arms it. Its settings come from --backtest-config (the same file). Paper
    /// only: nothing reaches a live broker.
    #[arg(long, requires_all = ["backtest_config", "headless"], conflicts_with = "backtest")]
    run_strategy: Option<String>,
    /// Run a strategy (decision 0025) in the app, on the paper broker, on this run's feed (the
    /// source's default contract, or --instrument). It starts stopped: Start in the strategy
    /// panel (palette: "Open strategies") arms it; Stop all (Ctrl+Shift+R) stops it and
    /// flattens. Its settings come from --backtest-config. Paper only.
    #[arg(
        long,
        requires = "backtest_config",
        conflicts_with_all = ["backtest", "run_strategy", "headless", "screenshot"]
    )]
    strategy: Option<String>,
    /// Write the backtest's full report here (JSON: inputs with hashes, trades, every decision
    /// with its reason and result); with --run-strategy, the run's fills, final state and
    /// decisions. Without it only the summary is printed.
    #[arg(long)]
    report: Option<PathBuf>,
    /// Show the synthetic demo level source on ES/MES charts (builds with `demo-levels`).
    #[cfg(feature = "demo-levels")]
    #[arg(long)]
    demo_levels: bool,
    /// Add a VWAP to the first chart (repeatable): eth (default reset), rth, week, month or
    /// anchor@2026-11-06T14:30:00Z, optionally with σ bands after a slash: eth/1,2,3 (quote it
    /// in PowerShell). In the app, Ctrl+I opens a chart's indicator menu.
    #[arg(long, value_parser = parse_vwap)]
    vwap: Vec<lines_indicators::VwapSpec>,
    /// The first chart's timeframe: 1m 2m 3m 5m 10m 15m 30m 1h 2h 4h 1d 1w (a bare number is
    /// minutes; bars align to the product's session open), or bars built from counted trades
    /// (reset at each session open): tick `500t` (100t 233t 500t 1000t 2000t, any 1t to
    /// 100000t), volume `500v` (whole contracts, 1v to 1000000v), range `10r` (price ticks, 1r
    /// to 10000r) or seconds `30s` (dividing the day, 1s to 3600s). A timeframe saved with the
    /// chart wins. In the app: type it on a chart (`5`, `1h`, `500t`, `500v`, `10r`, `30s` +
    /// Enter) or use the header chip.
    #[arg(long, value_parser = parse_timeframe)]
    timeframe: Option<lines_core::Timeframe>,
    /// Screenshot: the first chart with its timeframe list open (the header's "5m ▾" chip).
    #[arg(long, requires = "screenshot")]
    timeframe_menu: bool,
    /// Tick, volume, range and seconds bars (`--timeframe 500t`) run across sessions instead of
    /// restarting at each session open. A mode saved with the chart wins. In the app: the palette or the pane
    /// menu ("Trade-built bars run continuously").
    #[arg(long)]
    tick_continuous: bool,
    /// Screenshot: charts and DOMs with their account list open (the header's account chip).
    #[arg(long, requires = "screenshot")]
    account_menu: bool,
    /// Screenshot: draw these drawings (a drawings file, as saved in
    /// `<config dir>/drawings/<PRODUCT>.json`; it names its product). Screenshots never read
    /// the saved drawings.
    #[arg(long, requires = "screenshot")]
    drawings: Option<PathBuf>,
    /// Screenshot: record the frame's layout and write the layout lint's findings beside each
    /// PNG (`<stem>.lint.txt`, empty when clean; `docs/design.md` "Checking a UI change").
    #[arg(long, requires = "screenshot")]
    layout_report: bool,
    /// The UI gallery: render the fixed scene set into DIR from synthetic data, one PNG per
    /// scene, and lint each; exits 1 on any finding (`DIR/gallery.txt`). Screenshot builds
    /// only. The quick set unless `--gallery-full`.
    #[arg(long, conflicts_with = "screenshot")]
    gallery: Option<PathBuf>,
    /// With `--gallery`: every scene (themes, palettes, widths, states, menus).
    #[arg(long, requires = "gallery")]
    gallery_full: bool,
    /// Screenshot: the first chart with the style editor open on drawing ID of --drawings,
    /// optionally on tab TAB (0 = first) with the keyboard focus on control COL of row ROW
    /// (default 0, the first): `--style-editor 3`, `4:1`, `3:0:1` or `3:0:1:4`.
    #[arg(long, requires = "drawings", value_parser = parse_style_editor)]
    style_editor: Option<chart_view::StyleEditorAt>,
}

/// `ID`, `ID:TAB`, `ID:TAB:COL` or `ID:TAB:COL:ROW` (`--style-editor`); the focus is (row, col).
fn parse_style_editor(s: &str) -> Result<chart_view::StyleEditorAt, String> {
    let mut it = s.splitn(4, ':');
    let id = it.next().unwrap_or_default();
    let tab = it.next().unwrap_or("0");
    let id = id.parse().map_err(|_| format!("bad drawing id {id:?}"))?;
    let tab = tab.parse().map_err(|_| format!("bad tab {tab:?}"))?;
    let col = match it.next() {
        Some(c) => Some(c.parse().map_err(|_| format!("bad focus column {c:?}"))?),
        None => None,
    };
    let row = match it.next() {
        Some(r) => r.parse().map_err(|_| format!("bad focus row {r:?}"))?,
        None => 0,
    };
    Ok((id, tab, col.map(|c| (row, c))))
}

/// The CLI with every registered source as a `--source` value, with its help.
fn command(ext: &Extensions) -> clap::Command {
    let mut values: Vec<PossibleValue> = SourceKind::value_variants()
        .iter()
        .filter_map(|v| v.to_possible_value())
        .collect();
    values.extend(
        ext.sources
            .iter()
            .map(|e| PossibleValue::new(e.name()).help(e.help())),
    );
    let mut cmd = Cli::command().mut_arg("source", |a| {
        a.value_parser(clap::builder::PossibleValuesParser::new(values))
    });
    let extra: Vec<&str> = ext.sources.iter().filter_map(|e| e.long_help()).collect();
    if !extra.is_empty() {
        cmd = cmd.after_long_help(extra.join("\n\n"));
    }
    cmd
}

fn parse_cli<I, T>(args: I, ext: &Extensions) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut matches = command(ext).try_get_matches_from(args)?;
    Cli::from_arg_matches_mut(&mut matches)
}

fn parse_vwap(s: &str) -> Result<lines_indicators::VwapSpec, String> {
    use lines_indicators::{Bands, Reset, VwapSpec};
    let (reset, bands) = match s.split_once('/') {
        Some((r, b)) => (r, Some(b)),
        None => (s, None),
    };
    let reset = match reset.to_ascii_lowercase().as_str() {
        "eth" => Reset::Eth,
        "rth" => Reset::Rth,
        "week" | "weekly" => Reset::Weekly,
        "month" | "monthly" => Reset::Monthly,
        r => match r.strip_prefix("anchor@") {
            Some(t) => Reset::Anchored {
                from: parse_time(&t.to_ascii_uppercase())?.0,
            },
            None => {
                return Err(format!(
                    "unknown VWAP reset {r:?} (eth, rth, week, month, anchor@TIME)"
                ));
            }
        },
    };
    let mut mask = 0u8;
    for k in bands
        .into_iter()
        .flat_map(|b| b.split(','))
        .filter(|k| !k.is_empty())
    {
        match k.trim() {
            "1" => mask |= 1,
            "2" => mask |= 2,
            "3" => mask |= 4,
            k => return Err(format!("bands are 1, 2 or 3, not {k:?}")),
        }
    }
    Ok(VwapSpec {
        reset,
        bands: Bands(mask),
    })
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ProfileModeArg {
    Off,
    Session,
    Visible,
    Composite,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ProfileSessionArg {
    Eth,
    Rth,
}

fn parse_time(s: &str) -> Result<UtcMillis, String> {
    UtcMillis::parse_iso(s).ok_or_else(|| {
        format!("expected an ISO-8601 UTC time like 2026-11-06T14:05:00Z, got {s:?}")
    })
}

fn parse_size(s: &str) -> Result<(f32, f32), String> {
    let (w, h) = s.split_once(['x', 'X']).ok_or("expected WIDTHxHEIGHT")?;
    let (w, h): (f32, f32) = (
        w.parse().map_err(|_| "bad width")?,
        h.parse().map_err(|_| "bad height")?,
    );
    if !(200.0..=8192.0).contains(&w) || !(150.0..=8192.0).contains(&h) {
        return Err("size out of range".into());
    }
    Ok((w, h))
}

fn parse_timeframe(s: &str) -> Result<lines_core::Timeframe, String> {
    lines_core::Timeframe::parse(s).ok_or_else(|| {
        "expected 1m 2m 3m 5m 10m 15m 30m 1h 2h 4h 1d or 1w, tick bars 500t (1t to 100000t), \
             volume bars 500v (1v to 1000000v), range bars 10r (1r to 10000r) or seconds bars \
             30s (1s to 3600s, dividing the day)"
            .into()
    })
}

fn parse_zoom(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() && f > 0.0 => Ok(f),
        _ => Err("expected a positive factor, e.g. 1.5 (>1 zooms in)".into()),
    }
}

fn parse_point(s: &str) -> Result<(f32, f32), String> {
    let (x, y) = s.split_once(',').ok_or("expected X,Y")?;
    let (x, y): (f32, f32) = (
        x.trim().parse().map_err(|_| "bad x")?,
        y.trim().parse().map_err(|_| "bad y")?,
    );
    if !(x.is_finite() && y.is_finite()) {
        return Err("X,Y must be finite".into());
    }
    Ok((x, y))
}

fn feed_config(cli: &Cli, choice: &SourceChoice) -> Result<FeedConfig> {
    let kind = match choice {
        SourceChoice::Builtin(k) => Some(*k),
        SourceChoice::Extension(_) => None,
    };
    let source = match kind {
        _ if cli.screenshot.is_some() => {
            // Screenshots are always a full-speed replay to a fixed point.
            let path = cli
                .replay
                .clone()
                .context("--screenshot needs --replay <FILE>")?;
            Source::Replay {
                path,
                speed: 0.0,
                stop_at: cli.at,
            }
        }
        None => match choice {
            SourceChoice::Extension(e) => Source::Extension(e.clone()),
            SourceChoice::Builtin(_) => unreachable!("built-ins are matched below"),
        },
        Some(SourceKind::Mock) => Source::CqgMock(MockConfig {
            quotes_per_sec: cli.rate,
            quotes_per_frame: cli.batch.max(1),
            disconnect_after: cli.disconnect_every.map(Duration::from_secs),
            ..Default::default()
        }),
        Some(SourceKind::Replay) => {
            let Some(path) = cli.replay.clone() else {
                bail!("--source replay needs --replay <FILE>")
            };
            Source::Replay {
                path,
                speed: cli.speed,
                stop_at: cli.at,
            }
        }
        Some(SourceKind::Kraken) => Source::Kraken {
            spot_url: lines_feed::kraken::SPOT_WS_URL.into(),
            futures_url: lines_feed::kraken::FUTURES_WS_URL.into(),
        },
        Some(SourceKind::Coinbase) => Source::Coinbase {
            url: lines_feed::coinbase::WS_URL.into(),
        },
        #[cfg(feature = "vendor-cqg")]
        Some(SourceKind::Cqg) => {
            let user = std::env::var("LINES_CQG_USER").ok();
            let password = std::env::var("LINES_CQG_PASSWORD").ok();
            if user.is_none() || password.is_none() {
                bail!("set LINES_CQG_USER and LINES_CQG_PASSWORD for --source cqg");
            }
            Source::Cqg {
                url: cli.cqg_url.clone(),
                logon: lines_feed::cqg::LogonParams {
                    user_name: user,
                    password,
                    private_label: Some(cli.private_label.clone()),
                    client_app_id: Some(cli.client_app_id.clone()),
                    client_version: lines_feed::client_version(),
                },
            }
        }
    };
    let mut cfg = FeedConfig::new(source);
    if let Some(sym) = &cli.instrument {
        cfg.target = Some(catalog::parse_target(sym).with_context(|| {
            let known: Vec<_> = catalog::all().iter().map(|p| p.symbol).collect();
            format!(
                "unknown instrument {sym:?}; use a product ({}) or a contract like ESZ26",
                known.join(", ")
            )
        })?);
    }
    cfg.record = cli.record.clone();
    cfg.store = store_config(cli, choice);
    cfg.orderflow_sessions = cli.orderflow_sessions;
    Ok(cfg)
}

/// The local store (decision 0019) for live sources: the mock invents a new market every
/// run and replays and screenshots must not write, so those never store, unless `--store-dir`
/// names a store for this run.
fn store_config(cli: &Cli, choice: &SourceChoice) -> Option<lines_feed::disk::StoreConfig> {
    if cli.no_store {
        return None;
    }
    let forced = cli.store_dir.is_some();
    let source = match choice {
        _ if cli.screenshot.is_some() && !forced => return None,
        // A replay (screenshots replay too, whatever `--source` says) keeps its own store.
        _ if forced && cli.replay.is_some() => "replay",
        SourceChoice::Extension(e) if e.local_store() || forced => e.name(),
        SourceChoice::Builtin(SourceKind::Coinbase) => "coinbase",
        SourceChoice::Builtin(SourceKind::Kraken) => "kraken",
        #[cfg(feature = "vendor-cqg")]
        SourceChoice::Builtin(SourceKind::Cqg) => "cqg",
        SourceChoice::Builtin(SourceKind::Mock) if forced => "mock",
        SourceChoice::Builtin(SourceKind::Replay) if forced => "replay",
        SourceChoice::Extension(_)
        | SourceChoice::Builtin(SourceKind::Mock | SourceKind::Replay) => return None,
    };
    let root = match &cli.store_dir {
        Some(dir) => dir.clone(),
        None => match lines_feed::disk::default_root() {
            Some(r) => r,
            None => {
                tracing::warn!(
                    "no data directory (set LINES_DATA_DIR); running without the local store"
                );
                return None;
            }
        },
    };
    let mut cfg = lines_feed::disk::StoreConfig::new(root, source);
    cfg.depth_products = cli.store_depth.clone();
    cfg.budget_bytes = (cli.store_budget_gb.max(0.0) * (1u64 << 30) as f64) as u64;
    tracing::info!(
        "local store: {} (budget {:.1} GiB{})",
        cfg.root.display(),
        cli.store_budget_gb,
        if cfg.depth_products.is_empty() {
            String::new()
        } else {
            format!(", depth for {}", cfg.depth_products.join(","))
        }
    );
    Some(cfg)
}

/// The level source the public example strategy reads (the synthetic `demo` source, which
/// builds with `demo-levels` register).
const EXAMPLE_LEVELS: &str = "demo";

/// A registered strategy by name (the public build adds the example).
fn find_strategy(
    ext: &Extensions,
    name: &str,
) -> Result<Arc<dyn lines_strategy::StrategyExtension>> {
    let example: Arc<dyn lines_strategy::StrategyExtension> =
        Arc::new(lines_strategy::example::LevelTouchExt::new(EXAMPLE_LEVELS));
    let mut all: Vec<Arc<dyn lines_strategy::StrategyExtension>> = ext.strategies.clone();
    if !all.iter().any(|s| s.name() == example.name()) {
        all.push(example);
    }
    all.iter()
        .find(|s| s.name() == name)
        .cloned()
        .ok_or_else(|| {
            let names: Vec<&str> = all.iter().map(|s| s.name()).collect();
            anyhow!(
                "no strategy {name:?} in this build (available: {})",
                names.join(", ")
            )
        })
}

/// The strategy settings file (`--backtest-config`).
fn strategy_settings(cli: &Cli) -> Result<lines_strategy::backtest::Settings> {
    let path = cli
        .backtest_config
        .as_ref()
        .ok_or_else(|| anyhow!("--backtest-config is required"))?;
    serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("--backtest-config {}", path.display()))?,
    )
    .with_context(|| format!("--backtest-config {}", path.display()))
}

/// How long a strategy's feed may be silent before the host pauses it.
const STRATEGY_FEED_STALE: Duration = Duration::from_secs(60);

/// `--run-strategy`: one strategy through the strategy host on this run's feed, paper only,
/// headless (decision 0025). Prints its state, decisions and fills as they happen.
fn run_strategy(
    cli: &Cli,
    ext: &Extensions,
    name: &str,
    config: FeedConfig,
    replay: bool,
    dur: Duration,
) -> Result<()> {
    use lines_trading::{
        AccountConfig, AccountId, Clock, Command as TradeCommand, Event, PaperBroker,
        StrategyConfig, StrategyId, TradingCore, TradingService,
    };
    let strategy = find_strategy(ext, name)?;
    let settings = strategy_settings(cli)?;
    let feed = lines_feed::spawn(config.clone())?;
    let store = feed.store.clone();
    // Listen, and attach the paper core, as soon as the feed exists (before waiting for its
    // contract): a fast replay delivers a lot in moments, and the strategy and the paper
    // broker must see the same trades.
    let market = lines_feed::listen::listen(store.feed_id());
    let clock = if replay { Clock::Market } else { Clock::Wall };
    let account = AccountId::new("paper-strategy");
    let mut broker = PaperBroker::new(settings.paper.clone());
    broker.open_account(
        account.clone(),
        lines_trading::Money::whole(settings.starting_balance),
    );
    let mut core = TradingCore::new(Box::new(broker));
    core.add_account(
        AccountConfig::paper("paper-strategy", settings.starting_balance),
        None,
    );
    let service = TradingService::start_with(core, None, clock)?;
    let handle = service.handle();
    let events = handle.subscribe();
    let bridge = lines_trading::FeedBridge::new(lines_feed::registry::FeedRegistry::new(), config);
    bridge.follow_store(&store, &handle);
    // The contract comes from the feed (a recording's header, a source's front month).
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let (product, contract) = loop {
        {
            let d = store.read();
            if let Some(k) = d.contract {
                break (d.product.clone(), k);
            }
        }
        if std::time::Instant::now() > deadline {
            bail!("the feed never said which contract it carries");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut levels: Vec<Box<dyn lines_strategy::host::LevelFeed>> = Vec::new();
    // The same level tasks, to arm only once every source is current.
    let mut watch: Vec<lines_feed::levels::runtime::LevelHandle> = Vec::new();
    for id in strategy.level_sources() {
        let source = lines_feed::levels::by_id(id)
            .ok_or_else(|| anyhow!("level source {id:?} is not registered in this build"))?;
        let ctx = lines_feed::levels::LevelContext {
            product: product.clone(),
            contract: Some(contract),
            settings: lines_feed::levels::runtime::settings_for(id),
        };
        let prices = Arc::new(lines_feed::levels::StorePrices(store.clone()));
        let handle = lines_feed::levels::runtime::acquire(source, ctx, prices);
        watch.push(handle.clone());
        levels.push(Box::new(handle));
    }
    let host = lines_strategy::host::StrategyHost::start(handle.clone(), clock);
    let id = StrategyId(1);
    host.add(lines_strategy::host::Instance {
        ext: strategy.clone(),
        params: settings.params.clone(),
        config: StrategyConfig {
            id,
            label: format!("{} #1", strategy.label()),
            account,
            contract,
            limits: settings.limits.clone(),
        },
        product: product.clone(),
        market,
        levels,
        stale_after: STRATEGY_FEED_STALE,
    })
    .map_err(|e| anyhow!("{}: {e}", strategy.label()))?;
    handle.sync();
    host.sync();
    println!(
        "running {} v{} on {} (paper) for {} s",
        strategy.name(),
        strategy.version(),
        contract.label(&product).as_str(),
        dur.as_secs()
    );
    let start = std::time::Instant::now();
    let mut printed = 0u64;
    let mut state = None;
    let mut fault: Option<String> = None;
    let mut net = lines_trading::Money::ZERO;
    let mut fills: Vec<serde_json::Value> = Vec::new();
    let mut armed = false;
    while start.elapsed() < dur {
        std::thread::sleep(Duration::from_millis(100));
        // This command line is the person arming it (decision 0025: nothing else arms), once
        // every level source is current on the feed's own clock (a replay's levels arrive
        // with the replay).
        if !armed {
            let now = if replay {
                store.read().market_time
            } else {
                Some(lines_feed::now_utc())
            };
            let current = now.is_some_and(|now| {
                watch.iter().all(|h| {
                    matches!(
                        h.cell().read().status(now),
                        lines_feed::levels::runtime::LevelStatus::Current
                    )
                })
            });
            if current {
                armed = true;
                handle.submit(
                    lines_trading::Source::Palette,
                    TradeCommand::ArmStrategy { id },
                );
            }
        }
        for e in events.try_iter() {
            if let Event::Fill(f) = &e
                && f.contract == contract
            {
                net += f.realised - f.fee;
                fills.push(serde_json::json!({
                    "side": format!("{:?}", f.side),
                    "qty": f.qty.0,
                    "price": f.price.0,
                    "at": f.at.0,
                }));
                println!(
                    "fill {:?} {} @ {}",
                    f.side,
                    f.qty.0,
                    product.price.format::<32>(f.price)
                );
            }
        }
        if let Some(v) = host.snapshot().into_iter().find(|v| v.id == id) {
            if state != Some((v.state, v.reason.clone())) {
                println!(
                    "state: {}{}",
                    v.state.label(),
                    v.reason
                        .as_deref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                );
                state = Some((v.state, v.reason.clone()));
            }
            if v.fault != fault {
                if let Some(f) = &v.fault {
                    println!("fault: {f}");
                }
                fault = v.fault.clone();
            }
            // The decisions not printed yet (the ring keeps only the newest).
            let new = (v.decided - printed).min(v.decisions.len() as u64) as usize;
            for d in &v.decisions[v.decisions.len() - new..] {
                let result = match &d.result {
                    Some(Ok(_)) => " [sent]".to_string(),
                    Some(Err(r)) => format!(" [denied: {}]", r.message),
                    None if d.decision.command.is_some() => " [pending]".into(),
                    None => String::new(),
                };
                println!(
                    "{} {}: {}{result}",
                    d.at, d.decision.rule, d.decision.reason
                );
            }
            printed = v.decided;
        }
    }
    println!("{} fills, realized net {net}", fills.len());
    if !armed {
        println!("never armed: the level sources were not all current");
    }
    if let Some(out) = &cli.report {
        let v = host.snapshot().into_iter().find(|v| v.id == id);
        let report = serde_json::json!({
            "strategy": strategy.name(),
            "version": strategy.version(),
            "contract": contract.label(&product).as_str(),
            "armed": armed,
            "state": v.as_ref().map(|v| v.state.label()),
            "reason": v.as_ref().and_then(|v| v.reason.clone()),
            "fault": v.as_ref().and_then(|v| v.fault.clone()),
            "fills": fills,
            "net": net.to_string(),
            "decisions": v.map(|v| v.decisions).unwrap_or_default(),
        });
        std::fs::write(out, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("--report {}", out.display()))?;
    }
    drop(host);
    drop(feed);
    Ok(())
}

/// `--backtest`: one strategy over a replay, headless (decision 0025). Prints the summary;
/// writes the report with `--report`.
fn run_backtest(cli: &Cli, ext: &Extensions, name: &str) -> Result<()> {
    let strategy = find_strategy(ext, name)?;
    let settings = strategy_settings(cli)?;
    let market = cli.replay.clone().expect("clap requires it");
    let bt = lines_strategy::backtest::Backtest::from_settings(
        strategy,
        settings,
        market,
        cli.replay_levels.clone(),
    );
    let report = lines_strategy::backtest::run(&bt)?;
    println!(
        "backtest {} v{} on {}: {}",
        report.strategy,
        report.version,
        report.contract,
        report.summary.line()
    );
    for w in &report.warnings {
        println!("warning: {w}");
    }
    if let Some(open) = &report.open_at_end {
        println!(
            "open at the end: {} (unrealized {})",
            open.qty, open.unrealised
        );
    }
    if let Some(out) = &cli.report {
        std::fs::write(out, report.to_json())
            .with_context(|| format!("--report {}", out.display()))?;
        println!("report: {}", out.display());
    }
    Ok(())
}

/// Run Lines with the process's command line. The public `lines` binary passes
/// `Extensions::new()`; official builds pass their own sources (decision 0020).
pub fn run(ext: Extensions) -> Result<()> {
    run_with_args(std::env::args_os(), ext)
}

/// [`run`] with an explicit command line (`args[0]` is the program name).
pub fn run_with_args<I, T>(args: I, ext: Extensions) -> Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    // --help/--version print and exit 0; usage errors print and exit 2, as before.
    let cli = parse_cli(args, &ext).unwrap_or_else(|e| e.exit());
    if let Some(dir) = &cli.gallery {
        #[cfg(feature = "screenshot")]
        {
            gallery::check_dir(dir)?;
            std::process::exit(gallery::run(dir, cli.gallery_full)?);
        }
        #[cfg(not(feature = "screenshot"))]
        {
            let _ = (dir, cli.gallery_full);
            bail!("this build has no screenshot support; rebuild with `--features screenshot`");
        }
    }
    // Logs (roadmap U1): stderr always; rotating files only for the interactive app, never
    // for headless, screenshot or benchmark runs (tests run those). A second call in one
    // process (tests) keeps the first setup.
    let interactive = cli.headless.is_none()
        && cli.screenshot.is_none()
        && cli.bench.is_none()
        && cli.backtest.is_none();
    logs::init(interactive.then(logs::LogConfig::from_env).flatten());
    for e in &ext.sources {
        lines_feed::extension::register(e.clone())?;
    }
    for l in &ext.level_sources {
        lines_feed::levels::register(l.clone())?;
    }
    ext.validate()?;
    for b in &ext.brokers {
        // Counts only: account ids never go to logs.
        let s = lines_trading::extension::summarize(&b.accounts().accounts());
        tracing::info!(
            "broker {:?} ({}): {} simulated, {} live, {} unknown accounts; {} tradable",
            b.name(),
            b.label(),
            s.simulated,
            s.live,
            s.unknown,
            s.tradable
        );
    }

    let choice = SourceChoice::parse(&cli.source, &ext)
        .ok_or_else(|| anyhow!("unknown source {:?}", cli.source))?;
    let config = feed_config(&cli, &choice)?;
    // A market replay never runs a level source live (only its recorded batches, if any).
    let market_replay = matches!(config.source, lines_feed::Source::Replay { .. });
    lines_feed::levels::runtime::set_mode(lines_feed::levels::runtime::mode_for(
        market_replay,
        cli.record_levels.clone(),
        cli.replay_levels.clone(),
    ));
    #[cfg(feature = "demo-levels")]
    if cli.demo_levels {
        lines_feed::levels::register(Arc::new(lines_feed::levels::demo::DemoLevels))?;
    }

    if cli.backtest_config.is_some()
        && cli.backtest.is_none()
        && cli.run_strategy.is_none()
        && cli.strategy.is_none()
    {
        bail!("--backtest-config goes with --backtest, --run-strategy or --strategy");
    }
    if cli.report.is_some() && cli.backtest.is_none() && cli.run_strategy.is_none() {
        bail!("--report goes with --backtest or --run-strategy");
    }
    if let Some(name) = &cli.backtest {
        return run_backtest(&cli, &ext, name);
    }
    if let (Some(name), Some(secs)) = (&cli.run_strategy, cli.headless) {
        let replay = matches!(config.source, lines_feed::Source::Replay { .. });
        return run_strategy(&cli, &ext, name, config, replay, Duration::from_secs(secs));
    }
    if let Some(secs) = cli.headless {
        return run_headless(config, Duration::from_secs(secs));
    }
    // This run's look flags: over Settings › Appearance (the app reads it below), never saved.
    let look_flags = {
        use settings_schema::{HollowName, PaletteName, ThemeName};
        settings_schema::Appearance {
            theme: cli.theme.map(|t| match t {
                ThemeKind::Dark => ThemeName::Dark,
                ThemeKind::Light => ThemeName::Light,
            }),
            palette: cli.palette.map(|p| match p {
                PaletteKind::Standard => PaletteName::Standard,
                PaletteKind::Colorblind => PaletteName::Colorblind,
            }),
            hollow: cli.hollow.map(|h| match h {
                HollowKind::Up => HollowName::Up,
                HollowKind::Down => HollowName::Down,
            }),
        }
    };
    let theme =
        settings_schema::theme_for(&settings_schema::Appearance::default().with_flags(look_flags));

    // The source's (or `--instrument`'s) product sizes `--large-print`.
    let large_print = match &cli.large_print {
        Some(s) => {
            let product = lines_feed::new_store(&config)?.read().product.clone();
            Some(product.qty.parse_decimal(s).map_err(|e| {
                anyhow!("--large-print {s:?} is not a {} size: {e}", product.symbol)
            })?)
        }
        None => None,
    };
    let frozen = cli.screenshot.is_some();
    let bench = cli.bench.is_some();
    // Panes choose their own targets; the run fixes only the source.
    let mut base = config.clone();
    let instrument = base.target.take().map(|t| TargetDoc::from_target(&t));
    let record = base.record.take();
    // A replay shows its one contract in panes it can't otherwise serve, for this run only;
    // not when --instrument says what every pane shows.
    let fallback = feeds::Fallback::for_source(&base, instrument.is_none());
    let env = PaneEnv {
        fallback,
        availability: lines_feed::availability::availability(&base.source)?,
        base,
        theme: std::cell::Cell::new(theme),
        look_flags: std::cell::Cell::new(look_flags),
        frozen,
        track_visibility: !frozen && !bench,
        dom: DomOptions {
            grouping: cli.dom_grouping.unwrap_or(1),
            trail: cli.dom_trail,
            grouping_from_cli: cli.dom_grouping.is_some(),
            trail_from_cli: cli.dom_trail,
        },
        tape: TapeOptions { large_print },
        record: std::cell::RefCell::new(record),
        fixture: std::cell::RefCell::new(Some(chart_view::Fixture {
            studies: match &cli.study_set {
                Some(p) => Some(
                    serde_json::from_slice(
                        &std::fs::read(p)
                            .with_context(|| format!("--study-set {}", p.display()))?,
                    )
                    .with_context(|| format!("--study-set {}: not a study set", p.display()))?,
                ),
                None => None,
            },
            studies_menu: cli.studies_menu.clone(),
            magnet: cli.magnet,
            timeframe: cli.timeframe.unwrap_or_default(),
            timeframe_menu: cli.timeframe_menu,
            style_editor: cli.style_editor,
            tick_continuous: cli.tick_continuous,
            cursor: cli.cursor,
            drag_position: cli.drag_position,
            price_zoom: cli.price_zoom,
            picker: cli.picker.clone(),
            indicators: cli
                .vwap
                .iter()
                .enumerate()
                .map(|(i, spec)| lines_indicators::IndicatorConfig::Vwap {
                    spec: *spec,
                    color: (i % 4) as u8,
                    display: Default::default(),
                    style: Default::default(),
                })
                .collect(),
            profile: ProfileConfig {
                mode: match cli.profile {
                    ProfileModeArg::Off => ProfileMode::Off,
                    ProfileModeArg::Session => ProfileMode::Session,
                    ProfileModeArg::Visible => ProfileMode::VisibleRange,
                    ProfileModeArg::Composite => ProfileMode::Composite,
                },
                session: match cli.profile_session {
                    ProfileSessionArg::Eth => ProfileSession::Eth,
                    ProfileSessionArg::Rth => ProfileSession::Rth,
                },
                composite_sessions: cli.profile_sessions,
                value_area_percent: cli.profile_va,
                split: cli.profile_split,
                grouping: cli.profile_grouping,
                ..ProfileConfig::default()
            },
            visibility: wsdoc::StudyVisibility {
                hide_study_labels: cli.hide_study_labels,
                hide_studies: cli.hide_studies,
            },
        })),
        default_size: cli.size,
    };
    let registry = FeedRegistry::new();

    if frozen || bench {
        // Built-ins or explicit files only: these modes never read user settings.
        let mut doc = match (&cli.workspace, cli.layout) {
            (Some(w), _) => fixed_workspace(w)?,
            (None, Some(l)) => preset_doc(l),
            (None, None) if bench => WorkspaceDoc::bench(),
            (None, None) => preset_doc(LayoutKind::Trading),
        };
        if instrument.is_some() {
            doc.retarget_all(instrument.clone());
        }
        doc.hide_hints |= cli.hide_hints;
        doc.normalise().map_err(|e| anyhow!("--workspace: {e}"))?;
        let size = cli.size.unwrap_or((1280.0, 800.0));
        if let Some(secs) = cli.bench {
            let code = bench::run(
                registry,
                Workspace::new(env),
                doc,
                size,
                secs,
                cli.bench_hide_doms,
                cli.bench_expect_idle,
            )?;
            std::process::exit(code);
        }
        let out = cli.screenshot.clone().expect("screenshot mode");
        #[cfg(feature = "screenshot")]
        {
            let drawings = match &cli.drawings {
                Some(f) => {
                    Some(drawings_store::read_seed(f).map_err(|e| anyhow!("--drawings: {e}"))?)
                }
                None => None,
            };
            let shot = screenshot::Shot {
                out,
                size,
                scale: cli.scale,
                doc,
                paper: cli.paper.clone(),
                arm: cli.arm,
                demo_broker: cli.demo_broker,
                strategies_demo: cli.strategies_demo,
                account_menu: cli.account_menu,
                pnl: cli.pnl.map(PnlArg::mode),
                drawings,
                bracket_units: cli.bracket_units.map(BracketUnitsArg::units),
                bracket_editor: cli.bracket_editor,
                hover_header: cli.hover_header,
                layout_report: cli.layout_report,
                settings: cli
                    .settings
                    .clone()
                    .map(|section| screenshot::SettingsShot {
                        section,
                        search: cli.settings_search.clone(),
                        set: cli.settings_set.clone(),
                    }),
            };
            let code = screenshot::run(registry, Workspace::new(env), shot)?;
            std::process::exit(code);
        }
        #[cfg(not(feature = "screenshot"))]
        {
            let _ = (out, doc, size);
            bail!(
                "this build has no screenshot support; rebuild with `cargo run -p lines-app --features screenshot`"
            );
        }
    }

    // The app: restore the autosaved workspace (or open the one asked for).
    let dir = persist::config_dir();
    let writer = persist::Writer::spawn();
    let (settings, settings_report) = AppSettings::load(
        dir.as_ref().map(|d| d.join("settings.json")),
        Some(writer.clone()),
    );
    let mut reports: Vec<String> = settings_report.into_iter().collect();
    // `--instrument` and `--layout` are for this run only: don't save over the workspace.
    let ad_hoc = instrument.is_some() || cli.layout.is_some();
    let mut autosave = !ad_hoc && dir.is_some();
    let mut doc = match (&cli.workspace, cli.layout) {
        (Some(name), _) => match &dir {
            Some(d) if persist::named_path(d, name).exists() => {
                let l = versioned::load::<WorkspaceDoc>(&wsdoc::FORMAT, &persist::named_path(d, name));
                reports.extend(l.report);
                l.value
                    .ok_or_else(|| anyhow!("workspace {name:?} couldn't be read"))?
            }
            _ => wsdoc::builtin(name).ok_or_else(|| {
                anyhow!("no workspace named {name:?} (built-ins: pair, trading, chart, 1x2, 2x2, 1+2, 6c2d)")
            })?,
        },
        (None, Some(l)) => preset_doc(l),
        (None, None) => match &dir {
            Some(d) => {
                let l = versioned::load::<WorkspaceDoc>(&wsdoc::FORMAT, &persist::workspace_path(d));
                reports.extend(l.report);
                if !l.writable {
                    autosave = false;
                }
                l.value.unwrap_or_else(WorkspaceDoc::pair)
            }
            None => WorkspaceDoc::pair(),
        },
    };
    if let Err(e) = doc.normalise() {
        reports.push(format!("saved workspace: {e}; loaded the default"));
        doc = WorkspaceDoc::pair();
    }
    if instrument.is_some() {
        doc.retarget_all(instrument);
    }
    let mut ws = Workspace::new(env);
    // Settings › Appearance, under any look flag given.
    ws.env.theme.set(settings_schema::theme_for(
        &settings.settings.appearance.with_flags(look_flags),
    ));
    // The paper trading core (roadmap 7): its own thread, state per data source.
    let mut setup = trading::TradingSetup::for_live(dir.as_deref(), &config.source);
    // The trader's guardrails (roadmap 8b) from settings.json.
    setup.guardrails = settings.settings.guardrails.clone();
    // Prop rules from settings; one that doesn't resolve (an unknown preset) is reported and
    // left out (the account keeps its rules).
    for (account, doc) in &settings.settings.prop_rules {
        match doc.resolve() {
            Ok(r) => {
                setup.prop_rules.insert(account.clone(), r);
            }
            Err(e) => reports.push(format!("settings: prop rules for {account}: {e}")),
        }
    }
    // Broker extensions (decision 0020): each runs beside the paper core.
    setup.brokers = ext.brokers().to_vec();
    let trading = trading::Trading::start(setup, registry.clone(), config.clone());
    // `--strategy`: the strategy host on the paper core (decision 0025).
    let strategies = match &cli.strategy {
        Some(name) => {
            let strategy = find_strategy(&ext, name)?;
            let settings = strategy_settings(&cli)?;
            Some(strategy_host::start(
                strategy,
                settings,
                &registry,
                config.clone(),
                &trading,
            )?)
        }
        None => None,
    };
    // Kept to flush the journal's last records on quit.
    let journal = trading.journal.clone();
    if let Some(r) = &trading.report {
        reports.push(r.clone());
    }
    if let Some(d) = dir.clone() {
        ws = ws.with_files(d, writer.clone(), autosave);
    }
    for r in reports {
        ws.add_report(r);
    }
    if ad_hoc {
        tracing::info!("--instrument/--layout: this run's layout is not autosaved");
    }

    let drawings = drawings_store::Drawings::new(dir.clone(), Some(writer.clone()));
    gpui_platform::application().run(move |cx: &mut App| {
        cx.set_global(Feeds(registry));
        cx.set_global(trading);
        if let Some(s) = strategies {
            cx.set_global(strategy_panel::Strategies(Arc::new(s)));
        }
        cx.set_global(settings);
        cx.set_global(drawings);
        cx.set_global(ws);
        workspace::install(cx);
        if let Err(e) = workspace::open_workspace(&doc, cx) {
            tracing::error!("failed to open the workspace: {e:#}");
            cx.quit();
            return;
        }
        workspace::start_autosave(cx);
        diagnostics::start_sampler(cx);
        cx.activate(true);
    });
    writer.flush(Duration::from_secs(3));
    journal.flush(Duration::from_secs(3));
    Ok(())
}

/// A one-tab workspace from `--layout`, on the source's default (or `--instrument`'s) target.
fn preset_doc(l: LayoutKind) -> WorkspaceDoc {
    let p = match l {
        LayoutKind::Trading => Preset::Trading,
        LayoutKind::Chart => Preset::Chart,
    };
    wsdoc::builtin(p.name()).expect("presets are built in")
}

/// Screenshot/benchmark workspaces: a built-in, or a workspace file given by path.
fn fixed_workspace(w: &str) -> Result<WorkspaceDoc> {
    if let Some(doc) = wsdoc::builtin(w) {
        return Ok(doc);
    }
    let path = std::path::Path::new(w);
    let bytes = std::fs::read(path)
        .with_context(|| format!("--workspace {w:?} is neither a built-in nor a readable file"))?;
    match versioned::decode::<WorkspaceDoc>(&wsdoc::FORMAT, &bytes) {
        versioned::Decoded::Ok { value, .. } => Ok(value),
        versioned::Decoded::Newer { found } => {
            bail!("{w} is workspace version {found}, newer than this build reads")
        }
        versioned::Decoded::Corrupt { reason } => bail!("{w}: {reason}"),
    }
}

/// Runtime window icon from `assets/icon/png/lines-256.png`. GPUI applies it on X11 only; on
/// Windows the window/taskbar icon comes from the executable's icon resource 1 (`build.rs`),
/// and on macOS from the app bundle.
pub(crate) fn window_icon() -> Option<std::sync::Arc<image::RgbaImage>> {
    const PNG: &[u8] = include_bytes!("../../../assets/icon/png/lines-256.png");
    match image::load_from_memory_with_format(PNG, image::ImageFormat::Png) {
        Ok(img) => Some(std::sync::Arc::new(img.into_rgba8())),
        Err(e) => {
            tracing::warn!("window icon not decoded: {e}");
            None
        }
    }
}

fn run_headless(config: FeedConfig, dur: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    let mut feed = lines_feed::spawn(config)?;
    // Startup timing (roadmap 5a): when the first bars and the newest bar were in the store
    // (a pane paints them within one poll tick, 8 ms), and when history was complete.
    let (mut first_bars, mut recent, mut complete) = (None, None, None);
    let started_wall = lines_feed::now_utc();
    let mut next_print = Duration::from_secs(1);
    while start.elapsed() < dur {
        std::thread::sleep(Duration::from_millis(2));
        {
            let d = feed.store.read();
            if first_bars.is_none() && !d.series.is_empty() {
                first_bars = Some(start.elapsed());
                println!(
                    "first bars in the store after {:.1} ms ({} bars, newest {})",
                    start.elapsed().as_secs_f64() * 1e3,
                    d.series.len(),
                    d.series
                        .last()
                        .map(|b| b.time)
                        .unwrap_or(lines_core::UtcMillis(0))
                );
            }
            // The newest minutes are on screen (what a trader looks at first).
            if recent.is_none()
                && d.series
                    .last()
                    .is_some_and(|b| b.time.0 >= started_wall.0 - 3 * 60_000)
            {
                recent = Some(start.elapsed());
                println!(
                    "bars up to the last 3 minutes after {:.1} ms",
                    start.elapsed().as_secs_f64() * 1e3
                );
            }
            if complete.is_none() && d.history == lines_feed::store::HistoryState::Complete {
                complete = Some(start.elapsed());
                println!(
                    "history complete after {:.1} ms ({} bars)",
                    start.elapsed().as_secs_f64() * 1e3,
                    d.series.len()
                );
            }
        }
        if start.elapsed() < next_print {
            continue;
        }
        next_print += Duration::from_secs(1);
        let d = feed.store.read();
        println!(
            "[{:>4.1}s] {} {:<16} bars={:<6} updates={:<9} trades={:<8} frames={:<7} gaps={} rejected={} reconnects={} book_checksums_ok={} mismatches={} resyncs={}",
            start.elapsed().as_secs_f64(),
            d.contract_label().map_or_else(
                || format!("{} (contract unknown)", d.product.symbol),
                |l| l.to_string()
            ),
            d.status.label(),
            d.series.len(),
            d.stats.updates_in,
            d.stats.trades_in,
            d.stats.frames_in,
            d.stats.gaps,
            d.stats.rejected,
            d.stats.reconnects,
            d.stats.book_checksums_ok,
            d.stats.book_checksum_mismatches,
            d.stats.book_resyncs
        );
        if matches!(
            d.status,
            lines_feed::store::FeedStatus::ReplayFinished
                | lines_feed::store::FeedStatus::Failed(_)
        ) {
            println!("{:?}", d.status);
            break;
        }
    }
    feed.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Level sources (decision 0022) register through the same extension point as market data.
    #[test]
    fn extensions_carry_level_sources() {
        let ext =
            Extensions::new().with_level_source(Arc::new(lines_feed::levels::demo::DemoLevels));
        assert_eq!(ext.level_sources().len(), 1);
        assert_eq!(ext.level_sources()[0].id(), "demo");
        assert!(ext.sources().is_empty());
    }

    fn help() -> String {
        command(&Extensions::new()).render_long_help().to_string()
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        parse_cli(args, &Extensions::new())
    }

    #[test]
    fn window_icon_decodes() {
        let icon = window_icon().expect("embedded PNG decodes");
        assert_eq!(icon.dimensions(), (256, 256));
    }

    #[test]
    fn vwap_flags_parse() {
        use lines_indicators::{Bands, Reset};
        assert_eq!(parse_vwap("eth").unwrap().reset, Reset::Eth);
        let v = parse_vwap("rth/1,3").unwrap();
        assert_eq!((v.reset, v.bands), (Reset::Rth, Bands(0b101)));
        let v = parse_vwap("anchor@2026-11-06T14:30:00Z/2").unwrap();
        assert_eq!(
            v.reset,
            Reset::Anchored {
                from: UtcMillis::parse_iso("2026-11-06T14:30:00Z").unwrap().0
            }
        );
        assert!(parse_vwap("hourly").is_err());
        assert!(parse_vwap("eth/4").is_err());
    }

    /// Decision 0020: registered sources are `--source` values with their help; without
    /// them (the public binary) the name is an ordinary invalid value.
    #[test]
    fn extension_sources_join_the_cli() {
        struct Ext;
        impl SourceExtension for Ext {
            fn name(&self) -> &'static str {
                "example-feed"
            }
            fn help(&self) -> &'static str {
                "An example extension source."
            }
            fn long_help(&self) -> Option<&'static str> {
                Some("example-feed reads EXAMPLE_TOKEN.")
            }
            fn provider(&self) -> Arc<dyn lines_feed::provider::Provider> {
                Arc::new(lines_feed::coinbase::CoinbaseProvider)
            }
            fn default_product(&self) -> lines_core::ProductId {
                catalog::BTC_USD
            }
            fn connect(
                &self,
                _: &lines_core::Product,
                _: Option<lines_core::ContractKey>,
            ) -> futures_util::future::BoxFuture<'static, Result<Box<dyn DynTransport>>>
            {
                Box::pin(async { bail!("never connects") })
            }
        }
        let ext = Extensions::new().with_source(Arc::new(Ext));
        let cli = parse_cli(["lines", "--source", "example-feed"], &ext).unwrap();
        assert!(matches!(
            SourceChoice::parse(&cli.source, &ext),
            Some(SourceChoice::Extension(_))
        ));
        let h = command(&ext).render_long_help().to_string();
        for needle in [
            "example-feed",
            "An example extension source.",
            "EXAMPLE_TOKEN",
        ] {
            assert!(
                h.contains(needle),
                "--help lacks {needle:?}:
{h}"
            );
        }
        let err = parse(&["lines", "--source", "example-feed"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
        assert!(!help().contains("example-feed"));
        // The default is still the mock.
        let cli = parse(&["lines"]).unwrap();
        assert!(matches!(
            SourceChoice::parse(&cli.source, &Extensions::new()),
            Some(SourceChoice::Builtin(SourceKind::Mock))
        ));
    }

    #[test]
    fn public_sources_always_parse() {
        for s in ["mock", "coinbase", "kraken", "replay"] {
            assert!(parse(&["lines", "--source", s]).is_ok(), "{s}");
        }
    }

    /// Decision 0014 / docs/release.md: public builds have no real-CQG connection at all.
    #[cfg(not(feature = "vendor-cqg"))]
    #[test]
    fn real_cqg_is_absent_without_the_feature() {
        let err = parse(&["lines", "--source", "cqg"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue, "{err}");
        assert!(parse(&["lines", "--cqg-url", "wss://x"]).is_err());
        let h = help();
        for needle in [
            "Real CQG",
            "--cqg-url",
            "--private-label",
            "--client-app-id",
            "LINES_CQG_",
        ] {
            assert!(!h.contains(needle), "--help mentions {needle:?}:\n{h}");
        }
    }

    #[cfg(feature = "vendor-cqg")]
    #[test]
    fn real_cqg_is_available_with_the_feature() {
        let cli = parse(&["lines", "--source", "cqg"]).unwrap();
        assert!(matches!(
            SourceChoice::parse(&cli.source, &Extensions::new()),
            Some(SourceChoice::Builtin(SourceKind::Cqg))
        ));
        assert_eq!(cli.cqg_url, lines_feed::cqg::DEMO_URL);
        assert!(help().contains("--cqg-url"));
    }
}
