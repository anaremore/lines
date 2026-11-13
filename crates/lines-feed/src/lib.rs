//! Market-data feed for Lines: provider abstraction, CQG (mock + WebAPI), Coinbase public
//! feed, recording and replay.
//!
//! The feed runs on its own OS thread with its own Tokio runtime (decision 0001). It never
//! touches the UI; it publishes into a shared [`ChartStore`] that the UI samples per frame.
//! Above this crate only catalog instruments are visible, never vendor symbols (0003).

pub mod availability;
pub mod catalog;
pub mod coinbase;
pub mod connection;
pub mod cqg;
pub mod disk;
pub mod download;
pub mod econ;
pub mod extension;
pub mod indicators;
pub mod kraken;
pub mod levels;
pub mod listen;
pub mod live_record;
pub mod native;
pub mod occ;
pub mod provider;
pub mod recorded;
pub mod recording;
pub mod registry;
pub mod replay_control;
pub mod store;
pub mod synth;
pub mod synth_chain;
pub mod synth_crypto;
pub mod ticks;
pub mod transport;

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use lines_core::{CivilDate, ContractKey, Product, UtcMillis};
use parking_lot::Mutex;
use tokio::sync::watch;

use connection::{ConnectionEnd, ConnectionError, ConnectionOptions, EventSink};
use cqg::mock::{MockConfig, MockMarket, MockTransport};
use provider::Provider;
use recording::Recorder;
use store::{ChartStore, FeedStatus};
use transport::{Recording, ReplayTransport, Transport, WsTransport};

pub use store::{ChartData, FeedStats};

pub fn now_utc() -> UtcMillis {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    UtcMillis(d.as_millis() as i64)
}

pub fn client_version() -> String {
    format!("lines {}", env!("CARGO_PKG_VERSION"))
}

#[derive(Clone, Debug)]
pub enum Source {
    /// In-process mock that speaks the CQG WebAPI protocol.
    CqgMock(MockConfig),
    /// Real CQG WebAPI. Untested until credentials exist. Only with the `vendor-cqg` feature.
    #[cfg(feature = "vendor-cqg")]
    Cqg {
        url: String,
        logon: cqg::LogonParams,
    },
    /// Coinbase Exchange public feed.
    Coinbase { url: String },
    /// Kraken public data: spot over WebSocket v2, perpetuals over the Kraken Futures feed
    /// (chosen by product). REST bases are the provider's (`kraken::KrakenProvider`).
    Kraken {
        spot_url: String,
        futures_url: String,
    },
    /// A source registered through the extension point (decision 0020).
    Extension(extension::ExtensionSource),
    /// Replay a `.lfr` recording (provider, product and contract come from its header).
    /// `speed` 1.0 = real time, 0 = as fast as possible. `stop_at` ends the replay before
    /// the first frame recorded after that instant.
    Replay {
        path: PathBuf,
        speed: f64,
        stop_at: Option<UtcMillis>,
    },
}

#[derive(Clone, Debug)]
pub struct FeedConfig {
    pub source: Source,
    /// A product (followed to its front month) or one contract; `None` = the source's
    /// default product (or the recording's contract).
    pub target: Option<catalog::Target>,
    /// Tee all raw frames to this `.lfr` file.
    pub record: Option<PathBuf>,
    pub bar_period_ms: i64,
    /// Bars kept in memory (older ones are trimmed).
    pub max_bars: usize,
    /// History requested when none is stored (default 3 days of 1-minute bars). Must fit in
    /// `max_bars`; reconnects request only from the newest stored bar.
    pub history_ms: i64,
    /// Keep this feed in the local market-data store (decision 0019); `None` = don't.
    pub store: Option<disk::StoreConfig>,
    /// At startup, replay the store's trades of this many newest stored trade dates into the
    /// order-flow consumers (`disk::orderflow`; 0 = off). Default 2: the current and the
    /// previous session.
    pub orderflow_sessions: u32,
    /// Override how long a live feed may be silent before panes show it stale
    /// (default per provider, [`stale_after`]).
    pub stale_after: Option<Duration>,
    /// A replay the replay bar controls (pause, speed, seek). `None`: it plays at
    /// `Source::Replay`'s fixed speed (screenshots, headless runs, backtests).
    pub replay: Option<Arc<replay_control::ReplayControl>>,
}

impl FeedConfig {
    pub fn new(source: Source) -> Self {
        FeedConfig {
            source,
            target: None,
            record: None,
            bar_period_ms: UtcMillis::MINUTE,
            max_bars: 50_000,
            history_ms: 3 * 24 * 60 * UtcMillis::MINUTE,
            store: None,
            orderflow_sessions: disk::orderflow::DEFAULT_SESSIONS,
            stale_after: None,
            replay: None,
        }
    }
}

/// A validated plan: which provider serves which product and contract.
struct Plan {
    provider: Arc<dyn Provider>,
    product: Product,
    /// `None` only for replays of recordings that don't name their contract (v2).
    contract: Option<ContractKey>,
    /// Re-resolve the front month on every (re)connect (the target was a product).
    follow_front: bool,
}

