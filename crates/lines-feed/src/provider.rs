//! The provider abstraction (roadmap milestone 3).
//!
//! A provider is split in two:
//! - [`Provider`]: static facts (id, capabilities, which instruments it maps) and a factory
//!   for per-connection sessions. Vendor symbol mapping lives behind this trait (decision
//!   0003), so nothing above `lines-feed` sees vendor symbols.
//! - [`ProviderSession`]: I/O-free protocol logic for one connection. It turns wire frames
//!   into normalized [`MarketEvent`]s in *instrument units* (decision 0002) and says what to
//!   send. The generic driver (`connection.rs`) owns all I/O, so live, mock and replay run
//!   the identical session code.

use bytes::Bytes;
use lines_core::{Bar, Capabilities, Instrument, Price, Qty, UtcMillis};

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

/// Something a session wants the driver to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outgoing {
    Frame(Frame),
    /// GET this URL; the body comes back later as a [`FrameKind::Http`] frame.
    HttpGet(String),
}

/// Normalized market data, already converted into instrument units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarketEvent {
    /// Subscribed and streaming.
    Ready,
    /// The provider resolved to a different underlying contract than last time (e.g. a
    /// futures roll): existing bars are not comparable and must be dropped.
    ContractChanged,
    /// A complete, authoritative bar (history or a server bar update).
    Bar(Bar),
    Trade {
        time: UtcMillis,
        price: Price,
        qty: Qty,
    },
    /// Most recent trade price from a snapshot: updates "last", not volume.
    LastPrice(Price),
    Bid(Price),
    Ask(Price),
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
    /// `None` = no bars yet, request the provider's default history window.
    pub history_from: Option<UtcMillis>,
    pub bar_period_ms: i64,
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

pub trait Provider: Send + Sync {
    /// Stable id, e.g. `"cqg"`; stored in recording headers.
    fn id(&self) -> &'static str;
    /// Wire protocol version, stored in recordings (e.g. `"cqg-webapi-2.230"`).
    fn protocol(&self) -> String;
    fn capabilities(&self) -> Capabilities;
    /// Whether this provider maps the instrument to one of its symbols.
    fn supports(&self, instrument: &Instrument) -> bool;
    /// Fresh protocol state for one connection.
    fn new_session(
        &self,
        instrument: &Instrument,
    ) -> Result<Box<dyn ProviderSession>, SessionError>;
}
