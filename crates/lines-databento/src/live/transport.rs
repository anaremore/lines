//! The live transport: one raw TCP connection to Databento's live gateway (no TLS, no
//! WebSocket: challenge-response authentication, so the key never crosses the network).
//!
//! - Inbound bytes are passed on as they arrive, one frame per read (the session buffers
//!   partial lines and records), so recordings hold exactly what the gateway sent.
//! - The CRAM challenge is noted on the way in; the session's auth line carries
//!   [`AUTH_PLACEHOLDER`] and only here, on the way out, is it replaced by
//!   `sha256_hex("<cram>|<key>")-<last 5 characters of the key>`. The recorder records frames
//!   before `send`, so recordings never hold the key, its hash or its bucket id.
//! - At most [`MAX_CONNECTIONS`] connections per process (Databento's Standard plan allows 10
//!   per dataset per team), and at least [`CONNECT_SPACING`] between connects (a gateway takes
//!   at most 5 new connections per second per IP).

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use lines_feed::provider::Frame;
use lines_feed::transport::Transport;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::session::{AUTH_PLACEHOLDER, CONNECTION_MARKER, SENT_PREFIX};
use crate::key::ApiKey;

pub const MAX_CONNECTIONS: usize = 8;
pub const CONNECT_SPACING: Duration = Duration::from_millis(250);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_CHUNK: usize = 64 * 1024;

static OPEN: AtomicUsize = AtomicUsize::new(0);
static LAST_CONNECT: Mutex<Option<Instant>> = Mutex::new(None);

/// One of the process's live connections.
struct Slot;

impl Slot {
    fn take() -> Result<Slot> {
        let mut n = OPEN.load(Ordering::Acquire);
        loop {
            if n >= MAX_CONNECTIONS {
                bail!(
                    "Databento connection limit: {MAX_CONNECTIONS} live connections are open in \
                     this app (one per contract shown)"
                );
            }
            match OPEN.compare_exchange(n, n + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(Slot),
                Err(now) => n = now,
            }
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        OPEN.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The key's bucket id: its last 5 characters (databento-rs `ApiKey::bucket_id`).
pub fn bucket_id(key: &str) -> &str {
    &key[key.len() - 5..]
}

/// The CRAM response for `challenge` (databento-rs `live/protocol.rs`).
pub fn cram_response(challenge: &str, key: &str) -> String {
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        format!("{challenge}|{key}").as_bytes(),
    );
    let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex}-{}", bucket_id(key))
}

/// An outgoing line, with the placeholder replaced by the CRAM response.
pub fn authorize(line: &[u8], challenge: Option<&str>, key: &str) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(line).context("outgoing line is not UTF-8")?;
    if !text.contains(AUTH_PLACEHOLDER) {
        return Ok(line.to_vec());
    }
    let Some(challenge) = challenge else {
        bail!("the gateway sent no challenge before the auth line");
    };
    Ok(text
        .replace(AUTH_PLACEHOLDER, &cram_response(challenge, key))
        .into_bytes())
}

/// The challenge in the gateway's greeting so far, if complete.
pub fn find_challenge(greeting: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(greeting);
    let mut lines = text.split('\n');
    let last = lines.next_back();
    lines
        .chain(last.filter(|_| greeting.ends_with(b"\n")))
        .find_map(|l| {
            l.trim_end_matches('\r')
                .strip_prefix("cram=")
                .map(String::from)
        })
}

pub struct LiveTransport {
    stream: TcpStream,
    key: ApiKey,
    challenge: Option<String>,
    greeting: Vec<u8>,
    buf: Vec<u8>,
    /// The connection marker is still to be delivered.
    marker: bool,
    /// Echoes of sent lines, delivered before the next socket read (recorded: the replay plan).
    echoes: std::collections::VecDeque<Frame>,
    _slot: Slot,
}

