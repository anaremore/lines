mod chart_view;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use lines_feed::mock::MockConfig;
use lines_feed::session::LogonParams;
use lines_feed::{FeedConfig, Source};

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SourceKind {
    Mock,
    Replay,
    Cqg,
}

/// Lines: futures charting terminal.
#[derive(Parser, Debug)]
#[command(version)]
struct Cli {
    /// Where market data comes from.
    #[arg(long, value_enum, default_value_t = SourceKind::Mock)]
    source: SourceKind,
    /// CQG symbol to chart (CQG naming: EP = E-mini S&P 500).
    #[arg(long, default_value = "EP")]
    symbol: String,
    /// Record every raw feed frame to this .lfr file.
    #[arg(long)]
    record: Option<PathBuf>,
    /// Recording to play back (with --source replay).
    #[arg(long)]
    replay: Option<PathBuf>,
    /// Replay speed: 1 = real time, 10 = 10x, 0 = as fast as possible.
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// Mock: quotes per second.
    #[arg(long, default_value_t = 5_000)]
    rate: u32,
    /// Mock: quotes per frame.
    #[arg(long, default_value_t = 50)]
    batch: u32,
    /// Mock: drop the connection every N seconds (exercise reconnect + back-fill).
    #[arg(long)]
    disconnect_every: Option<u64>,
    /// CQG WebAPI endpoint (demo by default). Credentials: LINES_CQG_USER / LINES_CQG_PASSWORD.
    #[arg(long, default_value = "wss://demoapi.cqg.com:443")]
    url: String,
    #[arg(long, default_value = "CQG")]
    private_label: String,
    #[arg(long, default_value = "WebAPITest")]
    client_app_id: String,
    /// Run the feed without a window for N seconds and print stats (useful for recording).
    #[arg(long)]
    headless: Option<u64>,
}

fn feed_config(cli: &Cli) -> Result<FeedConfig> {
    let source = match cli.source {
        SourceKind::Mock => Source::Mock(MockConfig {
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
            }
        }
        SourceKind::Cqg => {
            let user = std::env::var("LINES_CQG_USER").ok();
            let password = std::env::var("LINES_CQG_PASSWORD").ok();
            if user.is_none() || password.is_none() {
                bail!("set LINES_CQG_USER and LINES_CQG_PASSWORD for --source cqg");
            }
            Source::Cqg {
                url: cli.url.clone(),
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
    cfg.symbol = cli.symbol.clone();
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

    let feed = lines_feed::spawn(config)?;
    gpui_platform::application().run(move |cx: &mut App| {
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Lines".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_, cx| cx.new(|cx| chart_view::ChartView::new(feed, cx)),
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
            "[{:>4.1}s] {:<16} bars={:<6} quotes={:<9} trades={:<8} frames={:<7} reconnects={}",
            start.elapsed().as_secs_f64(),
            d.status.label(),
            d.series.len(),
            d.stats.quotes_in,
            d.stats.trades_in,
            d.stats.frames_in,
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
