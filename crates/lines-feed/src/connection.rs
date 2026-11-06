//! Generic async driver: runs one connection (any [`Transport`]) through any
//! [`ProviderSession`]. All I/O lives here; sessions stay pure.

use std::time::Duration;

use anyhow::Result;
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};

use lines_core::UtcMillis;

use crate::provider::{MarketEvent, Outgoing, ProviderSession, SessionError, StartContext};
use crate::store::{ChartStore, FeedStatus};
use crate::transport::Transport;

#[derive(Debug)]
pub enum ConnectionEnd {
    /// Server closed the stream / replay reached its end.
    Closed,
    Shutdown,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("no data from server for {0:?}")]
    Idle(Duration),
    #[error(transparent)]
    Transport(#[from] anyhow::Error),
}

impl ConnectionError {
    pub fn is_retryable(&self) -> bool {
        match self {
            ConnectionError::Session(e) => e.is_retryable(),
            _ => true,
        }
    }
}

pub struct ConnectionOptions {
    /// Client keep-alive interval (only if the session has a keep-alive).
    pub ping_every: Duration,
    /// Treat the connection as dead if nothing arrives for this long.
    pub idle_timeout: Duration,
    /// History to request when none is stored.
    pub history_ms: i64,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        ConnectionOptions {
            ping_every: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
            history_ms: 3 * 24 * 60 * 60_000,
        }
    }
}

async fn flush<T: Transport>(t: &mut T, out: &mut Vec<Outgoing>) -> Result<()> {
    for o in out.drain(..) {
        match o {
            Outgoing::Frame(f) => t.send(f).await?,
            Outgoing::HttpGet(url) => t.request_http(url),
        }
    }
    Ok(())
}

/// Receives every decoded batch of a connection, after the store applied it (e.g. the local
/// store's writer, `crate::disk`). Runs on the feed task: it must not block.
pub trait EventSink: Send {
    /// A connection starts. `newest_bar` is the newest bar in the chart store and
    /// `window_start` the oldest time the history window asks for; returns where history
    /// should start (by default, the newest bar: it may be partial).
    fn begin(
        &mut self,
        newest_bar: Option<UtcMillis>,
        window_start: UtcMillis,
    ) -> Option<UtcMillis> {
        let _ = window_start;
        newest_bar
    }
    /// One decoded frame's events (an atomic batch), at feed time `now`.
    fn frame(&mut self, events: &[MarketEvent], now: UtcMillis);
    /// The connection ended (closed, failed or shut down).
    fn end(&mut self, now: UtcMillis) {
        let _ = now;
    }
}

/// A sink that ignores everything.
pub struct NoSink;

impl EventSink for NoSink {
    fn frame(&mut self, _: &[MarketEvent], _: UtcMillis) {}
}

/// Run one connection until it ends. Bars accumulate in `store` and survive the return.
pub async fn run_connection<T: Transport>(
    transport: &mut T,
    session: &mut dyn ProviderSession,
    store: &ChartStore,
    opts: &ConnectionOptions,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<ConnectionEnd, ConnectionError> {
    run_connection_with(transport, session, store, opts, shutdown, &mut NoSink).await
}

/// [`run_connection`], handing every batch to `sink` as well.
pub async fn run_connection_with<T: Transport>(
    transport: &mut T,
    session: &mut dyn ProviderSession,
    store: &ChartStore,
    opts: &ConnectionOptions,
    shutdown: &mut watch::Receiver<bool>,
    sink: &mut dyn EventSink,
) -> Result<ConnectionEnd, ConnectionError> {
    let now = transport.clock().unwrap_or_else(crate::now_utc);
    let result = drive(transport, session, store, opts, shutdown, sink, now).await;
    sink.end(transport.clock().unwrap_or_else(crate::now_utc));
    result
}

async fn drive<T: Transport>(
    transport: &mut T,
    session: &mut dyn ProviderSession,
    store: &ChartStore,
    opts: &ConnectionOptions,
    shutdown: &mut watch::Receiver<bool>,
    sink: &mut dyn EventSink,
    now: UtcMillis,
) -> Result<ConnectionEnd, ConnectionError> {
    // Re-request from the newest stored bar (it may be partial); everything before it is
    // complete. The store marks history as loading for this connection.
    let (newest_bar, period) = store.update(|d| {
        d.begin_connection();
        d.status = FeedStatus::LoggingOn;
        (d.series.last().map(|b| b.time), d.series.period_ms())
    });
    let window_start = UtcMillis(now.0 - opts.history_ms).floor_to(period);
    let history_from = sink.begin(newest_bar, window_start);
    let mut out: Vec<Outgoing> = Vec::new();
    let mut events: Vec<MarketEvent> = Vec::new();
    session.start(
        StartContext {
            now,
            history_from,
            bar_period_ms: period,
            history_ms: opts.history_ms,
        },
        &mut out,
    );
    flush(transport, &mut out).await?;

    let mut ping = tokio::time::interval(opts.ping_every);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ping.tick().await; // first tick is immediate
    let mut last_rx = Instant::now();

    loop {
        let idle_deadline = last_rx + opts.idle_timeout;
        tokio::select! {
            biased;
            _ = shutdown.changed() => return Ok(ConnectionEnd::Shutdown),
            frame = transport.recv() => {
                let Some(frame) = frame? else { return Ok(ConnectionEnd::Closed) };
                last_rx = Instant::now();
                // Replay supplies the recorded time, so sessions see a frozen, deterministic clock.
                let now = transport.clock().unwrap_or_else(crate::now_utc);
                events.clear();
                // Decode + convert outside the store lock.
                let result = session.on_frame(&frame, now, &mut events, &mut out);
                let len = frame.data.len() as u64;
                store.set_frame_time(now);
                store.update_frame(|d| {
                    d.stats.frames_in += 1;
                    d.stats.bytes_in += len;
                    if result.is_err() {
                        d.stats.decode_errors += 1;
                    }
                    d.apply(&events);
                });
                sink.frame(&events, now);
                result?;
                flush(transport, &mut out).await?;
            }
            _ = ping.tick() => {
                if let Some(p) = session.keepalive(crate::now_utc()) {
                    out.push(p);
                    flush(transport, &mut out).await?;
                }
            }
            _ = tokio::time::sleep_until(idle_deadline) => {
                return Err(ConnectionError::Idle(opts.idle_timeout));
            }
        }
    }
}

/// Exponential backoff with jitter: 0.5 s, 1 s, 2 s … capped at 30 s.
pub fn backoff(attempt: u32) -> Duration {
    let base_ms = 500u64.saturating_mul(1u64 << attempt.min(6)).min(30_000);
    // Cheap jitter (±20%) without a RNG dependency.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let jitter = base_ms / 5;
    Duration::from_millis(base_ms - jitter + nanos % (2 * jitter + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert!(backoff(0) <= Duration::from_millis(600));
        assert!(backoff(0) >= Duration::from_millis(400));
        assert!(backoff(3) >= Duration::from_millis(3200));
        assert!(backoff(30) <= Duration::from_millis(36_000));
    }
}