/// Front month for a product today. Volume isn't known before connecting, so this uses the
/// calendar rule (decision 0013's fallback); `current` keeps it from moving backwards.
pub fn front_month(product: &Product, current: Option<ContractKey>) -> Option<ContractKey> {
    let today = CivilDate::from_utc(now_utc());
    lines_core::roll::resolve_front(product, today, current, None, &[]).map(|f| f.contract)
}

fn provider_for(source: &Source) -> Result<Arc<dyn Provider>> {
    Ok(match source {
        Source::CqgMock(_) => Arc::new(cqg::CqgProvider::new(cqg::LogonParams {
            client_version: client_version(),
            ..Default::default()
        })),
        #[cfg(feature = "vendor-cqg")]
        Source::Cqg { logon, .. } => Arc::new(cqg::CqgProvider::new(logon.clone())),
        Source::Coinbase { .. } => Arc::new(coinbase::CoinbaseProvider),
        Source::Kraken { .. } => Arc::new(kraken::KrakenProvider::default()),
        Source::Extension(e) => e.provider(),
        Source::Replay { path, .. } => {
            let h = transport::read_header(path)?;
            match h.provider.as_str() {
                provider::CQG if cqg::protocol_compatible(&h.protocol) => {
                    Arc::new(cqg::CqgProvider::new(cqg::LogonParams::default()))
                }
                provider::COINBASE if coinbase::protocol_compatible(&h.protocol) => {
                    Arc::new(coinbase::CoinbaseProvider)
                }
                provider::KRAKEN if kraken::protocol_compatible(&h.protocol) => {
                    Arc::new(kraken::KrakenProvider::default())
                }
                native::PROVIDER if native::protocol_compatible(&h.protocol) => {
                    Arc::new(native::NativeProvider)
                }
                p => match extension::for_recording(p, &h.protocol) {
                    Some(e) => e.provider(),
                    None => bail!(
                        "recording is from provider {p:?} protocol {:?}, which this build cannot decode",
                        h.protocol
                    ),
                },
            }
        }
    })
}

fn plan(config: &FeedConfig) -> Result<Plan> {
    use catalog::Target;
    let provider = provider_for(&config.source)?;
    let get = |id| catalog::get(id).ok_or_else(|| anyhow!("unknown product {id}"));
    let (product, contract, follow_front) = match (&config.source, &config.target) {
        (Source::Replay { path, .. }, requested) => {
            let h = transport::read_header(path)?;
            let product = catalog::by_symbol(&h.product)
                .ok_or_else(|| anyhow!("recording is for unknown product {:?}", h.product))?;
            if let Some(k) = h.contract
                && k.product != product.id
            {
                bail!(
                    "corrupt recording header: contract {k:?} is not a {} contract",
                    product.symbol
                );
            }
            let label = |k: ContractKey| k.label(&product).as_str().to_owned();
            match requested {
                Some(Target::Product(id)) if *id != product.id => {
                    bail!(
                        "recording is for {}, not {}",
                        product.symbol,
                        get(*id)?.symbol
                    )
                }
                Some(Target::Contract(k)) => match h.contract {
                    _ if k.product != product.id => {
                        bail!(
                            "recording is for {}, not {}",
                            product.symbol,
                            get(k.product)?.symbol
                        )
                    }
                    Some(c) if c != *k => bail!("recording is for {}, not {}", label(c), label(*k)),
                    None => bail!(
                        "this v{} recording doesn't say which contract it holds; request the product ({}) instead",
                        h.version,
                        product.symbol
                    ),
                    _ => {}
                },
                _ => {}
            }
            (product, h.contract, false)
        }
        (_, Some(Target::Contract(k))) => (get(k.product)?, Some(*k), false),
        (source, target) => {
            let id = match (target, source) {
                (Some(Target::Product(id)), _) => *id,
                (_, Source::Coinbase { .. }) => catalog::BTC_USD,
                (_, Source::Kraken { .. }) => catalog::BTC_USD_KR,
                (_, Source::Extension(e)) => e.default_product(),
                _ => catalog::ES,
            };
            let product = get(id)?;
            let front = front_month(&product, None)
                .ok_or_else(|| anyhow!("no listed contract for {}", product.symbol))?;
            (product, Some(front), true)
        }
    };
    if !provider.supports(&product) {
        bail!("{} does not provide {}", provider.id(), product.symbol);
    }
    Ok(Plan {
        provider,
        product,
        contract,
        follow_front,
    })
}

/// The product, contract and front-month following a config resolves to (validated).
pub(crate) fn resolve(
    config: &FeedConfig,
) -> Result<(lines_core::ProductId, Option<ContractKey>, bool)> {
    let p = plan(config)?;
    Ok((p.product.id, p.contract, p.follow_front))
}

