//! Market-data feed for Lines: CQG WebAPI session, mock server, recording and replay.
//!
//! The feed runs on its own OS thread with its own Tokio runtime. It never touches the UI;
//! it publishes into a shared [`ChartStore`] that the UI samples once per frame.

pub mod connection;
pub mod mock;
pub mod recording;
pub mod session;
pub mod store;
pub mod transport;

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lines_core::UtcMillis;
use parking_lot::Mutex;
use tokio::sync::watch;

use connection::{ConnectionEnd, ConnectionOptions, run_connection};
use mock::{MockConfig, MockMarket, MockTransport};
use recording::Recorder;
use session::{LogonParams, Session, SessionConfig};
use store::{ChartStore, FeedStatus};
use transport::{Recording, ReplayTransport, Transport, WsTransport};

pub use store::{ChartData, FeedStats, InstrumentInfo};

pub fn now_utc() -> UtcMillis {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    UtcMillis(d.as_millis() as i64)
}

#[derive(Clone, Debug)]
pub enum Source {
    Mock(MockConfig),
    /// Replay a `.lfr` recording. `speed` 1.0 = real time, 0 = as fast as possible.
    Replay {
        path: PathBuf,
        speed: f64,
    },
    /// Real CQG WebAPI. Untested until credentials exist (see CLAUDE.md).
    Cqg {
        url: String,
        logon: LogonParams,
    },
}

#[derive(Clone, Debug)]
pub struct FeedConfig {
    pub source: Source,
    pub symbol: String,
    /// Tee all raw frames to this `.lfr` file.
    pub record: Option<PathBuf>,
    pub history_ms: i64,
    pub max_bars: usize,
}

impl FeedConfig {
    pub fn new(source: Source) -> Self {
        FeedConfig {
            source,
            symbol: "EP".into(),
            record: None,
            history_ms: 3 * 24 * 60 * UtcMillis::MINUTE,
            max_bars: 50_000,
        }
    }
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

/// Start the feed on a dedicated thread.
pub fn spawn(config: FeedConfig) -> std::io::Result<FeedHandle> {
    let store = Arc::new(ChartStore::new(UtcMillis::MINUTE, config.max_bars));
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
        })?;
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
    let recorder = match &config.record {
        Some(path) => match Recorder::create(
            path,
            cqg_proto::PROTOCOL_VERSION_MAJOR,
            cqg_proto::PROTOCOL_VERSION_MINOR,
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
    let session_cfg = SessionConfig {
        symbol: config.symbol.clone(),
        logon: match &config.source {
            Source::Cqg { logon, .. } => logon.clone(),
            _ => LogonParams {
                client_version: client_version(),
                ..Default::default()
            },
        },
        history_ms: config.history_ms,
    };
    let mock_market = match &config.source {
        Source::Mock(m) => Some(Arc::new(Mutex::new(MockMarket::new(m, now_utc())))),
        _ => None,
    };
    let opts = ConnectionOptions::default();
    let mut attempt: u32 = 0;

    loop {
        if *shutdown.borrow() {
            break;
        }
        store.set_status(FeedStatus::Connecting);
        let mut session = Session::new(session_cfg.clone());
        let started = tokio::time::Instant::now();

        let result = match &config.source {
            Source::Mock(m) => {
                let t =
                    MockTransport::connect(m.clone(), mock_market.clone().expect("mock market"));
                drive(
                    t,
                    recorder.as_ref(),
                    &mut session,
                    &store,
                    &opts,
                    &mut shutdown,
                )
                .await
            }
            Source::Replay { path, speed } => match ReplayTransport::open(path, *speed) {
                // Replay is driven by recorded frames only; no idle timeout or reconnects.
                Ok(t) => {
                    let opts = ConnectionOptions {
                        idle_timeout: Duration::from_secs(365 * 86_400),
                        ..Default::default()
                    };
                    drive(
                        t,
                        recorder.as_ref(),
                        &mut session,
                        &store,
                        &opts,
                        &mut shutdown,
                    )
                    .await
                }
                Err(e) => Err(connection::ConnectionError::Transport(e)),
            },
            Source::Cqg { url, .. } => {
                match WsTransport::connect(url, Duration::from_secs(15)).await {
                    Ok(t) => {
                        drive(
                            t,
                            recorder.as_ref(),
                            &mut session,
                            &store,
                            &opts,
                            &mut shutdown,
                        )
                        .await
                    }
                    Err(e) => Err(connection::ConnectionError::Transport(e)),
                }
            }
        };

        let is_replay = matches!(config.source, Source::Replay { .. });
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
    session: &mut Session,
    store: &ChartStore,
    opts: &ConnectionOptions,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<ConnectionEnd, connection::ConnectionError> {
    match recorder {
        Some(r) => run_connection(&mut Recording::new(t, r), session, store, opts, shutdown).await,
        None => {
            let mut t = t;
            run_connection(&mut t, session, store, opts, shutdown).await
        }
    }
}

pub fn client_version() -> String {
    format!("lines {}", env!("CARGO_PKG_VERSION"))
}
