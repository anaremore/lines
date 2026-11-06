//! Frame transports. The session only ever sees opaque binary frames, so the real CQG
//! WebSocket, the in-process mock, and file replay are interchangeable.
//!
//! `recv` implementations must be **cancel-safe**: the connection loop races `recv` against
//! timers, so no frame may be lost if the future is dropped mid-await.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::recording::{Direction, Reader, Recorder};

pub trait Transport: Send {
    fn send(&mut self, frame: Bytes) -> impl Future<Output = Result<()>> + Send;
    /// Next server frame; `Ok(None)` = clean end of stream.
    fn recv(&mut self) -> impl Future<Output = Result<Option<Bytes>>> + Send;
}

// ---------------------------------------------------------------------------------------
// Real WebSocket (CQG WebAPI only accepts wss://).

pub struct WsTransport {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

/// TLS config with the `ring` provider and the OS certificate store (so corporate
/// TLS-inspecting proxies with a locally-installed root keep working).
pub fn tls_config() -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for e in &native.errors {
        tracing::warn!("native cert store: {e}");
    }
    let (added, ignored) = roots.add_parsable_certificates(native.certs);
    tracing::debug!(added, ignored, "loaded OS root certificates");
    if roots.is_empty() {
        return Err(anyhow!("no usable root certificates found in the OS store"));
    }
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

impl WsTransport {
    pub async fn connect(url: &str, timeout: Duration) -> Result<Self> {
        if !url.starts_with("wss://") {
            return Err(anyhow!("CQG WebAPI requires wss:// (got {url})"));
        }
        let connector = Connector::Rustls(Arc::new(tls_config()?));
        let (ws, _resp) = tokio::time::timeout(
            timeout,
            tokio_tungstenite::connect_async_tls_with_config(url, None, true, Some(connector)),
        )
        .await
        .map_err(|_| anyhow!("connect to {url} timed out"))?
        .with_context(|| format!("connect to {url}"))?;
        Ok(WsTransport { ws })
    }
}

impl Transport for WsTransport {
    async fn send(&mut self, frame: Bytes) -> Result<()> {
        self.ws
            .send(Message::Binary(frame))
            .await
            .context("websocket send")
    }

    async fn recv(&mut self) -> Result<Option<Bytes>> {
        // StreamExt::next is cancel-safe.
        while let Some(msg) = self.ws.next().await {
            match msg.context("websocket recv")? {
                Message::Binary(b) => return Ok(Some(b)),
                Message::Close(_) => return Ok(None),
                Message::Text(t) => tracing::warn!("unexpected text frame ({} bytes)", t.len()),
                // tungstenite answers pings itself.
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------------------
// Recording wrapper: tees every frame (both directions) to a `.lfr` file.

pub struct Recording<'r, T> {
    inner: T,
    recorder: &'r Recorder,
}

impl<'r, T: Transport> Recording<'r, T> {
    pub fn new(inner: T, recorder: &'r Recorder) -> Self {
        Recording { inner, recorder }
    }
}

impl<T: Transport> Transport for Recording<'_, T> {
    async fn send(&mut self, frame: Bytes) -> Result<()> {
        self.recorder.record(Direction::ToServer, &frame);
        self.inner.send(frame).await
    }

    async fn recv(&mut self) -> Result<Option<Bytes>> {
        let r = self.inner.recv().await?;
        if let Some(f) = &r {
            self.recorder.record(Direction::FromServer, f);
        }
        Ok(r)
    }
}

// ---------------------------------------------------------------------------------------
// Replay: yields the recorded server frames with their original timing (scaled by `speed`).

pub struct ReplayTransport {
    reader: Reader<std::io::BufReader<std::fs::File>>,
    /// `0` or negative = as fast as possible.
    speed: f64,
    started: Instant,
    /// Read but not yet delivered (keeps `recv` cancel-safe across the timing sleep).
    pending: Option<(Duration, Bytes)>,
}

impl ReplayTransport {
    pub fn open(path: &Path, speed: f64) -> Result<Self> {
        let reader =
            Reader::open(path).with_context(|| format!("open recording {}", path.display()))?;
        let h = &reader.header;
        if h.proto_major != cqg_proto::PROTOCOL_VERSION_MAJOR {
            return Err(anyhow!(
                "recording uses CQG protocol {}.{}, this build decodes {}.{}",
                h.proto_major,
                h.proto_minor,
                cqg_proto::PROTOCOL_VERSION_MAJOR,
                cqg_proto::PROTOCOL_VERSION_MINOR
            ));
        }
        if h.proto_minor != cqg_proto::PROTOCOL_VERSION_MINOR {
            tracing::warn!(
                "recording minor protocol {} differs from build ({}); fields may be missing",
                h.proto_minor,
                cqg_proto::PROTOCOL_VERSION_MINOR
            );
        }
        Ok(ReplayTransport {
            reader,
            speed,
            started: Instant::now(),
            pending: None,
        })
    }
}

impl Transport for ReplayTransport {
    async fn send(&mut self, _frame: Bytes) -> Result<()> {
        Ok(()) // Nobody is listening; the recorded server replies drive the session.
    }

    async fn recv(&mut self) -> Result<Option<Bytes>> {
        if self.pending.is_none() {
            loop {
                match self.reader.next_record()? {
                    None => return Ok(None),
                    Some(r) if r.direction == Direction::FromServer => {
                        self.pending = Some((r.since_start, r.payload));
                        break;
                    }
                    Some(_) => continue,
                }
            }
        }
        if self.speed > 0.0 {
            let (at, _) = self.pending.as_ref().expect("set above");
            let due = self.started + at.div_f64(self.speed);
            tokio::time::sleep_until(due).await;
        }
        Ok(self.pending.take().map(|(_, b)| b))
    }
}