/// Owns the feed thread. Dropping it shuts the feed down and joins the thread.
pub struct FeedHandle {
    pub store: Arc<ChartStore>,
    shutdown: watch::Sender<bool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FeedHandle {
    pub fn shutdown(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for FeedHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Create the store for a config (validates provider, product and contract up front).
pub fn new_store(config: &FeedConfig) -> Result<Arc<ChartStore>> {
    let p = plan(config)?;
    let store = Arc::new(ChartStore::new(
        p.product,
        p.contract,
        p.provider.capabilities(),
        p.provider.id(),
        config.bar_period_ms,
        config.max_bars,
    ));
    if let (Source::Replay { .. }, Some(c)) = (&config.source, &config.replay) {
        store.set_replay(c.clone());
    }
    Ok(store)
}

/// Start the feed on a dedicated thread.
pub fn spawn(config: FeedConfig) -> Result<FeedHandle> {
    let store = new_store(&config)?;
    let (tx, rx) = watch::channel(false);
    let s = store.clone();
    let thread = thread::Builder::new()
        .name("lines-feed".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("lines-feed-rt")
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    s.set_status(FeedStatus::Failed(format!("tokio runtime: {e}")));
                    return;
                }
            };
            rt.block_on(run_feed(config, s, rx));
        })
        .context("spawn feed thread")?;
    Ok(FeedHandle {
        store,
        shutdown: tx,
        thread: Some(thread),
    })
}

/// A live source refused the login (not a recording of one).
fn refuses_logon(e: &connection::ConnectionError) -> bool {
    matches!(
        e,
        connection::ConnectionError::Session(provider::SessionError::LogonRejected { .. })
    )
}

