//! Async driver: runs one connection (any [`Transport`]) through a [`Session`], and the
//! reconnect loop around it.

use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use cqg_proto::web_api_2::{ClientMsg, ServerMsg};
use prost::Message;
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};

use crate::session::{Session, SessionError};
use crate::store::{ChartStore, FeedStatus};
use crate::transport::Transport;

#[derive(Debug)]
pub enum ConnectionEnd {
    /// Server closed the stream / replay reached end of file.
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
    /// Client keep-alive ping interval.
    pub ping_every: Duration,
    /// Treat the connection as dead if nothing arrives for this long.
    pub idle_timeout: Duration,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        ConnectionOptions {
            ping_every: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
        }
    }
}

async fn send<T: Transport>(t: &mut T, msg: &ClientMsg) -> Result<()> {
    t.send(Bytes::from(msg.encode_to_vec())).await
}

/// Run one connection until it ends. Bars accumulate in `store` and survive the return.
pub async fn run_connection<T: Transport>(
    transport: &mut T,
    session: &mut Session,
    store: &ChartStore,
    opts: &ConnectionOptions,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<ConnectionEnd, ConnectionError> {
    store.set_status(FeedStatus::LoggingOn);
    send(transport, &session.logon_msg()).await?;

    let mut ping = tokio::time::interval(opts.ping_every);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ping.tick().await; // first tick is immediate
    let mut last_rx = Instant::now();
    let mut out: Vec<ClientMsg> = Vec::new();

    loop {
        let idle_deadline = last_rx + opts.idle_timeout;
        tokio::select! {
            biased;
            _ = shutdown.changed() => return Ok(ConnectionEnd::Shutdown),
            frame = transport.recv() => {
                let Some(frame) = frame? else { return Ok(ConnectionEnd::Closed) };
                last_rx = Instant::now();
                let len = frame.len() as u64;
                // Decode outside the store lock.
                let decoded = ServerMsg::decode(frame);
                let now = crate::now_utc();
                let result = store.update(|d| {
                    d.stats.frames_in += 1;
                    d.stats.bytes_in += len;
                    match &decoded {
                        Ok(msg) => session.on_server_msg(msg, now, d, &mut out),
                        Err(_) => { d.stats.decode_errors += 1; Ok(()) }
                    }
                });
                if let Err(e) = &decoded {
                    tracing::warn!("undecodable server frame ({len} bytes): {e}");
                }
                result?;
                for msg in out.drain(..) {
                    send(transport, &msg).await?;
                }
            }
            _ = ping.tick() => {
                if let Some(p) = session.ping_msg(crate::now_utc()) {
                    send(transport, &p).await?;
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
