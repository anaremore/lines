//! Frame transports. Sessions only see [`Frame`]s, so the real WebSocket, the in-process
//! CQG mock, and file replay are interchangeable for every provider.
//!
//! `recv` implementations must be **cancel-safe**: the connection loop races `recv` against
//! timers, so no frame may be lost if the future is dropped mid-await.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use lines_core::UtcMillis;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::provider::{Frame, FrameKind};
use crate::recording::{Direction, Header, Reader, Recorder};

pub trait Transport: Send {
    fn send(&mut self, frame: Frame) -> impl Future<Output = Result<()>> + Send;
    /// Next inbound frame; `Ok(None)` = clean end of stream.
    fn recv(&mut self) -> impl Future<Output = Result<Option<Frame>>> + Send;
    /// Start an HTTP GET. The body is delivered later by `recv` as a [`FrameKind::Http`]
    /// frame, so it is recorded and replayed in order with everything else.
    fn request_http(&mut self, url: String) {
        tracing::warn!("transport cannot fetch {url}");
    }
    /// Recorded time of the most recent frame (replay's frozen clock); `None` = live.
    fn clock(&self) -> Option<UtcMillis> {
        None
    }
}

// ---------------------------------------------------------------------------------------
// Real WebSocket (+ HTTP GETs on the side).

pub struct WsTransport {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    http_tx: mpsc::UnboundedSender<Result<Frame, String>>,
    http_rx: mpsc::UnboundedReceiver<Result<Frame, String>>,
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
            return Err(anyhow!("only wss:// is supported (got {url})"));
        }
        let connector = Connector::Rustls(Arc::new(tls_config()?));
        let (ws, _resp) = tokio::time::timeout(
            timeout,
            tokio_tungstenite::connect_async_tls_with_config(url, None, true, Some(connector)),
        )
        .await
        .map_err(|_| anyhow!("connect to {url} timed out"))?
        .with_context(|| format!("connect to {url}"))?;
        let (http_tx, http_rx) = mpsc::unbounded_channel();
        Ok(WsTransport {
            ws,
            http_tx,
            http_rx,
        })
    }
}

fn http_get(url: &str) -> Result<Vec<u8>, String> {
    // ureq: rustls with the `ring` provider; blocking, so it runs on spawn_blocking.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent(concat!("lines/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?;
    resp.body_mut()
        .read_to_vec()
        .map_err(|e| format!("GET {url}: {e}"))
}

impl Transport for WsTransport {
    async fn send(&mut self, frame: Frame) -> Result<()> {
        let msg = match frame.kind {
            FrameKind::Binary => Message::Binary(frame.data),
            FrameKind::Text => Message::Text(
                String::from_utf8(frame.data.to_vec())
                    .map_err(|_| anyhow!("text frame is not UTF-8"))?
                    .into(),
            ),
            FrameKind::Http => return Err(anyhow!("cannot send an HTTP frame over a websocket")),
        };
        self.ws.send(msg).await.context("websocket send")
    }

    async fn recv(&mut self) -> Result<Option<Frame>> {
        // Both branches are cancel-safe (StreamExt::next, mpsc::recv).
        loop {
            tokio::select! {
                http = self.http_rx.recv() => match http {
                    Some(Ok(frame)) => return Ok(Some(frame)),
                    Some(Err(e)) => return Err(anyhow!(e)),
                    None => unreachable!("we hold a sender"),
                },
                msg = self.ws.next() => match msg {
                    None => return Ok(None),
                    Some(msg) => match msg.context("websocket recv")? {
                        Message::Binary(b) => return Ok(Some(Frame::binary(b))),
                        Message::Text(t) => return Ok(Some(Frame { kind: FrameKind::Text, data: t.into() })),
                        Message::Close(_) => return Ok(None),
                        // tungstenite answers pings itself.
                        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                    },
                },
            }
        }
    }

    fn request_http(&mut self, url: String) {
        let tx = self.http_tx.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(http_get(&url).map(Frame::http));
        });
    }
}