/// The reconnect loop. Public so tests and tools can run it on their own runtime.
pub async fn run_feed(
    config: FeedConfig,
    store: Arc<ChartStore>,
    mut shutdown: watch::Receiver<bool>,
) {
    let plan = match plan(&config) {
        Ok(p) => p,
        Err(e) => {
            store.set_status(FeedStatus::Failed(format!("{e:#}")));
            return;
        }
    };
    let mut recorder = match &config.record {
        Some(path) => match Recorder::create(
            path,
            plan.provider.id(),
            &plan.provider.protocol(),
            plan.product.symbol,
            plan.contract,
        ) {
            Ok(r) => {
                tracing::info!("recording raw frames to {}", path.display());
                let r = Arc::new(r);
                store
                    .recording
                    .set_active(path.clone(), now_utc().0, r.clone());
                Some(r)
            }
            Err(e) => {
                store.set_status(FeedStatus::Failed(format!(
                    "cannot create recording {}: {e}",
                    path.display()
                )));
                return;
            }
        },
        None => None,
    };
    // A live feed can be recorded from the app ("Start recording"); a replay never again.
    store
        .recording
        .set_recordable(!matches!(config.source, Source::Replay { .. }));
    // The local store: cached bars first (panes show them at once), then the feed fills from
    // where they end. One writer per contract; a roll switches it.
    let mut writer = open_writer(&config, &plan.product, plan.contract, &store);
    // A replay's tick bars are rebuilt from its own file (from the first frame).
    if let Source::Replay { path, .. } = &config.source {
        let source = ticks::TickSource::Replay(path.clone());
        store.update(|d| d.tick_source = Some(source));
    }
    // Indicators see every batch of this feed from the start (decision 0017).
    let mut tap = indicators::engine().tap(
        indicators::FeedInfo {
            feed: store.feed_id(),
            product: plan.product.id,
            period_ms: config.bar_period_ms,
            max_bars: config.max_bars,
        },
        plan.provider.capabilities().history,
        matches!(config.source, Source::Replay { .. }),
    );
    if let Some(w) = writer.as_ref() {
        warm_start(&config, w, &store, &mut tap);
    }
    store.set_stale_after(
        config
            .stale_after
            .unwrap_or_else(|| stale_after(plan.provider.id())),
    );
    let mock_market = match &config.source {
        Source::CqgMock(m) => Some(match &m.market {
            Some(shared) => shared.0.clone(),
            None => Arc::new(Mutex::new(MockMarket::new(m, now_utc()))),
        }),
        _ => None,
    };
    let is_replay = matches!(config.source, Source::Replay { .. });
    let control = config.replay.clone().filter(|_| is_replay);
    if let (Some(c), Source::Replay { path, .. }) = (&control, &config.source) {
        scan_last_frame(path.clone(), c.clone());
    }
    let opts = if let Source::Extension(e) = &config.source {
        let t = e.timing();
        ConnectionOptions {
            idle_timeout: t.idle_timeout,
            ping_every: t.keepalive_every,
            history_ms: config.history_ms,
        }
    } else if is_replay {
        // Replay is driven by recorded frames only: no idle timeout, no keep-alives.
        ConnectionOptions {
            idle_timeout: Duration::from_secs(365 * 86_400),
            ping_every: Duration::from_secs(86_400),
            history_ms: config.history_ms,
        }
    } else {
        ConnectionOptions {
            history_ms: config.history_ms,
            ..ConnectionOptions::default()
        }
    };
    let mut attempt: u32 = 0;

    loop {
        if *shutdown.borrow() {
            break;
        }
        store.set_status(FeedStatus::Connecting);
        let started = tokio::time::Instant::now();
        // Following a product: re-resolve the front month on every (re)connect. A roll only
        // moves the *chart* to the next contract (old bars retired, not mixed). Nothing here
        // touches positions; rolling a position is always an explicit pair of orders.
        // A writer opened mid-connection stored segments only: now a full one (bar index,
        // coverage), with the cached bars older than the chart's.
        if writer.as_ref().is_some_and(|w| w.segments_only()) {
            drop(writer.take());
            let k = store.read().contract;
            writer = full_writer(&config, &plan.product, k, &store, &mut tap);
        }
        let current = store.read().contract;
        let contract = if plan.follow_front {
            let next = front_month(&plan.product, current).or(current);
            if next != current
                && let Some(k) = next
            {
                tracing::info!("front month rolled to {}", k.label(&plan.product));
                store.update(|d| d.switch_contract(k));
                drop(writer.take());
                writer = open_writer(&config, &plan.product, Some(k), &store);
                tap.reset();
                if let Some(w) = writer.as_ref() {
                    warm_start(&config, w, &store, &mut tap);
                }
                // Recordings hold exactly one contract: stop rather than mix two.
                // Ended first, so this thread drops the last copy (and joins its writer).
                if recorder.is_some() {
                    tracing::warn!("recording stopped at the roll (recordings hold one contract)");
                    store.recording.end();
                    drop(recorder.take());
                }
            }
            next
        } else {
            current
        };
        let mut session = match plan.provider.new_session(&plan.product, contract) {
            Ok(s) => s,
            Err(e) => {
                // The reason first: whoever sees the flag reads it.
                store.set_status(FeedStatus::Failed(e.to_string()));
                if !is_replay && matches!(e, provider::SessionError::LogonRejected { .. }) {
                    store.set_logon_refused();
                }
                break;
            }
        };
        let session = session.as_mut();
        // A recording stopped from the app is finished: drop it. One asked for starts here,
        // at a connect, so its file begins with a whole session.
        if recorder.as_ref().is_some_and(|r| r.is_stopped()) {
            recorder = None;
        }
        if let Some(dir) = store.recording.take_pending() {
            recorder = start_recording(&dir, &plan, contract, &store);
        }
        let rec = recorder.as_deref();
        // The contract's writer; without one (a past day of this contract being downloaded
        // holds its lock) the sink tries again every so often, so this session is stored
        // from then on.
        let mut store_sink = StoreSink {
            writer: &mut writer,
            retry: config.store.as_ref().map(|cfg| Retry {
                cfg,
                product: &plan.product,
                contract,
                store: &store,
                next: None,
            }),
        };
        let mut tee = indicators::Tee(&mut store_sink, &mut tap);
        // Listeners (e.g. the trading core's paper broker) get every batch too.
        let mut listen_tap = listen::ListenTap {
            feed: store.feed_id(),
        };
        let mut tee = indicators::Tee(&mut tee, &mut listen_tap);
        let sink: &mut dyn EventSink = &mut tee;

        let result = match &config.source {
            Source::CqgMock(m) => {
                let t =
                    MockTransport::connect(m.clone(), mock_market.clone().expect("mock market"));
                drive(t, rec, session, &store, &opts, &mut shutdown, sink).await
            }
            // A stored contract or day replays like a recording (decision 0019).
            Source::Replay {
                path,
                speed,
                stop_at,
            } if disk::is_stored(path) => {
                match disk::replay::SegmentReplay::open(path, *speed, *stop_at) {
                    Ok(t) => {
                        let t = t.controlled(control.clone());
                        drive(t, rec, session, &store, &opts, &mut shutdown, sink).await
                    }
                    Err(e) => Err(ConnectionError::Transport(e)),
                }
            }
            Source::Replay {
                path,
                speed,
                stop_at,
            } => match ReplayTransport::open(path, *speed, *stop_at) {
                Ok(t) => {
                    let t = t.controlled(control.clone());
                    drive(t, rec, session, &store, &opts, &mut shutdown, sink).await
                }
                Err(e) => Err(ConnectionError::Transport(e)),
            },
            #[cfg(feature = "vendor-cqg")]
            Source::Cqg { url, .. } => {
                match WsTransport::connect(url, Duration::from_secs(15)).await {
                    Ok(t) => drive(t, rec, session, &store, &opts, &mut shutdown, sink).await,
                    Err(e) => Err(ConnectionError::Transport(e)),
                }
            }
            Source::Coinbase { url } => {
                match WsTransport::connect(url, Duration::from_secs(15)).await {
                    Ok(t) => drive(t, rec, session, &store, &opts, &mut shutdown, sink).await,
                    Err(e) => Err(ConnectionError::Transport(e)),
                }
            }
            Source::Extension(e) => match e.connect(&plan.product, contract).await {
                Ok(t) => {
                    let t = extension::BoxedTransport(t);
                    drive(t, rec, session, &store, &opts, &mut shutdown, sink).await
                }
                Err(e) => Err(ConnectionError::Transport(e)),
            },
            Source::Kraken {
                spot_url,
                futures_url,
            } => {
                let url = if kraken::is_futures(&plan.product) {
                    futures_url
                } else {
                    spot_url
                };
                match WsTransport::connect(url, Duration::from_secs(15)).await {
                    Ok(t) => drive(t, rec, session, &store, &opts, &mut shutdown, sink).await,
                    Err(e) => Err(ConnectionError::Transport(e)),
                }
            }
        };

        match result {
            Ok(ConnectionEnd::Shutdown) => break,
            // A recording asked to start: reconnect at once with it.
            Ok(ConnectionEnd::Restart) => {
                attempt = 0;
                continue;
            }
            Ok(ConnectionEnd::Closed) if is_replay => {
                let Some(c) = control.as_ref() else {
                    store.set_status(FeedStatus::ReplayFinished);
                    break;
                };
                // The replay bar: a finished replay waits for a seek back (or the end of the
                // run); a seek back restarts the file from its start.
                if !c.rewind_requested() {
                    c.set_finished();
                    store.set_status(FeedStatus::ReplayFinished);
                    tokio::select! {
                        _ = c.wait_for_rewind() => {}
                        _ = shutdown.changed() => break,
                    }
                }
                c.restarted();
                store.rewind();
                tap.reset();
                // Listeners (the trading core) learn where the earlier data ends.
                listen::restarted(store.feed_id());
                continue;
            }
            Err(e) if !e.is_retryable() || is_replay => {
                tracing::error!("feed stopped: {e:#}");
                // The reason first: whoever sees the flag reads it.
                store.set_status(FeedStatus::Failed(e.to_string()));
                if !is_replay && refuses_logon(&e) {
                    store.set_logon_refused();
                }
                break;
            }
            other => {
                let reason = match other {
                    Err(e) => e.to_string(),
                    _ => "server closed the connection".to_string(),
                };
                // A connection that stayed up a while resets the backoff.
                if started.elapsed() > Duration::from_secs(30) {
                    attempt = 0;
                }
                let delay = connection::backoff(attempt);
                attempt = attempt.saturating_add(1);
                tracing::warn!("connection lost ({reason}); reconnecting in {delay:?}");
                store.update(|d| {
                    d.stats.reconnects += 1;
                    d.status = FeedStatus::Reconnecting { attempt, reason };
                });
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown.changed() => break,
                }
            }
        }
    }
    // The feed has stopped: nothing more to record here.
    store.recording.close();
    if let Some(r) = recorder.and_then(|r| Arc::try_unwrap(r).ok())
        && let Err(e) = r.finish()
    {
        tracing::error!("recording incomplete: {e}");
    }
    if !matches!(
        store.read().status,
        FeedStatus::Failed(_) | FeedStatus::ReplayFinished
    ) {
        store.set_status(FeedStatus::Stopped);
    }
}

