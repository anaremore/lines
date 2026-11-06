mod chart_view;
#[cfg(feature = "screenshot")]
mod screenshot;
mod theme;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use lines_core::UtcMillis;
use lines_feed::cqg::mock::MockConfig;
use lines_feed::cqg::{self, LogonParams};
use lines_feed::{FeedConfig, Source, catalog};

use crate::chart_view::ChartView;
use crate::theme::Theme;

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SourceKind {
    /// In-process mock of the CQG WebAPI protocol.
    Mock,
    /// Real CQG WebAPI (needs credentials).
    Cqg,
    /// Coinbase Exchange public feed (free, 24/7; dev tool).
    Coinbase,
    /// A `.lfr` recording (provider + instrument come from the file).
    Replay,
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
    #[arg(long, value_enum, default_value_t = SourceKind::Mock)]
    source: SourceKind,
    /// Catalog instrument (ES, NQ, ZN, BTC/USD, ETH/USD). Default: ES, or BTC/USD for coinbase.
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
    #[arg(long, default_value = cqg::DEMO_URL)]
    cqg_url: String,
    #[arg(long, default_value = "CQG")]
    private_label: String,
    #[arg(long, default_value = "WebAPITest")]
    client_app_id: String,
    /// Run the feed without a window for N seconds and print stats (useful for recording).
    #[arg(long)]
    headless: Option<u64>,
    /// Color theme.
    #[arg(long, value_enum, default_value_t = ThemeKind::Dark)]
    theme: ThemeKind,
    /// Window size in logical pixels, WIDTHxHEIGHT.
    #[arg(long, default_value = "1280x800", value_parser = parse_size)]
    size: (f32, f32),
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

fn feed_config(cli: &Cli) -> Result<FeedConfig> {
    let source = match cli.source {
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
        SourceKind::Mock => Source::CqgMock(MockConfig {
            quotes_per_sec: cli.rate,
            quotes_per_frame: cli.batch.max(1),
            disconnect_after: cli.disconnect_every.map(Duration::from_secs),
            ..Default::default()
        }),
        SourceKind::Replay => {
            let Some(path) = cli.replay.clone() else {
                bail!("--source replay needs --replay <FILE>")
            };
            Source::Replay {
                path,
                speed: cli.speed,
                stop_at: cli.at,
            }
        }
        SourceKind::Coinbase => Source::Coinbase {
            url: lines_feed::coinbase::WS_URL.into(),
        },
        SourceKind::Cqg => {
            let user = std::env::var("LINES_CQG_USER").ok();
            let password = std::env::var("LINES_CQG_PASSWORD").ok();
            if user.is_none() || password.is_none() {
                bail!("set LINES_CQG_USER and LINES_CQG_PASSWORD for --source cqg");
            }
            Source::Cqg {
                url: cli.cqg_url.clone(),
                logon: LogonParams {
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
        cfg.instrument = Some(catalog::by_symbol(sym).with_context(|| {
            let known: Vec<_> = catalog::all().iter().map(|i| i.symbol).collect();
            format!("unknown instrument {sym:?}; known: {}", known.join(", "))
        })?);
    }
    cfg.record = cli.record.clone();
    Ok(cfg)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,naga=warn,wgpu=warn".into()),
        )
        .init();
    let cli = Cli::parse();
    let config = feed_config(&cli)?;

    if let Some(secs) = cli.headless {
        return run_headless(config, Duration::from_secs(secs));
    }
    let theme = match cli.theme {
        ThemeKind::Dark => Theme::dark(),
        ThemeKind::Light => Theme::light(),
    };

    if let Some(out) = cli.screenshot.clone() {
        #[cfg(feature = "screenshot")]
        {
            let shot = screenshot::Shot {
                out,
                size: cli.size,
                scale: cli.scale,
                theme,
                fixture: chart_view::Fixture {
                    cursor: cli.cursor,
                    price_zoom: cli.price_zoom,
                },
            };
            let code = screenshot::run(config, shot)?;
            std::process::exit(code);
        }
        #[cfg(not(feature = "screenshot"))]
        {
            let _ = out;
            bail!(
                "this build has no screenshot support; rebuild with `cargo run -p lines-app --features screenshot`"
            );
        }
    }

    let feed = lines_feed::spawn(config)?;
    let (w, h) = cli.size;
    gpui_platform::application().run(move |cx: &mut App| {
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Lines".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_, cx| cx.new(|cx| ChartView::new(feed, theme, false, Default::default(), cx)),
        );
        if let Err(e) = opened {
            tracing::error!("failed to open window: {e:#}");
            cx.quit();
            return;
        }
        cx.activate(true);
    });
    Ok(())
}

fn run_headless(config: FeedConfig, dur: Duration) -> Result<()> {
    let mut feed = lines_feed::spawn(config)?;
    let start = std::time::Instant::now();
    while start.elapsed() < dur {
        std::thread::sleep(Duration::from_secs(1));
        let d = feed.store.read();
        println!(
            "[{:>4.1}s] {} {:<16} bars={:<6} updates={:<9} trades={:<8} frames={:<7} gaps={} rejected={} reconnects={}",
            start.elapsed().as_secs_f64(),
            d.instrument.symbol,
            d.status.label(),
            d.series.len(),
            d.stats.updates_in,
            d.stats.trades_in,
            d.stats.frames_in,
            d.stats.gaps,
            d.stats.rejected,
            d.stats.reconnects
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