impl LiveTransport {
    /// Connect to `gateway` (`host:port`). Keys are 32 characters (Databento).
    pub async fn connect(gateway: &str, key: ApiKey) -> Result<Self> {
        if key.secret().len() != 32 {
            bail!(
                "the Databento API key should be 32 characters ({})",
                key.describe()
            );
        }
        let slot = Slot::take()?;
        let wait = {
            let mut last = LAST_CONNECT.lock().expect("connect spacing");
            let now = Instant::now();
            let at = last.map_or(now, |t| (t + CONNECT_SPACING).max(now));
            *last = Some(at);
            at - now
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        tracing::info!("Databento: connecting to {gateway}");
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(gateway))
            .await
            .with_context(|| format!("connecting to {gateway}: timed out"))?
            .with_context(|| format!("connecting to {gateway}"))?;
        stream.set_nodelay(true).ok();
        Ok(LiveTransport {
            stream,
            key,
            challenge: None,
            greeting: Vec::new(),
            buf: vec![0; READ_CHUNK],
            marker: true,
            echoes: std::collections::VecDeque::new(),
            _slot: slot,
        })
    }
}

impl Transport for LiveTransport {
    async fn send(&mut self, frame: Frame) -> Result<()> {
        let bytes = authorize(&frame.data, self.challenge.as_deref(), self.key.secret())?;
        self.stream.write_all(&bytes).await?;
        // Every line but the auth line comes back as a recorded echo (the session's plan).
        if !frame.data.starts_with(b"auth=") {
            self.echoes
                .push_back(Frame::binary([SENT_PREFIX, &frame.data[..]].concat()));
        }
        Ok(())
    }

    // Cancel-safe: `TcpStream::read` loses nothing when cancelled before it completes.
    async fn recv(&mut self) -> Result<Option<Frame>> {
        // First, the marker that starts a connection (recorded; see CONNECTION_MARKER).
        if self.marker {
            self.marker = false;
            return Ok(Some(Frame::binary(CONNECTION_MARKER.to_vec())));
        }
        if let Some(echo) = self.echoes.pop_front() {
            return Ok(Some(echo));
        }
        let n = self.stream.read(&mut self.buf).await?;
        if n == 0 {
            return Ok(None);
        }
        let data = self.buf[..n].to_vec();
        if self.challenge.is_none() && self.greeting.len() < 4096 {
            self.greeting.extend_from_slice(&data);
            self.challenge = find_challenge(&self.greeting);
        }
        Ok(Some(Frame::binary(data)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "db-0123456789abcdefghijklmnopqrs"; // 32 characters, made up

    #[test]
    fn the_placeholder_becomes_the_cram_response_only_on_the_way_out() {
        assert_eq!(KEY.len(), 32);
        let line = format!("auth={AUTH_PLACEHOLDER}|dataset=GLBX.MDP3|encoding=dbn\n");
        let out = authorize(line.as_bytes(), Some("abc"), KEY).unwrap();
        let out = String::from_utf8(out).unwrap();
        let resp = out
            .strip_prefix("auth=")
            .unwrap()
            .split('|')
            .next()
            .unwrap();
        let (hex, bucket) = resp.split_once('-').unwrap();
        assert_eq!(bucket, "opqrs", "the key's last 5 characters");
        assert_eq!(hex.len(), 64);
        let expect = ring::digest::digest(
            &ring::digest::SHA256,
            b"abc|db-0123456789abcdefghijklmnopqrs",
        );
        let expect: String = expect.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, expect);
        assert!(!out.contains(KEY), "the key itself never leaves");
        // Other lines pass unchanged; no challenge yet: refused.
        assert_eq!(
            authorize(b"start_session\n", None, KEY).unwrap(),
            b"start_session\n"
        );
        assert!(authorize(line.as_bytes(), None, KEY).is_err());
    }

    #[test]
    fn the_challenge_is_found_in_split_greetings() {
        assert_eq!(find_challenge(b"lsg_version=0.6.2\ncra"), None);
        assert_eq!(
            find_challenge(b"lsg_version=0.6.2\ncram=XYZ"),
            None,
            "not complete without its newline"
        );
        assert_eq!(
            find_challenge(b"lsg_version=0.6.2\ncram=XYZ\n").as_deref(),
            Some("XYZ")
        );
        assert_eq!(find_challenge(b"cram=Q\r\n").as_deref(), Some("Q"));
    }

    #[test]
    fn connections_are_capped() {
        let slots: Vec<Slot> = (0..MAX_CONNECTIONS)
            .map(|_| Slot::take().unwrap())
            .collect();
        assert!(Slot::take().is_err());
        drop(slots);
        assert!(Slot::take().is_ok());
    }
}