/// Create a recording asked for from the app in `dir`: `<source>-<symbol>-<UTC time>.lfr`, the
/// provider (the source) in its header. A failure is reported to the app
/// (`RecordCtl::take_error`, shown inline there) and never stops the feed.
fn start_recording(
    dir: &std::path::Path,
    plan: &Plan,
    contract: Option<ContractKey>,
    store: &ChartStore,
) -> Option<Arc<Recorder>> {
    let now = now_utc().0;
    let symbol = contract.map_or_else(
        || plan.product.symbol.to_owned(),
        |k| k.label(&plan.product).to_string(),
    );
    let name = live_record::file_name(plan.provider.id(), &symbol, now);
    let made = live_record::fresh_path(dir, &name).and_then(|path| {
        Recorder::create(
            &path,
            plan.provider.id(),
            &plan.provider.protocol(),
            plan.product.symbol,
            contract,
        )
        .map(|r| (path, r))
    });
    match made {
        Ok((path, r)) => {
            tracing::info!("recording raw frames to {}", path.display());
            let r = Arc::new(r);
            // A stop while the file was being created wins: no recording.
            store
                .recording
                .set_active(path, now, r.clone())
                .then_some(r)
        }
        Err(e) => {
            tracing::warn!("can't start a recording in {}: {e}", dir.display());
            store
                .recording
                .fail(format!("can't record in {}: {e}", dir.display()));
            None
        }
    }
}

