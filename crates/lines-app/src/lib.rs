//! Lines as a library: the `lines` binary is [`run`] with no extensions. Official builds
//! (decision 0020) call the same entry point with their own sources registered through
//! [`Extensions`]; nothing in this crate names them.

mod bench;
mod chart_view;
mod commands;
mod dom_view;
mod feeds;
mod layout;
mod levels_layer;
mod menu;
mod pane;
mod pane_columns;
mod pane_window;
mod persist;
mod picker;
mod picker_view;
mod profile;
#[cfg(feature = "screenshot")]
mod screenshot;
mod studies;
mod tabstate;
mod tape_view;
mod text;
mod theme;
mod versioned;
mod workspace;
mod wsdoc;

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
use profile::{ProfileConfig, ProfileMode, ProfileSession};

use crate::dom_view::DomOptions;
use crate::feeds::Feeds;
use crate::layout::Preset;
use crate::picker_view::AppSettings;
use crate::tape_view::TapeOptions;
use crate::theme::{HollowBodies, Palette, Theme};
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

/// What the entry point adds to the built-in sources (decision 0020). Empty for the public
/// `lines` binary.
#[derive(Default, Clone)]
pub struct Extensions {
    sources: Vec<Arc<dyn SourceExtension>>,
    level_sources: Vec<Arc<dyn LevelSource>>,
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

/// Up/down colour preset.
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum PaletteKind {
    /// Mint up, rose down.
    Standard,
    /// Blue up, orange down (red-green colour blindness).
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
    /// Run the feed without a window for N seconds and print stats (useful for recording).
    #[arg(long)]
    headless: Option<u64>,
    /// Color theme.
    #[arg(long, value_enum, default_value_t = ThemeKind::Dark)]
    theme: ThemeKind,
    /// Up/down colour preset (`--palette cb` for the colourblind-safe one). Rising candle
    /// bodies are hollow in every preset, so direction never relies on colour alone.
    #[arg(long, value_enum, default_value_t = PaletteKind::Standard)]
    palette: PaletteKind,
    /// Which direction's candle bodies are hollow (the other is filled).
    #[arg(long, value_enum, default_value_t = HollowKind::Up)]
    hollow: HollowKind,
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
    /// in a hidden window, draw a frame every 8 ms for this many seconds while the feeds run,
    /// then print frame-time percentiles and memory and exit.
    #[arg(long, conflicts_with = "screenshot")]
    bench: Option<u64>,
    /// Benchmark: hide the workspace's DOMs (they must paint nothing).
    #[arg(long, requires = "bench")]
    bench_hide_doms: bool,
    /// DOM price grouping in ticks per row (G cycles 1, 2, 5, 10, 25 in the app).
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    dom_grouping: u32,
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
    /// Screenshot: zoom the auto-fitted price range by this factor around the plot's centre
    /// (as Ctrl+wheel would), which switches the price axis to manual scale.
    #[arg(long, requires = "screenshot", value_parser = parse_zoom)]
    price_zoom: Option<f64>,
    /// Screenshot: render with the symbol picker open on this query (e.g. "nq").
    #[arg(long, requires = "screenshot")]
    picker: Option<String>,
    /// Screenshot: the chart without study legends and tags (Ctrl+Shift+L in the app).
    #[arg(long, requires = "screenshot")]
    hide_study_labels: bool,
    /// Screenshot: the chart with its studies hidden (Ctrl+Shift+H in the app).
    #[arg(long, requires = "screenshot")]
    hide_studies: bool,
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
    /// Show the synthetic demo level source on ES/MES charts (builds with `demo-levels`).
    #[cfg(feature = "demo-levels")]
    #[arg(long)]
    demo_levels: bool,
    /// Add a VWAP to the first chart (repeatable): eth (default reset), rth, week, month or
    /// anchor@2026-11-06T14:30:00Z, optionally with σ bands after a slash: eth/1,2,3 (quote it
    /// in PowerShell). In the app, Ctrl+I opens a chart's indicator menu.
    #[arg(long, value_parser = parse_vwap)]
    vwap: Vec<lines_indicators::VwapSpec>,
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
    Ok(cfg)
}

/// The local store (decision 0019) for live sources: the mock invents a new market every
/// run and replays and screenshots must not write, so those never store.
fn store_config(cli: &Cli, choice: &SourceChoice) -> Option<lines_feed::disk::StoreConfig> {
    let source = match choice {
        _ if cli.no_store || cli.screenshot.is_some() => return None,
        SourceChoice::Extension(e) if e.local_store() => e.name(),
        SourceChoice::Builtin(SourceKind::Coinbase) => "coinbase",
        SourceChoice::Builtin(SourceKind::Kraken) => "kraken",
        #[cfg(feature = "vendor-cqg")]
        SourceChoice::Builtin(SourceKind::Cqg) => "cqg",
        SourceChoice::Extension(_)
        | SourceChoice::Builtin(SourceKind::Mock | SourceKind::Replay) => return None,
    };
    let Some(root) = lines_feed::disk::default_root() else {
        tracing::warn!("no data directory (set LINES_DATA_DIR); running without the local store");
        return None;
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
    // A second call in one process (tests) keeps the first subscriber.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,naga=warn,wgpu=warn".into()),
        )
        .try_init();
    for e in &ext.sources {
        lines_feed::extension::register(e.clone())?;
    }
    for l in &ext.level_sources {
        lines_feed::levels::register(l.clone())?;
    }
    // --help/--version print and exit 0; usage errors print and exit 2, as before.
    let cli = parse_cli(args, &ext).unwrap_or_else(|e| e.exit());
    let choice = SourceChoice::parse(&cli.source, &ext)
        .ok_or_else(|| anyhow!("unknown source {:?}", cli.source))?;
    let config = feed_config(&cli, &choice)?;
    lines_feed::levels::runtime::set_mode(match (&cli.record_levels, &cli.replay_levels) {
        (Some(d), _) => lines_feed::levels::runtime::LevelMode::Record(d.clone()),
        (_, Some(d)) => lines_feed::levels::runtime::LevelMode::Replay(d.clone()),
        _ => lines_feed::levels::runtime::LevelMode::Live,
    });
    #[cfg(feature = "demo-levels")]
    if cli.demo_levels {
        lines_feed::levels::register(Arc::new(lines_feed::levels::demo::DemoLevels))?;
    }

    if let Some(secs) = cli.headless {
        return run_headless(config, Duration::from_secs(secs));
    }
    let palette = match cli.palette {
        PaletteKind::Standard => Palette::Standard,
        PaletteKind::Colorblind => Palette::Colorblind,
    };
    let mut theme = match cli.theme {
        ThemeKind::Dark => Theme::dark(palette),
        ThemeKind::Light => Theme::light(palette),
    };
    theme.hollow = match cli.hollow {
        HollowKind::Up => HollowBodies::Up,
        HollowKind::Down => HollowBodies::Down,
    };

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
        theme,
        frozen,
        track_visibility: !frozen && !bench,
        dom: DomOptions {
            grouping: cli.dom_grouping,
        },
        tape: TapeOptions { large_print },
        record: std::cell::RefCell::new(record),
        fixture: std::cell::RefCell::new(Some(chart_view::Fixture {
            cursor: cli.cursor,
            price_zoom: cli.price_zoom,
            picker: cli.picker.clone(),
            indicators: cli
                .vwap
                .iter()
                .enumerate()
                .map(|(i, spec)| lines_indicators::IndicatorConfig::Vwap {
                    spec: *spec,
                    color: (i % 4) as u8,
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
            )?;
            std::process::exit(code);
        }
        let out = cli.screenshot.clone().expect("screenshot mode");
        #[cfg(feature = "screenshot")]
        {
            let shot = screenshot::Shot {
                out,
                size,
                scale: cli.scale,
                doc,
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
    if let Some(d) = dir {
        ws = ws.with_files(d, writer.clone(), autosave);
    }
    for r in reports {
        ws.add_report(r);
    }
    if ad_hoc {
        tracing::info!("--instrument/--layout: this run's layout is not autosaved");
    }

    gpui_platform::application().run(move |cx: &mut App| {
        cx.set_global(Feeds(registry));
        cx.set_global(settings);
        cx.set_global(ws);
        workspace::install(cx);
        if let Err(e) = workspace::open_workspace(&doc, cx) {
            tracing::error!("failed to open the workspace: {e:#}");
            cx.quit();
            return;
        }
        workspace::start_autosave(cx);
        cx.activate(true);
    });
    writer.flush(Duration::from_secs(3));
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
