//! Market-data feed for Lines: provider abstraction, CQG (mock + WebAPI), Coinbase public
//! feed, recording and replay.
//!
//! The feed runs on its own OS thread with its own Tokio runtime (decision 0001). It never
//! touches the UI; it publishes into a shared [`ChartStore`] that the UI samples per frame.
//! Above this crate only catalog instruments are visible, never vendor symbols (0003).

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
use lines_core::{Instrument, UtcMillis};
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
    /// Real CQG WebAPI. Untested until credentials exist.
    Cqg {
        url: String,
        logon: cqg::LogonParams,
    },
    /// Coinbase Exchange public feed.
    Coinbase { url: String },
    /// Replay a `.lfr` recording (provider and instrument come from its header).
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
    /// Catalog instrument; `None` = the source's default (or the recording's).
    pub instrument: Option<Instrument>,
    /// Tee all raw frames to this `.lfr` file.
    pub record: Option<PathBuf>,
    pub bar_period_ms: i64,
    pub max_bars: usize,
}

impl FeedConfig {
    pub fn new(source: Source) -> Self {
        FeedConfig {
            source,
            instrument: None,
            record: None,
            bar_period_ms: UtcMillis::MINUTE,
            max_bars: 50_000,
        }
    }
}

/// A validated plan: which provider serves which instrument.
struct Plan {
    provider: Arc<dyn Provider>,
    instrument: Instrument,
}

fn provider_for(source: &Source) -> Result<Arc<dyn Provider>> {
    Ok(match source {
        Source::CqgMock(_) => Arc::new(cqg::CqgProvider::new(cqg::LogonParams {
            client_version: client_version(),
            ..Default::default()
        })),
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
    let provider = provider_for(&config.source)?;
    let instrument = match (&config.source, &config.instrument) {
        (Source::Replay { path, .. }, requested) => {
            let h = transport::read_header(path)?;
            let recorded = catalog::by_symbol(&h.instrument)
                .ok_or_else(|| anyhow!("recording is for unknown instrument {:?}", h.instrument))?;
            if let Some(r) = requested
                && r.id != recorded.id
            {
                bail!("recording is for {}, not {}", recorded.symbol, r.symbol);
            }
            recorded
        }
        (_, Some(i)) => i.clone(),
        (Source::Coinbase { .. }, None) => catalog::get(catalog::BTC_USD).expect("catalog"),
        (_, None) => catalog::get(catalog::ES).expect("catalog"),
    };
    if !provider.supports(&instrument) {
        bail!("{} does not provide {}", provider.id(), instrument.symbol);
    }
    Ok(Plan {
        provider,
        instrument,
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

/// Create the store for a config (validates provider/instrument up front).
pub fn new_store(config: &FeedConfig) -> Result<Arc<ChartStore>> {
    let p = plan(config)?;
    Ok(Arc::new(ChartStore::new(
        p.instrument,
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
    let recorder = match &config.record {
        Some(path) => match Recorder::create(
            path,
            plan.provider.id(),
            &plan.provider.protocol(),
            plan.instrument.symbol,
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
        let mut session = match plan.provider.new_session(&plan.instrument) {
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
            Source::Cqg { url, .. } | Source::Coinbase { url } => {
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