/// The replay bar's timeline end: the last recorded frame's time, read on a helper thread
/// (a large recording takes a while; the bar shows no end until then).
fn scan_last_frame(path: PathBuf, control: Arc<replay_control::ReplayControl>) {
    let spawned = thread::Builder::new()
        .name("lines-replay-scan".into())
        .spawn(move || {
            let last = if disk::is_stored(&path) {
                disk::replay::SegmentReplay::open(&path, 0.0, None).and_then(|mut r| {
                    let mut last = None;
                    // The latest time, not the last in file order (days can come back in time).
                    while let Some((t, _)) = r.next_frame()? {
                        let t = t.div_euclid(1000);
                        last = Some(last.map_or(t, |l: i64| l.max(t)));
                    }
                    Ok(last)
                })
            } else {
                recording::Reader::open(&path)
                    .and_then(|mut r| {
                        let mut last = None;
                        while let Some(rec) = r.next_record()? {
                            if rec.direction == recording::Direction::FromServer {
                                let t = rec.wall_ms;
                                last = Some(last.map_or(t, |l: i64| l.max(t)));
                            }
                        }
                        Ok(last)
                    })
                    .map_err(anyhow::Error::from)
            };
            match last {
                Ok(Some(t)) => control.set_last(UtcMillis(t)),
                Ok(None) => {}
                Err(e) => tracing::warn!("replay timeline: can't read the recording's end: {e:#}"),
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("replay timeline: no scan thread: {e}");
    }
}

async fn drive<T: Transport>(
    t: T,
    recorder: Option<&Recorder>,
    session: &mut dyn provider::ProviderSession,
    store: &ChartStore,
    opts: &ConnectionOptions,
    shutdown: &mut watch::Receiver<bool>,
    sink: &mut dyn EventSink,
) -> Result<ConnectionEnd, ConnectionError> {
    use connection::run_connection_with;
    match recorder {
        Some(r) => {
            run_connection_with(
                &mut Recording::new(t, r),
                session,
                store,
                opts,
                shutdown,
                sink,
            )
            .await
        }
        None => {
            let mut t = t;
            run_connection_with(&mut t, session, store, opts, shutdown, sink).await
        }
    }
}

/// Open the local store's writer for a contract and show its cached bars. Store problems
/// never stop the feed: they are logged and the feed runs without the store (a locked
/// contract is tried again during the connection: `StoreSink`).
fn open_writer(
    config: &FeedConfig,
    product: &Product,
    contract: Option<ContractKey>,
    store: &Arc<ChartStore>,
) -> Option<disk::ContractWriter> {
    let cfg = config.store.as_ref()?;
    match disk::ContractWriter::open(cfg, product, contract, store.clone()) {
        Ok((w, opened)) => {
            tracing::info!(
                "local store {}: {} cached bars{}",
                w.dir().path.display(),
                opened.bars.len(),
                if opened.rebuilt {
                    " (rebuilt from segments)"
                } else {
                    ""
                }
            );
            let source = ticks::TickSource::Stored(w.dir().clone());
            store.update(|d| {
                d.load_cached(&opened.bars);
                // Tick bars are rebuilt from this contract's stored trades.
                d.tick_source = Some(source);
            });
            Some(w)
        }
        Err(e) => {
            tracing::warn!("local store unavailable: {e}");
            None
        }
    }
}

/// The full writer for a feed whose earlier connection stored segments only: the cached bars
/// older than the chart's oldest go into the chart (newer ones would overwrite what this run
/// built), so its bar index saves keep them; no stored order flow is replayed (this run
/// counted those trades live).
fn full_writer(
    config: &FeedConfig,
    product: &Product,
    contract: Option<ContractKey>,
    store: &Arc<ChartStore>,
    tap: &mut indicators::EngineTap,
) -> Option<disk::ContractWriter> {
    let cfg = config.store.as_ref()?;
    match disk::ContractWriter::open(cfg, product, contract, store.clone()) {
        Ok((w, opened)) => {
            let source = ticks::TickSource::Stored(w.dir().clone());
            // Chosen outside the lock; put in front in one step (the lock is held for a copy).
            let oldest = store.read().series.bars().first().map(|b| b.time);
            let older: Vec<lines_core::Bar> = opened
                .bars
                .into_iter()
                .filter(|b| oldest.is_none_or(|o| b.time < o))
                .collect();
            store.update(|d| {
                d.series.prepend_older(&older);
                d.tick_source = Some(source);
            });
            // The indicator engine's series gets them too, as at startup (`warm_start`), and
            // where the store's bars are complete.
            tap.cached(older);
            tap.covered(w.coverage().bars.ranges().to_vec());
            Some(w)
        }
        Err(e) => {
            tracing::warn!("local store unavailable: {e}");
            None
        }
    }
}

/// Feed time between tries to open a contract's writer that another holds.
const RETRY_WRITER_MS: i64 = 10_000;

/// Where to open a writer that wasn't available.
struct Retry<'a> {
    cfg: &'a disk::StoreConfig,
    product: &'a Product,
    contract: Option<ContractKey>,
    store: &'a Arc<ChartStore>,
    /// The next try (feed time); `None`: at the next frame.
    next: Option<i64>,
}

/// The local store's sink for one connection: the contract's writer, or, while another
/// writer holds it (a past day of this contract being downloaded, or another Lines), a try
/// every [`RETRY_WRITER_MS`] of feed time. A writer opened mid-connection stores this
/// connection's trade segments only: it never saves the bar index or claims coverage (the
/// chart doesn't hold the cached bars, so its bars would overwrite the index, a downloaded
/// day's included). At the next connection the feed opens a full writer
/// ([`full_writer`]).
struct StoreSink<'a> {
    writer: &'a mut Option<disk::ContractWriter>,
    retry: Option<Retry<'a>>,
}