// ---------------------------------------------------------------------------------------
// Recording wrapper: tees every frame (both directions, incl. HTTP bodies) to a `.lfr` file.

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
    async fn send(&mut self, frame: Frame) -> Result<()> {
        self.recorder.record(Direction::ToServer, &frame);
        self.inner.send(frame).await
    }

    async fn recv(&mut self) -> Result<Option<Frame>> {
        let r = self.inner.recv().await?;
        if let Some(f) = &r {
            self.recorder.record(Direction::FromServer, f);
        }
        Ok(r)
    }

    fn request_http(&mut self, url: String) {
        self.inner.request_http(url);
    }

    fn clock(&self) -> Option<UtcMillis> {
        self.inner.clock()
    }
}

// ---------------------------------------------------------------------------------------
// Replay: yields recorded inbound frames with their original timing (scaled by `speed`).

pub struct ReplayTransport {
    reader: Reader<std::io::BufReader<std::fs::File>>,
    /// `0` or negative = as fast as possible.
    speed: f64,
    /// Stop (end of stream) before the first frame recorded after this time.
    stop_at: Option<UtcMillis>,
    started: Instant,
    /// Read but not yet delivered (keeps `recv` cancel-safe across the timing sleep).
    pending: Option<(Duration, i64, Frame)>,
    clock: Option<UtcMillis>,
}

impl ReplayTransport {
    pub fn open(path: &Path, speed: f64, stop_at: Option<UtcMillis>) -> Result<Self> {
        let reader =
            Reader::open(path).with_context(|| format!("open recording {}", path.display()))?;
        let mut reader = reader;
        // Before any frame is delivered, the clock reads the time of the first recorded frame
        // (normally the session's own logon/subscribe, i.e. when start() ran live), so
        // sessions computing time windows in start() behave exactly as they did live.
        let mut clock = Some(UtcMillis(reader.header.start_wall_ms));
        let mut pending = None;
        if let Some(first) = reader.next_record()? {
            let wall = first.wall_ms(&reader.header);
            clock = Some(UtcMillis(wall));
            if first.direction == Direction::FromServer && stop_at.is_none_or(|t| wall <= t.0) {
                pending = Some((first.since_start, wall, first.frame));
            }
        }
        Ok(ReplayTransport {
            reader,
            speed,
            stop_at,
            started: Instant::now(),
            pending,
            clock,
        })
    }

    pub fn header(&self) -> &Header {
        &self.reader.header
    }
}

/// Read only the header of a recording (to pick the provider and instrument).
pub fn read_header(path: &Path) -> Result<Header> {
    Ok(Reader::open(path)
        .with_context(|| format!("open recording {}", path.display()))?
        .header)
}

impl Transport for ReplayTransport {
    async fn send(&mut self, _frame: Frame) -> Result<()> {
        Ok(()) // Nobody is listening; the recorded server replies drive the session.
    }

    async fn recv(&mut self) -> Result<Option<Frame>> {
        if self.pending.is_none() {
            loop {
                match self.reader.next_record()? {
                    None => return Ok(None),
                    Some(r) if r.direction == Direction::FromServer => {
                        let wall = r.wall_ms(&self.reader.header);
                        if self.stop_at.is_some_and(|t| wall > t.0) {
                            return Ok(None);
                        }
                        self.pending = Some((r.since_start, wall, r.frame));
                        break;
                    }
                    Some(_) => continue,
                }
            }
        }
        if self.speed > 0.0 {
            let (at, _, _) = self.pending.as_ref().expect("set above");
            tokio::time::sleep_until(self.started + at.div_f64(self.speed)).await;
        }
        Ok(self.pending.take().map(|(_, wall, f)| {
            self.clock = Some(UtcMillis(wall));
            f
        }))
    }

    fn request_http(&mut self, _url: String) {
        // The recorded response is already in the stream.
    }

    fn clock(&self) -> Option<UtcMillis> {
        self.clock
    }
}
