//! The provider abstraction (roadmap milestone 3).
//!
//! A provider is split in two:
//! - [`Provider`]: static facts (id, capabilities, which instruments it maps) and a factory
//!   for per-connection sessions. Vendor symbol mapping lives behind this trait (decision
//!   0003), so nothing above `lines-feed` sees vendor symbols.
//! - [`ProviderSession`]: I/O-free protocol logic for one connection. It turns wire frames
//!   into normalized [`MarketEvent`]s in *catalog units* (decision 0002) and says what to
//!   send. The generic driver (`connection.rs`) owns all I/O, so live, mock and replay run
//!   the identical session code.

use bytes::Bytes;
use lines_core::{Aggressor, Bar, Capabilities, ContractKey, Price, Product, Qty, Side, UtcMillis};

/// Wire frame kinds. HTTP responses (e.g. REST history) travel through the same pipe so
/// recordings capture them and replays are deterministic.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum FrameKind {
    Binary,
    Text,
    Http,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub data: Bytes,
}

impl Frame {
    pub fn binary(data: impl Into<Bytes>) -> Self {
        Frame {
            kind: FrameKind::Binary,
            data: data.into(),
        }
    }
    pub fn text(data: impl Into<String>) -> Self {
        Frame {
            kind: FrameKind::Text,
            data: Bytes::from(data.into()),
        }
    }
    pub fn http(data: impl Into<Bytes>) -> Self {
        Frame {
            kind: FrameKind::Http,
            data: data.into(),
        }
    }
}

/// Split an HTTP frame into its request URL and body. Frames carry `URL\nbody`; frames from
/// older recordings carry only the body (`None` URL).
pub fn split_http(data: &[u8]) -> (Option<&str>, &[u8]) {
    let first = data
        .iter()
        .position(|&b| b == b'\n')
        .map_or(data, |i| &data[..i]);
    match std::str::from_utf8(first) {
        Ok(url) if url.starts_with("http://") || url.starts_with("https://") => {
            (Some(url), &data[first.len() + 1..])
        }
        _ => (None, data),
    }
}

/// One query parameter's value from a URL (no decoding; Lines' own URLs need none).
pub fn query_param<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    let query = url.split_once('?')?.1;
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

/// Something a session wants the driver to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outgoing {
    Frame(Frame),
    /// GET this URL; the body comes back later as a [`FrameKind::Http`] frame.
    HttpGet(String),
}

/// Normalized market data, already converted into catalog units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarketEvent {
    /// Subscribed and streaming.
    Ready,
    /// Data from here on belongs to this contract (e.g. after a front-month roll). The store
    /// retires the previous contract's bars rather than mixing them (decision 0013).
    ContractChanged(ContractKey),
    /// A history snapshot (initial back-fill, reconnect back-fill, or a server bar update):
    /// authoritative bars that include exactly the trades with `time <= as_of` (closed;
    /// `None` = complete bars only, closed at the end of the last one). Stitched with live
    /// trades by `lines_core::stitch`.
    History {
        bars: Vec<Bar>,
        as_of: Option<UtcMillis>,
    },
    /// The provider has delivered all history requested on this connection.
    HistoryComplete,
    /// History known to be missing between these times (shown as missing, never invented).
    HistoryGap {
        from: UtcMillis,
        to: UtcMillis,
    },
    Trade {
        time: UtcMillis,
        price: Price,
        qty: Qty,
        /// Who initiated, if the source says (time & sales).
        aggressor: Option<Aggressor>,
    },
    /// Most recent trade price from a snapshot: updates "last", not volume.
    LastPrice(Price),
    Bid(Price),
    Ask(Price),
    /// Remove every depth level (start of a book snapshot).
    BookClear,
    /// Set one aggregated depth level; `qty == 0` removes it. All book events in one frame
    /// form one atomic batch: the book is only consistent at frame boundaries.
    BookLevel {
        side: Side,
        price: Price,
        qty: Qty,
        orders: u32,
    },
    /// The rebuilt book matched the exchange's checksum after this message (Kraken spot).
    BookChecksumOk,
    /// The rebuilt book did *not* match the exchange's checksum. The session follows it with
    /// `BookClear` and `BookResync`: a book known to be wrong is never shown.
    BookChecksumMismatch,
    /// The session dropped its book and asked the exchange for a fresh snapshot (checksum
    /// mismatch or a book sequence gap).
    BookResync,
    /// Messages known to be missing (sequence gap).
    Gap {
        missed: u64,
    },
    /// Vendor values that could not be represented exactly and were dropped.
    Rejected {
        count: u64,
    },
    /// Trade corrections the provider sent but we do not apply yet.
    CorrectionsIgnored {
        count: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// Credentials/entitlement problems: retrying won't help.
    #[error("logon rejected (code {code}): {text}")]
    LogonRejected { code: u32, text: String },
    #[error("server logged us off: {0}")]
    LoggedOff(String),
    #[error("instrument unavailable: {0}")]
    Unavailable(String),
    /// Vendor metadata disagrees with our catalog (decision 0002: adapters validate scales).
    #[error("instrument metadata mismatch: {0}")]
    BadMetadata(String),
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl SessionError {
    /// Whether reconnecting could plausibly fix this.
    pub fn is_retryable(&self) -> bool {
        matches!(self, SessionError::LoggedOff(_) | SessionError::Protocol(_))
    }
}

/// Context the driver gives a session when a connection starts.
#[derive(Clone, Copy, Debug)]
pub struct StartContext {
    pub now: UtcMillis,
    /// Start of the newest bar we already hold (re-request it; it may be incomplete).
    /// `None` = no bars yet, request `history_ms` of history.
    pub history_from: Option<UtcMillis>,
    pub bar_period_ms: i64,
    /// How much history to request when none is stored (bounded, configurable).
    pub history_ms: i64,
}

pub trait ProviderSession: Send {
    /// Initial messages (logon, subscribe, history request).
    fn start(&mut self, ctx: StartContext, out: &mut Vec<Outgoing>);

    /// Handle one inbound frame. Must not block or do I/O.
    fn on_frame(
        &mut self,
        frame: &Frame,
        now: UtcMillis,
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError>;

    /// Application-level keep-alive, if the protocol has one.
    fn keepalive(&mut self, _now: UtcMillis) -> Option<Outgoing> {
        None
    }
}

/// Stable provider identifiers (stored in recordings).
pub const CQG: &str = "cqg";
pub const COINBASE: &str = "coinbase";
pub const KRAKEN: &str = "kraken";

pub trait Provider: Send + Sync {
    /// Stable id, e.g. `"cqg"`; stored in recording headers.
    fn id(&self) -> &'static str;
    /// Wire protocol version, stored in recordings (e.g. `"cqg-webapi-2.230"`).
    fn protocol(&self) -> String;
    fn capabilities(&self) -> Capabilities;
    /// Whether this provider maps the product to one of its root symbols.
    fn supports(&self, product: &Product) -> bool;
    /// Fresh protocol state for one connection to `contract` (derived by the provider into its
    /// own symbol by rule). `None` = contract unknown (replay of a pre-v3 recording); the
    /// session then accepts whatever contract the data is for.
    fn new_session(
        &self,
        product: &Product,
        contract: Option<ContractKey>,
    ) -> Result<Box<dyn ProviderSession>, SessionError>;
}