impl EventSink for StoreSink<'_> {
    fn begin(
        &mut self,
        newest_bar: Option<UtcMillis>,
        window_start: UtcMillis,
    ) -> Option<UtcMillis> {
        match self.writer.as_mut() {
            Some(w) => w.begin(newest_bar, window_start),
            None => newest_bar,
        }
    }

    fn frame(&mut self, events: &[provider::MarketEvent], now: UtcMillis) {
        if self.writer.is_none()
            && let Some(r) = self.retry.as_mut()
            && r.next.is_none_or(|t| now.0 >= t)
        {
            r.next = Some(now.0 + RETRY_WRITER_MS);
            match disk::ContractWriter::open_segments_only(
                r.cfg,
                r.product,
                r.contract,
                r.store.clone(),
            ) {
                Ok(mut w) => {
                    tracing::info!(
                        "local store {}: the contract is free again; storing from now",
                        w.dir().path.display()
                    );
                    w.begin(None, now);
                    *self.writer = Some(w);
                }
                Err(e) => tracing::debug!("local store still unavailable: {e}"),
            }
        }
        if let Some(w) = self.writer.as_mut() {
            w.frame(events, now);
        }
    }

    fn end(&mut self, now: UtcMillis) {
        if let Some(w) = self.writer.as_mut() {
            w.end(now);
        }
    }
}

/// Stored trades applied per store update at startup (a few ms under the lock).
const STORED_SLICE: usize = 20_000;

/// The local store's warm start (decision 0019), on the feed thread before connecting: the
/// engine gets the cached bars, then the stored trades of the newest stored sessions replay
/// into everything that counts trades (`disk::orderflow`). Panes show the bars first and
/// "loading order flow" until this ends. Store problems never stop the feed.
fn warm_start(
    config: &FeedConfig,
    writer: &disk::ContractWriter,
    store: &Arc<ChartStore>,
    tap: &mut indicators::EngineTap,
) {
    let (bars, newest) = {
        let d = store.read();
        (d.series.bars().to_vec(), d.series.last().map(|b| b.time))
    };
    tap.cached(bars);
    // Where those bars are complete: a history gap the provider reports there isn't missing.
    tap.covered(writer.coverage().bars.ranges().to_vec());
    let Some(from) = newest
        .and_then(|t| disk::orderflow::lookback_start(writer.dir(), t, config.orderflow_sessions))
    else {
        return;
    };
    let dir = writer.dir().clone();
    store.update(|d| {
        d.orderflow_loading = true;
        // Tick-bar rebuilds read the same stored trades again (`ticks`).
        d.stored.dir = Some(dir);
    });
    let result = disk::orderflow::read(writer.dir(), from, |chunk| {
        // In slices, so the store's lock is held for milliseconds (panes keep drawing); the
        // coverage comes with the last one.
        // The indicator engine gets the same slices (one batch never holds a busy day).
        for part in chunk.trades.chunks(STORED_SLICE) {
            store.update(|d| d.apply_stored(part, &[]));
            tap.stored(part.to_vec(), Vec::new());
        }
        store.update(|d| d.apply_stored(&[], &chunk.covered));
        tap.stored(Vec::new(), chunk.covered);
    });
    // Tick charts that asked meanwhile rebuild now, from the trades replayed (`ticks`).
    drop(store.end_orderflow_loading());
    match result {
        Ok(s) => tracing::info!(
            "stored order flow: {} trades from {} trade dates in {:?} ({} decoded, {} \
             segments cut)",
            s.replayed,
            s.dates,
            s.elapsed,
            s.decoded,
            s.cut
        ),
        Err(e) => tracing::warn!("stored order flow unavailable: {e}"),
    }
}

