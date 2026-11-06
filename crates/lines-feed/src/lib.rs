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
pub mod native;
pub mod provider;
pub mod recording;
pub mod store;
pub mod transport;

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use lines_core::{CivilDate, ContractKey, Product, UtcMillis};
use parking_lot::Mutex;
use tokio::sync::watch;

use connection::{ConnectionEnd, ConnectionError, ConnectionOptions, run_connection};
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
    pub max_bars: usize,
}

impl FeedConfig {
    pub fn new(source: Source) -> Self {
        FeedConfig {
            source,
            target: None,
            record: None,
            bar_period_ms: UtcMillis::MINUTE,
            max_bars: 50_000,
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
        Source::Replay { path, .. } => {
            let h = transport::read_header(path)?;
            match h.provider.as_str() {
                provider::CQG if cqg::protocol_compatible(&h.protocol) => {
                    Arc::new(cqg::CqgProvider::new(cqg::LogonParams::default()))
                }
                provider::COINBASE if coinbase::protocol_compatible(&h.protocol) => {
                    Arc::new(coinbase::CoinbaseProvider)
                }
                native::PROVIDER if native::protocol_compatible(&h.protocol) => {
                    Arc::new(native::NativeProvider)
                }
                p => bail!(
                    "recording is from provider {p:?} protocol {:?}, which this build cannot decode",
                    h.protocol
                ),
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
    Ok(Arc::new(ChartStore::new(
        p.product,
        p.contract,
        p.provider.capabilities(),
        p.provider.id(),
        config.bar_period_ms,
        config.max_bars,
    )))
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
    let mock_market = match &config.source {
        Source::CqgMock(m) => Some(Arc::new(Mutex::new(MockMarket::new(m, now_utc())))),
        _ => None,
    };
    let is_replay = matches!(config.source, Source::Replay { .. });
    let opts = if is_replay {
        // Replay is driven by recorded frames only: no idle timeout, no keep-alives.
        ConnectionOptions {
            idle_timeout: Duration::from_secs(365 * 86_400),
            ping_every: Duration::from_secs(86_400),
        }
    } else {
        ConnectionOptions::default()
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
        let current = store.read().contract;
        let contract = if plan.follow_front {
            let next = front_month(&plan.product, current).or(current);
            if next != current
                && let Some(k) = next
            {
                tracing::info!("front month rolled to {}", k.label(&plan.product));
                store.update(|d| d.switch_contract(k));
                // Recordings hold exactly one contract: stop rather than mix two.
                if let Some(r) = recorder.take() {
                    tracing::warn!("recording stopped at the roll (recordings hold one contract)");
                    if let Err(e) = r.finish() {
                        tracing::error!("recording incomplete: {e}");
                    }
                }
            }
            next
        } else {
            current
        };
        let mut session = match plan.provider.new_session(&plan.product, contract) {
            Ok(s) => s,
            Err(e) => {
                store.set_status(FeedStatus::Failed(e.to_string()));
                break;
            }
        };
        let session = session.as_mut();
        let rec = recorder.as_ref();

        let result = match &config.source {
            Source::CqgMock(m) => {
                let t =
                    MockTransport::connect(m.clone(), mock_market.clone().expect("mock market"));
                drive(t, rec, session, &store, &opts, &mut shutdown).await
            }
            Source::Replay {
                path,
                speed,
                stop_at,
            } => match ReplayTransport::open(path, *speed, *stop_at) {
                Ok(t) => drive(t, rec, session, &store, &opts, &mut shutdown).await,
                Err(e) => Err(ConnectionError::Transport(e)),
            },
            #[cfg(feature = "vendor-cqg")]
            Source::Cqg { url, .. } => {
                match WsTransport::connect(url, Duration::from_secs(15)).await {
                    Ok(t) => drive(t, rec, session, &store, &opts, &mut shutdown).await,
                    Err(e) => Err(ConnectionError::Transport(e)),
                }
            }
            Source::Coinbase { url } => {
                match WsTransport::connect(url, Duration::from_secs(15)).await {
                    Ok(t) => drive(t, rec, session, &store, &opts, &mut shutdown).await,
                    Err(e) => Err(ConnectionError::Transport(e)),
                }
            }
        };

        match result {
            Ok(ConnectionEnd::Shutdown) => break,
            Ok(ConnectionEnd::Closed) if is_replay => {
                store.set_status(FeedStatus::ReplayFinished);
                break;
            }
            Err(e) if !e.is_retryable() || is_replay => {
                tracing::error!("feed stopped: {e:#}");
                store.set_status(FeedStatus::Failed(e.to_string()));
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
    if let Some(r) = recorder
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

async fn drive<T: Transport>(
    t: T,
    recorder: Option<&Recorder>,
    session: &mut dyn provider::ProviderSession,
    store: &ChartStore,
    opts: &ConnectionOptions,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<ConnectionEnd, ConnectionError> {
    match recorder {
        Some(r) => run_connection(&mut Recording::new(t, r), session, store, opts, shutdown).await,
        None => {
            let mut t = t;
            run_connection(&mut t, session, store, opts, shutdown).await
        }
    }
}