/// How long a live feed may go without any frame before panes show it as stale
/// (docs/design.md §7). Coinbase and the mock send several frames a second (Coinbase has a
/// 1 s heartbeat); CQG can be quiet in slow markets, so it gets longer.
pub fn stale_after(provider: &str) -> Duration {
    match provider {
        provider::CQG => Duration::from_secs(10),
        p => extension::by_provider(p).map_or(Duration::from_secs(5), |e| e.timing().stale_after),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A live feed that finds its contract locked by a day being downloaded stores again once
    /// the lock frees, at its next try (feed time), not only after a reconnect.
    #[test]
    fn a_feed_stores_again_once_a_download_frees_the_contract() {
        let root = lines_core::scratch::Scratch::new("late-writer");
        let es = catalog::get(catalog::ES).unwrap();
        let k = ContractKey::monthly(es.id, 2026, lines_core::Month::new(12).unwrap());
        let cfg = disk::StoreConfig::new(root.to_path_buf(), "test");
        let store = Arc::new(ChartStore::new(
            es.clone(),
            Some(k),
            lines_core::Capabilities::default(),
            "test",
            60_000,
            1000,
        ));
        let dir = disk::ContractDir::new(&root, "test", &es, Some(k));
        // A day downloaded earlier: its bars in the index, its session claimed.
        let wed = lines_core::CivilDate::new(2026, 11, 4).unwrap();
        let after = UtcMillis::parse_iso("2026-11-06T00:00:00Z").unwrap();
        disk::bars::save(&dir.bars_path(), 60_000, &[]).unwrap();
        let mut w = disk::day::DayWriter::begin(&root, "test", &es, Some(k), wed, after).unwrap();
        let at = w.span().0.0 * 1000 + 60_000_000;
        w.block(
            at,
            &[native::NativeEvent::Trade {
                time_us: at,
                price: lines_core::Price(600_000),
                qty: lines_core::Qty(3),
                aggressor: native::Aggressor::Buy,
            }],
        )
        .unwrap();
        w.commit().unwrap();
        let bars_before = disk::bars::load(&dir.bars_path(), 60_000).unwrap();
        let coverage_before = dir.coverage();
        assert_eq!(bars_before.len(), 1);
        // Another day downloading now: the feed finds the contract locked.
        let thu = lines_core::CivilDate::new(2026, 11, 5).unwrap();
        let day = disk::day::DayWriter::begin(&root, "test", &es, Some(k), thu, after).unwrap();
        let mut writer = None;
        let t0 = UtcMillis::parse_iso("2026-11-09T15:00:00Z").unwrap();
        let trade = |t: UtcMillis| {
            [provider::MarketEvent::Trade {
                time: t,
                price: lines_core::Price(600_000),
                qty: lines_core::Qty(1),
                aggressor: None,
            }]
        };
        {
            let mut sink = StoreSink {
                writer: &mut writer,
                retry: Some(Retry {
                    cfg: &cfg,
                    product: &es,
                    contract: Some(k),
                    store: &store,
                    next: None,
                }),
            };
            sink.frame(&trade(t0), t0);
            assert!(sink.writer.is_none(), "locked by the download");
            drop(day); // the download ends
            let t1 = UtcMillis(t0.0 + RETRY_WRITER_MS - 1);
            sink.frame(&trade(t1), t1);
            assert!(sink.writer.is_none(), "not due yet");
            let t2 = UtcMillis(t0.0 + RETRY_WRITER_MS);
            sink.frame(&trade(t2), t2);
            assert!(sink.writer.is_some(), "opened at the next try");
            assert!(sink.writer.as_ref().unwrap().segments_only());
            sink.end(t2);
        }
        drop(writer);
        // The late writer saved neither the bar index nor coverage: the downloaded day stays.
        assert_eq!(
            disk::bars::load(&dir.bars_path(), 60_000).unwrap(),
            bars_before
        );
        assert_eq!(dir.coverage(), coverage_before);
        let date = disk::partition::trade_date(&es, t0);
        assert!(
            dir.segment_path(date).exists(),
            "the trade at the try is stored"
        );

        // The next connection: a full writer, the cached bars older than the chart's in front;
        // when it saves, the index and coverage still hold the downloaded day.
        let mut config = FeedConfig::new(Source::Replay {
            path: root.join("none.lfr"),
            speed: 0.0,
            stop_at: None,
        });
        config.store = Some(cfg.clone());
        let info = indicators::FeedInfo {
            feed: store.feed_id(),
            product: es.id,
            period_ms: 60_000,
            max_bars: 1000,
        };
        let vwap = indicators::engine().acquire(
            info,
            lines_indicators::Spec::Vwap {
                period_ms: 60_000,
                vwap: lines_indicators::VwapSpec {
                    reset: lines_indicators::Reset::Eth,
                    bands: lines_indicators::Bands::NONE,
                },
            },
        );
        let mut tap = indicators::engine().tap(info, false, false);
        // This run's own (live) bar, newer than the cached ones.
        let live = lines_core::Bar::from_trade(
            t0.floor_to(60_000),
            lines_core::Price(600_100),
            lines_core::Qty(1),
        );
        store.update(|d| d.series.upsert(live));
        let mut full = full_writer(&config, &es, Some(k), &store, &mut tap).unwrap();
        assert!(!full.segments_only());
        let mut want = bars_before.clone();
        want.push(live);
        assert_eq!(
            store.read().series.bars(),
            &want[..],
            "older bars in front of the live one"
        );
        // The indicator engine got the cached bars too.
        indicators::engine().sync();
        let cached_at = bars_before[0].time;
        assert!(
            vwap.cell()
                .read()
                .points
                .iter()
                .any(|p| p.time == cached_at),
            "the engine's series has the cached bar"
        );
        let t3 = UtcMillis(t0.0 + RETRY_WRITER_MS + 60_000);
        full.begin(None, t3);
        full.frame(&trade(t3), t3);
        full.end(t3);
        drop(full);
        let saved = disk::bars::load(&dir.bars_path(), 60_000).unwrap();
        assert!(
            bars_before.iter().all(|b| saved.contains(b)),
            "the downloaded day's bars are kept"
        );
        let cov = dir.coverage();
        for &(a, b) in coverage_before.trades.ranges() {
            assert!(cov.trades.holes(a, b).is_empty(), "trade claims kept");
        }
        for &(a, b) in coverage_before.bars.ranges() {
            assert!(cov.bars.holes(a, b).is_empty(), "bar claims kept");
        }
    }
}
