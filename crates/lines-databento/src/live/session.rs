//! The live session: Databento's gateway protocol for one contract, with no I/O (decision
//! 0011). The transport hands it the socket's bytes as frames; it answers with the lines to
//! send. Replays of a recording drive this same code, so decoding is byte-exact.
//!
//! Protocol (LIVE-DESIGN.md, checked 2026-11-07 against the docs and databento-rs 0.64.0):
//! - Each connection starts with the transport's [`CONNECTION_MARKER`] frame (recorded, so one
//!   replay session can span reconnects). Each subscription line the transport sends comes back
//!   as a recorded [`SENT_PREFIX`] frame: the session takes its replay plan (which subscriptions
//!   replay, from when) from those, never from its own settings, so a recording decodes the
//!   same whatever the replaying machine's settings. Then come the gateway's `lsg_version=…` and
//!   `cram=<challenge>` lines. The session answers with the auth line, the CRAM response
//!   replaced by [`AUTH_PLACEHOLDER`]: only the transport holds the key and swaps it in on the
//!   way out, so no frame the session sees or the recorder writes carries the key, its hash or
//!   its bucket id.
//! - `success=1|…` → the subscriptions and a bare `start_session`; `success=0|error=…` is a
//!   rejected logon (no retry).
//! - Then DBN: metadata (version 3 only; any other is refused with a clear error, never a
//!   guess), then records.
//!
//! Subscriptions (raw symbol, e.g. `ESZ6`):
//! - `definition` from `start=0` (GLBX.MDP3 keeps the weekly session): the contract's
//!   definition is checked against the catalog (tick, multiplier, currency, exchange, symbol)
//!   before any market
//!   record is applied; records before it are held.
//! - `trades` replayed from the newest stored bar (the feed's `history_from`, a bar boundary),
//!   at most 24 hours back. The replayed trades are history, not live trades: they become bars,
//!   sent as one `History` snapshot whose closed `as_of` is the newest replayed millisecond
//!   (inclusive). `lines_core::stitch` then replaces the stored bars and drops re-delivered
//!   live trades, so a restart or reconnect never counts a trade twice. Should the switch from
//!   replay to live split a millisecond, a live trade at or before `as_of` (which the stitch
//!   would drop) is added to the snapshot's last bar instead, sent again as a corrected
//!   `History` (it replaces the bar). History older than the replay reaches is a `HistoryGap`.
//!   Trades come from this schema only (the book schemas' own trade records are ignored).
//! - The book, without a start (natural refresh: every record carries the whole book): `mbp-1`,
//!   the inside bid and ask, by default ([`TOP_OF_BOOK`]). `mbp-10` only when asked for
//!   ([`BookPlan`]): real-time MBP-10 and MBO need Databento's CME Plus plan. The book clears
//!   at each connection and rebuilds at the first event end.
//!
//! A plan can leave a schema out: the gateway sends `Not authorized for <schema> schema` and
//! closes the connection (seen 2026-11-09). A refused book schema isn't a failure: the feed says
//! so, and the next connection, and every other feed this run, doesn't ask again ([`BookPlan`]).
//! Without `trades` or `definition` there's no chart: the error is final, never a reconnect loop.
//!
//! `slow_reader_behavior=warn`: the gateway never skips records; warnings are counted. Should
//! records be skipped anyway, the book clears. A replay that falls out of Databento's window
//! is a history gap; the next connection replays from no earlier than the start the gateway
//! names (a floor the provider keeps per contract).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use dbn::decode::dbn::fsm::{DbnFsm, ProcessResult};
use dbn::{
    BidAskPair, ErrorCode, ErrorMsg, FlagSet, InstrumentDefMsg, Mbp1Msg, Mbp10Msg, RecordHeader,
    SymbolMappingMsg, SystemCode, SystemMsg, TradeMsg, UNDEF_PRICE, VersionUpgradePolicy, rtype,
};
use lines_core::{Aggressor, Bar, ContractKey, Price, Product, Qty, UtcMillis};
use lines_feed::native::NativeEvent;
use lines_feed::provider::{
    Frame, MarketEvent, Outgoing, ProviderSession, SessionError, StartContext,
};

use crate::book::{BookDiff, BookStats};
use crate::defs::{DefOutcome, classify};
use crate::import::raw_symbol;
use crate::price::PriceConv;

/// Stands for the CRAM response in the auth line; the transport replaces it.
pub const AUTH_PLACEHOLDER: &str = "<lines-databento-auth>";
/// The transport's first frame on every connection (recorded; never sent by the gateway).
pub const CONNECTION_MARKER: &[u8] = b"\0lines-databento: new connection\n";
/// The transport's echo of a line it sent (not the auth line), recorded as an inbound frame.
pub const SENT_PREFIX: &[u8] = b"\0lines-databento: sent ";
pub const DATASET: &str = "GLBX.MDP3";
/// The only DBN version this build decodes.
pub const DBN_VERSION: u8 = 3;
/// Intraday replay reaches 24 hours back (less over weekends), from the next 10-minute
/// boundary (at 17:01 the gateway took 17:10 the day before at the earliest); ask for less, so
/// the first try is accepted. A start refused anyway names the earliest, and the next connection
/// starts there ([`invalid_start_floor`]).
pub const REPLAY_WINDOW_MS: i64 = 23 * 3_600_000 + 40 * 60_000;
/// Market events held while the contract's definition hasn't arrived.
pub const MAX_HELD: usize = 200_000;
/// Gateway heartbeats are requested this often.
pub const HEARTBEAT_S: u32 = 10;
/// If the gateway reports fewer replay completions than subscriptions it replays, history is
/// complete once a completion has come and no replayed trade for this long (Databento doesn't
/// document whether each replaying subscription reports its own; LIVE-DESIGN.md "still to
/// verify").
pub const REPLAY_SETTLE_MS: i64 = 10_000;

/// The earliest replay start Databento still holds, per contract, from a
/// `REPLAY_DATA_AGED_OUT` error (ns). Only ever a lower bound, so feeds may share it.
pub type Floors = Arc<Mutex<HashMap<ContractKey, u64>>>;

/// Shown with the DOM's book: the inside only, so it isn't read as the market's depth.
pub const TOP_OF_BOOK: &str = "Top of book only";
/// The same, when `mbp-10` was asked for and refused.
/// One state for the trader whatever the cause (the log says why).
pub const DEPTH_REFUSED: &str = TOP_OF_BOOK;
/// Shown when the plan refuses `mbp-1` too: trades only.
pub const NO_BOOK: &str = "No bid or ask: not on your plan";

/// Which book schema the provider's feeds ask for. What the gateway refuses stays refused for
/// this run (a restart asks again, so a changed plan is picked up).
#[derive(Debug, Default)]
pub struct BookPlan {
    /// Ask for `mbp-10` (a CME Plus account), not `mbp-1`.
    pub want_depth: bool,
    depth_refused: AtomicBool,
    top_refused: AtomicBool,
}

impl BookPlan {
    pub fn new(want_depth: bool) -> Arc<BookPlan> {
        Arc::new(BookPlan {
            want_depth,
            ..BookPlan::default()
        })
    }

    /// The schema to subscribe, and what the feed should say about its book.
    fn choice(&self) -> (Option<&'static str>, Option<&'static str>) {
        if self.want_depth && !self.depth_refused.load(Ordering::Acquire) {
            (Some("mbp-10"), None)
        } else if self.top_refused.load(Ordering::Acquire) {
            (None, Some(NO_BOOK))
        } else if self.want_depth {
            (Some("mbp-1"), Some(DEPTH_REFUSED))
        } else {
            (Some("mbp-1"), Some(TOP_OF_BOOK))
        }
    }
}

/// The schema in a `Not authorized for <schema> schema` error (`Some("")` if it names none of
/// ours: still a refusal, and retrying won't help).
fn not_authorized_schema(text: &str) -> Option<&'static str> {
    let text = text.to_ascii_lowercase();
    if !text.contains("not authorized") {
        return None;
    }
    let named = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .find_map(|word| {
            ["mbp-10", "mbp-1", "trades", "definition"]
                .into_iter()
                .find(|s| *s == word)
        });
    Some(named.unwrap_or(""))
}

/// How much trade history a connection replays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayLimit {
    /// At most this far back (capped by [`REPLAY_WINDOW_MS`]); 0 = no replay.
    pub max_ms: i64,
}

impl Default for ReplayLimit {
    fn default() -> Self {
        ReplayLimit {
            max_ms: REPLAY_WINDOW_MS,
        }
    }
}

/// What the session counted (tests, diagnostics).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionStats {
    /// Live trades emitted.
    pub trades: u64,
    /// Trades folded into the replay's history bars.
    pub replayed_trades: u64,
    pub book: BookStats,
    /// Prices that aren't exact or on the tick grid (counted, never rounded).
    pub rejected: u64,
    pub slow_reader_warnings: u64,
    pub skipped_record_notices: u64,
    /// Market events dropped because the definition never came within [`MAX_HELD`].
    pub held_dropped: u64,
    pub heartbeats: u64,
    pub connections: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum Phase {
    Greeting,
    Auth,
    Streaming,
}

/// A replayed trade, kept until it is folded into a bar (bars carry no aggressor).
#[derive(Clone, Copy, Debug)]
struct Replayed {
    time: UtcMillis,
    price: Price,
    qty: Qty,
}

pub struct DatabentoSession {
    product: Product,
    contract: Option<ContractKey>,
    symbol: String,
    client: String,
    floors: Floors,
    plan: Arc<BookPlan>,
    /// This connection asked for a book (from the recorded echoes).
    book_echoed: bool,
    limit: ReplayLimit,
    phase: Phase,
    line: Vec<u8>,
    fsm: DbnFsm,
    conv: PriceConv,
    ctx: Option<StartContext>,
    now: UtcMillis,
    /// The instrument id of our contract (definition / symbol mapping).
    instrument: Option<u32>,
    defined: bool,
    ready_sent: bool,
    held: Vec<MarketEvent>,
    held_dropped_trades: u64,
    book: BookDiff,
    bid: Option<Price>,
    ask: Option<Price>,
    /// The trades replay's start (ms), when this connection replays.
    replay_start: Option<i64>,
    /// Replays still running (definition, trades); trades are history until 0.
    replays_pending: u8,
    /// Replay completions seen, and the last replay activity (a completion or a replayed
    /// trade), for the settle fallback.
    completions_seen: u8,
    replay_activity: Option<UtcMillis>,
    history_done: bool,
    /// Replayed trades folded into bars (bucket start ms → bar, its first and last trade time).
    replay_bars: BTreeMap<i64, (Bar, UtcMillis, UtcMillis)>,
    /// The newest replayed trade time (the snapshot's `as_of`).
    replay_max: Option<UtcMillis>,
    /// The snapshot's last bar and `as_of`, for live trades at or before it.
    snapshot_tail: Option<(Bar, UtcMillis)>,
    stats: SessionStats,
}

/// The largest integer in `text` that looks like a nanosecond timestamp.
/// The earliest start a refused replay names: Databento's "Invalid start time. Must be
/// 2026-11-09T17:10:00Z or later, or 0" (ns). `None` for any other text.
fn invalid_start_floor(text: &str) -> Option<u64> {
    let rest = text.split_once("Invalid start time")?.1;
    let after = rest.split_once("Must be ")?.1;
    let iso = after.split_whitespace().next()?.trim_end_matches(',');
    let t = UtcMillis::parse_iso(iso)?;
    u64::try_from(t.0).ok()?.checked_mul(1_000_000)
}

fn ns_in(text: &str) -> Option<u64> {
    text.split(|c: char| !c.is_ascii_digit())
        .filter(|s| s.len() >= 18)
        .filter_map(|s| s.parse::<u64>().ok())
        .max()
}

impl DatabentoSession {
    /// `contract` is `None` only when replaying a recording whose contract is unknown.
    pub fn new(
        product: &Product,
        contract: Option<ContractKey>,
        floors: Floors,
        limit: ReplayLimit,
        client: &str,
    ) -> Result<Self, SessionError> {
        let symbol = match contract {
            Some(k) => raw_symbol(k).ok_or_else(|| {
                SessionError::Unavailable(format!(
                    "{} has no Databento symbol",
                    k.label(product).as_str()
                ))
            })?,
            None => String::new(),
        };
        let fsm = DbnFsm::builder()
            .upgrade_policy(VersionUpgradePolicy::AsIs)
            .build()
            .map_err(|e| SessionError::Protocol(format!("DBN decoder: {e}")))?;
        Ok(DatabentoSession {
            product: product.clone(),
            contract,
            symbol,
            client: client.to_string(),
            floors,
            plan: Arc::default(),
            book_echoed: false,
            limit,
            phase: Phase::Greeting,
            line: Vec::new(),
            fsm,
            conv: PriceConv::new(product),
            ctx: None,
            now: UtcMillis(0),
            instrument: None,
            defined: contract.is_none(),
            ready_sent: false,
            held: Vec::new(),
            held_dropped_trades: 0,
            book: BookDiff::new(),
            bid: None,
            ask: None,
            replay_start: None,
            replays_pending: 0,
            completions_seen: 0,
            replay_activity: None,
            history_done: false,
            replay_bars: BTreeMap::new(),
            replay_max: None,
            snapshot_tail: None,
            stats: SessionStats::default(),
        })
    }

    /// The provider's book plan, shared by its feeds and their later connections.
    pub fn with_book_plan(mut self, plan: Arc<BookPlan>) -> Self {
        self.plan = plan;
        self
    }

    pub fn stats(&self) -> SessionStats {
        self.stats
    }

    /// A new connection (the transport's marker): protocol state starts over; what was learned
    /// about the contract stays.
    fn new_connection(&mut self) {
        self.stats.connections += 1;
        self.phase = Phase::Greeting;
        self.line.clear();
        self.fsm.reset();
        self.book.clear();
        self.bid = None;
        self.ask = None;
        self.instrument = None;
        self.replay_start = None;
        self.book_echoed = false;
        self.replays_pending = 0;
        self.completions_seen = 0;
        self.replay_activity = None;
        self.history_done = false;
        self.replay_bars.clear();
        self.replay_max = None;
        self.snapshot_tail = None;
    }

    fn period_ms(&self) -> i64 {
        self.ctx.map_or(60_000, |c| c.bar_period_ms.max(1))
    }

    /// The auth line (the CRAM response as [`AUTH_PLACEHOLDER`]).
    fn auth_line(&self) -> String {
        format!(
            "auth={AUTH_PLACEHOLDER}|dataset={DATASET}|encoding=dbn|compression=none|ts_out=0|client={}|heartbeat_interval_s={HEARTBEAT_S}|slow_reader_behavior=warn\n",
            self.client
        )
    }

    /// The subscriptions and `start_session`, after a successful logon.
    fn subscribe(&mut self, events: &mut Vec<MarketEvent>, out: &mut Vec<Outgoing>) {
        let Some(contract) = self.contract else {
            return; // a replay of an unknown contract: nothing is sent anyway
        };
        let sym = self.symbol.clone();
        let now_ms = self.now.0;
        let period = self.period_ms();
        // What the feed wants: from its newest stored bar, else `history_ms` back.
        let wanted = self.ctx.map_or(now_ms, |c| {
            c.history_from.map_or(c.now.0 - c.history_ms, |f| f.0)
        });
        let floor_ms = self
            .floors
            .lock()
            .expect("floors")
            .get(&contract)
            .map(|ns| (*ns / 1_000_000) as i64);
        let reach = self.limit.max_ms.min(REPLAY_WINDOW_MS);
        let start = if reach <= 0 {
            None
        } else {
            // On a bar boundary, so the snapshot's first bar is whole; never after now.
            let earliest = (now_ms - reach).max(floor_ms.unwrap_or(i64::MIN));
            let earliest = UtcMillis(earliest + period - 1).floor_to(period).0;
            let latest = self.now.floor_to(period).0;
            Some(wanted.max(earliest).min(latest).max(0))
        };
        let line = |s: String| Outgoing::Frame(Frame::text(s));
        out.push(line(format!(
            "schema=definition|stype_in=raw_symbol|symbols={sym}|snapshot=0|is_last=1|start=0\n"
        )));
        out.push(line(match start {
            Some(ms) => format!(
                "schema=trades|stype_in=raw_symbol|symbols={sym}|snapshot=0|is_last=1|start={}\n",
                ms as u64 * 1_000_000
            ),
            None => {
                format!("schema=trades|stype_in=raw_symbol|symbols={sym}|snapshot=0|is_last=1\n")
            }
        }));
        let (book_schema, note) = self.plan.choice();
        if let Some(schema) = book_schema {
            out.push(line(format!(
                "schema={schema}|stype_in=raw_symbol|symbols={sym}|snapshot=0|is_last=1\n"
            )));
        }
        out.push(line("start_session\n".to_string()));
        // A new connection's book starts empty (it fills at the first event end).
        self.book.clear();
        events.push(MarketEvent::BookClear);
        let _ = note; // the feed is told from the echoes (`on_sent`), as a replay is
        // The replay plan is taken from the transport's echoes of these lines (on_sent).
    }

    /// The transport sent `line` (its recorded echo): the replay plan.
    fn on_sent(&mut self, line: &str, events: &mut Vec<MarketEvent>) {
        let fields: HashMap<&str, &str> = line
            .trim_end()
            .split('|')
            .filter_map(|kv| kv.split_once('='))
            .collect();
        let start_ns = fields.get("start").and_then(|s| s.parse::<u64>().ok());
        match (fields.get("schema").copied(), start_ns) {
            (Some("definition"), Some(_)) => self.replays_pending += 1,
            (Some("mbp-10"), _) => self.book_echoed = true,
            (Some("mbp-1"), _) => {
                self.book_echoed = true;
                events.push(MarketEvent::DepthUnavailable {
                    reason: TOP_OF_BOOK.into(),
                });
            }
            (Some("trades"), start) => {
                let start_ms = start.map(|ns| (ns / 1_000_000) as i64);
                if start_ms.is_some() {
                    self.replays_pending += 1;
                }
                self.replay_start = start_ms;
                let wanted = self.ctx.map_or(self.now.0, |c| {
                    c.history_from.map_or(c.now.0 - c.history_ms, |f| f.0)
                });
                let covered_from = start_ms.unwrap_or(self.now.0);
                // Only for a session's first connection: live, every connection is a new
                // session with the store's newest bar; a replay runs one session across a
                // recording's connections and can't know what a later one wanted.
                if wanted < covered_from && self.stats.connections <= 1 {
                    events.push(MarketEvent::HistoryGap {
                        from: UtcMillis(wanted),
                        to: UtcMillis(covered_from),
                    });
                }
            }
            _ if line.trim_end() == "start_session" && !self.book_echoed => {
                events.push(MarketEvent::DepthUnavailable {
                    reason: NO_BOOK.into(),
                });
            }
            _ => {}
        }
    }

    fn handle_line(
        &mut self,
        line: &str,
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        let line = line.trim_end_matches(['\r', '\n']);
        match self.phase {
            Phase::Greeting => {
                if line.starts_with("cram=") {
                    out.push(Outgoing::Frame(Frame::text(self.auth_line())));
                    self.phase = Phase::Auth;
                }
                // `lsg_version=…` and anything else before the challenge: informational.
                Ok(())
            }
            Phase::Auth => {
                let fields: HashMap<&str, &str> = line
                    .split('|')
                    .filter_map(|kv| kv.split_once('='))
                    .collect();
                match fields.get("success") {
                    Some(&"1") => {
                        self.subscribe(events, out);
                        self.phase = Phase::Streaming;
                        Ok(())
                    }
                    Some(_) => Err(SessionError::LogonRejected {
                        code: 0,
                        text: fields
                            .get("error")
                            .copied()
                            .unwrap_or("authentication failed")
                            .to_string(),
                    }),
                    None => Ok(()),
                }
            }
            Phase::Streaming => Ok(()),
        }
    }

    /// An event for the store: held until the contract's definition is checked.
    fn emit(&mut self, e: MarketEvent, events: &mut Vec<MarketEvent>) {
        if self.defined {
            events.push(e);
        } else if self.held.len() < MAX_HELD {
            self.held.push(e);
        } else {
            self.stats.held_dropped += 1;
            if matches!(e, MarketEvent::Trade { .. }) {
                self.held_dropped_trades += 1;
            }
        }
    }

    fn mine(&self, instrument_id: u32) -> bool {
        self.instrument.is_none_or(|i| i == instrument_id)
    }

    fn on_definition(
        &mut self,
        d: &InstrumentDefMsg,
        events: &mut Vec<MarketEvent>,
    ) -> Result<(), SessionError> {
        let raw = d.raw_symbol().unwrap_or("").to_string();
        match classify(d) {
            DefOutcome::Contract(k) => {
                let ours = match self.contract {
                    Some(c) => c == k,
                    // Replay of an unknown contract: the first of our product.
                    None => k.product == self.product.id && self.instrument.is_none(),
                };
                if !ours {
                    return Ok(());
                }
                if self.contract.is_none() {
                    self.contract = Some(k);
                    events.push(MarketEvent::ContractChanged(k));
                }
                self.instrument = Some(d.hd.instrument_id);
                // Ready, then what was held for the definition, in order.
                if !self.ready_sent {
                    self.ready_sent = true;
                    events.push(MarketEvent::Ready);
                }
                if !self.defined {
                    self.defined = true;
                    events.append(&mut self.held);
                    if self.held_dropped_trades > 0 {
                        events.push(MarketEvent::Gap {
                            missed: self.held_dropped_trades,
                        });
                        self.held_dropped_trades = 0;
                    }
                }
                Ok(())
            }
            DefOutcome::BadMetadata(m) if raw == self.symbol => Err(SessionError::BadMetadata(m)),
            _ => Ok(()),
        }
    }

    /// A replayed trade into its bar: open and close follow trade time, not arrival order.
    fn fold(&mut self, t: Replayed) {
        let bucket = t.time.floor_to(self.period_ms());
        self.replay_max = Some(self.replay_max.map_or(t.time, |m| m.max(t.time)));
        match self.replay_bars.get_mut(&bucket.0) {
            Some((b, first, last)) => {
                b.high = b.high.max(t.price);
                b.low = b.low.min(t.price);
                if t.time < *first {
                    b.open = t.price;
                    *first = t.time;
                }
                if t.time >= *last {
                    b.close = t.price;
                    *last = t.time;
                }
                b.volume = b.volume.saturating_add(t.qty);
            }
            None => {
                self.replay_bars.insert(
                    bucket.0,
                    (Bar::from_trade(bucket, t.price, t.qty), t.time, t.time),
                );
            }
        }
    }

    fn on_trade(&mut self, t: &TradeMsg, events: &mut Vec<MarketEvent>) {
        if !self.mine(t.hd.instrument_id) {
            return;
        }
        let time = UtcMillis((t.hd.ts_event / 1_000_000) as i64);
        let Ok(price) = self.conv.price(t.price) else {
            self.stats.rejected += 1;
            self.emit(MarketEvent::Rejected { count: 1 }, events);
            return;
        };
        let aggressor = match t.side as u8 {
            b'B' => Some(Aggressor::Buy),
            b'A' => Some(Aggressor::Sell),
            _ => None,
        };
        let r = Replayed {
            time,
            price,
            qty: Qty(t.size as i64),
        };
        if self.replay_start.is_some() && !self.history_done {
            // History: folded into the snapshot's bars.
            self.stats.replayed_trades += 1;
            self.replay_activity = Some(self.now);
            self.fold(r);
            return;
        }
        self.stats.trades += 1;
        let period = self.period_ms();
        if let Some((bar, as_of)) = self.snapshot_tail.as_mut() {
            // Only within the last bar's bucket; an older straggler goes out as a trade (the
            // stitch drops it as already covered).
            if time <= *as_of && time.floor_to(period) == bar.time {
                // A live trade inside the snapshot's last millisecond: the stitch would drop
                // it, so the snapshot's last bar takes it and is sent again.
                bar.high = bar.high.max(price);
                bar.low = bar.low.min(price);
                if time >= *as_of {
                    bar.close = price;
                }
                bar.volume = bar.volume.saturating_add(r.qty);
                let (bar, as_of) = (*bar, *as_of);
                self.emit(
                    MarketEvent::History {
                        bars: vec![bar],
                        as_of: Some(as_of),
                    },
                    events,
                );
                return;
            }
            if time > *as_of {
                self.snapshot_tail = None;
            }
        }
        self.emit(
            MarketEvent::Trade {
                time,
                price,
                qty: r.qty,
                aggressor,
            },
            events,
        );
    }

    fn on_mbp10(&mut self, m: &Mbp10Msg, events: &mut Vec<MarketEvent>) {
        self.on_book(&m.hd, m.action as u8, m.flags, &m.levels, events);
    }

    /// `mbp-1` is the same book, one level deep.
    fn on_mbp1(&mut self, m: &Mbp1Msg, events: &mut Vec<MarketEvent>) {
        let levels: [BidAskPair; 10] = std::array::from_fn(|i| match i {
            0 => m.levels[0].clone(),
            _ => BidAskPair {
                bid_px: UNDEF_PRICE,
                ask_px: UNDEF_PRICE,
                ..Default::default()
            },
        });
        self.on_book(&m.hd, m.action as u8, m.flags, &levels, events);
    }

    fn on_book(
        &mut self,
        hd: &RecordHeader,
        action: u8,
        flags: FlagSet,
        levels: &[BidAskPair; 10],
        events: &mut Vec<MarketEvent>,
    ) {
        if !self.mine(hd.instrument_id) {
            return;
        }
        if action == b'R' {
            self.book.clear();
        }
        if !flags.is_last() {
            return; // the book is consistent only at an event's end
        }
        let time_us = (hd.ts_event / 1_000) as i64;
        let mut native = Vec::new();
        let rejected_before = self.stats.book.off_grid_levels;
        self.book.apply(
            time_us,
            levels,
            &self.conv,
            &mut self.stats.book,
            &mut native,
        );
        let off = self.stats.book.off_grid_levels - rejected_before;
        if off > 0 {
            self.stats.rejected += off;
            self.emit(MarketEvent::Rejected { count: off }, events);
        }
        for e in native {
            let e = match e {
                NativeEvent::BookClear { .. } => MarketEvent::BookClear,
                NativeEvent::BookLevel {
                    side,
                    price,
                    qty,
                    orders,
                    ..
                } => MarketEvent::BookLevel {
                    side,
                    price,
                    qty,
                    orders,
                },
                _ => continue,
            };
            self.emit(e, events);
        }
        let top = |px: i64| {
            (px != UNDEF_PRICE)
                .then(|| self.conv.price(px).ok())
                .flatten()
        };
        let (bid, ask) = (top(levels[0].bid_px), top(levels[0].ask_px));
        if bid != self.bid {
            self.bid = bid;
            if let Some(b) = bid {
                self.emit(MarketEvent::Bid(b), events);
            }
        }
        if ask != self.ask {
            self.ask = ask;
            if let Some(a) = ask {
                self.emit(MarketEvent::Ask(a), events);
            }
        }
    }

    fn on_system(&mut self, s: &SystemMsg, events: &mut Vec<MarketEvent>) {
        let text = s.msg().unwrap_or("");
        let code = s.code().ok();
        let replay_done = code == Some(SystemCode::ReplayCompleted)
            || (code.is_none_or(|c| c == SystemCode::Unset)
                && text.contains("Finished")
                && text.contains("replay"));
        if s.is_heartbeat() || code == Some(SystemCode::Heartbeat) {
            self.stats.heartbeats += 1;
        } else if code == Some(SystemCode::SlowReaderWarning) {
            self.stats.slow_reader_warnings += 1;
            tracing::warn!("Databento: the gateway says Lines reads too slowly: {text}");
        } else if replay_done && self.replays_pending > 0 {
            self.replays_pending -= 1;
            self.completions_seen += 1;
            self.replay_activity = Some(self.now);
            if self.replays_pending == 0 {
                self.finish_history(events);
            }
        }
    }

    /// The replay is history: its bars as one snapshot, closed at the newest replayed
    /// millisecond, then `HistoryComplete`.
    fn finish_history(&mut self, events: &mut Vec<MarketEvent>) {
        if self.history_done {
            return;
        }
        self.history_done = true;
        if let Some(as_of) = self.replay_max.take() {
            let bars: Vec<Bar> = std::mem::take(&mut self.replay_bars)
                .into_values()
                .map(|(b, _, _)| b)
                .collect();
            self.snapshot_tail = bars.last().map(|b| (*b, as_of));
            self.emit(
                MarketEvent::History {
                    bars,
                    as_of: Some(as_of),
                },
                events,
            );
        }
        self.emit(MarketEvent::HistoryComplete, events);
    }

    /// Databento no longer holds the replay start asked for: the next connection starts no
    /// earlier than `floor_ns` (shared by feeds of the contract); what lies before it is a gap.
    /// A retryable error (the feed reconnects).
    fn replay_floor(
        &mut self,
        floor_ns: u64,
        text: &str,
        events: &mut Vec<MarketEvent>,
    ) -> SessionError {
        if let Some(k) = self.contract {
            let mut f = self.floors.lock().expect("floors");
            let v = f.entry(k).or_insert(floor_ns);
            *v = (*v).max(floor_ns);
        }
        let from = UtcMillis(self.replay_start.unwrap_or(self.now.0));
        let to = UtcMillis((floor_ns / 1_000_000) as i64).min(self.now);
        if to > from {
            events.push(MarketEvent::HistoryGap { from, to });
        }
        SessionError::Protocol(format!(
            "Databento replay start refused ({text}); reconnecting from the earliest data it holds"
        ))
    }

    fn on_error(
        &mut self,
        e: &ErrorMsg,
        events: &mut Vec<MarketEvent>,
    ) -> Result<(), SessionError> {
        let text = e.err().unwrap_or("").to_string();
        if let Ok(ErrorCode::AuthFailed | ErrorCode::ApiKeyDeactivated) = e.code() {
            return Err(SessionError::LogonRejected { code: 0, text });
        }
        // A replay start Databento no longer holds, refused as a subscription (not as an aged-out
        // replay): the next connection starts at the earliest it names.
        if let Some(floor_ns) = invalid_start_floor(&text) {
            return Err(self.replay_floor(floor_ns, &text, events));
        }
        match not_authorized_schema(&text) {
            Some(schema @ ("mbp-10" | "mbp-1")) => {
                tracing::warn!("Databento: {text}; not asking for {schema} again this run");
                let refused = if schema == "mbp-10" {
                    &self.plan.depth_refused
                } else {
                    &self.plan.top_refused
                };
                refused.store(true, Ordering::Release);
                self.book.clear();
                self.bid = None;
                self.ask = None;
                // What this refusal means, from the recording alone (never this machine's plan).
                events.push(MarketEvent::DepthUnavailable {
                    reason: if schema == "mbp-10" {
                        DEPTH_REFUSED
                    } else {
                        NO_BOOK
                    }
                    .into(),
                });
                return Ok(());
            }
            Some(_) => {
                return Err(SessionError::Unavailable(format!(
                    "Databento: not on your plan: {text}"
                )));
            }
            None => {}
        }
        match e.code() {
            Ok(ErrorCode::AuthFailed | ErrorCode::ApiKeyDeactivated) => {
                Err(SessionError::LogonRejected { code: 0, text })
            }
            Ok(ErrorCode::ConnectionLimitExceeded) => Err(SessionError::LoggedOff(format!(
                "Databento connection limit: {text}"
            ))),
            Ok(ErrorCode::SymbolResolutionFailed | ErrorCode::InvalidSubscription) => {
                Err(SessionError::Unavailable(format!("Databento: {text}")))
            }
            Ok(ErrorCode::SkippedRecordsAfterSlowReading) => {
                // Not expected with `warn`; the book can't be trusted until the next event end.
                self.stats.skipped_record_notices += 1;
                self.book.clear();
                self.emit(MarketEvent::BookClear, events);
                Ok(())
            }
            Ok(ErrorCode::ReplayDataAgedOut) => {
                // Without a named start, the floor is now: asking again for the same start
                // would only fail again (this contract replays nothing more until restart).
                let now_ns = (self.now.0.max(0) as u64) * 1_000_000;
                let floor_ns = ns_in(&text).unwrap_or(now_ns);
                Err(self.replay_floor(floor_ns, &text, events))
            }
            _ => Err(SessionError::Protocol(format!("Databento: {text}"))),
        }
    }

    fn drain_records(&mut self, events: &mut Vec<MarketEvent>) -> Result<(), SessionError> {
        loop {
            match self.fsm.process() {
                ProcessResult::ReadMore(_) => return Ok(()),
                ProcessResult::Metadata(m) => {
                    if m.version != DBN_VERSION {
                        return Err(SessionError::BadMetadata(format!(
                            "Databento sent DBN version {}; this build decodes version {DBN_VERSION}",
                            m.version
                        )));
                    }
                }
                ProcessResult::Err(e) => {
                    return Err(SessionError::Protocol(format!("DBN: {e}")));
                }
                ProcessResult::Record(()) => {
                    let Some(rec) = self.fsm.last_record() else {
                        continue;
                    };
                    match rec.header().rtype {
                        rtype::MBP_0 => {
                            if let Some(t) = rec.get::<TradeMsg>().cloned() {
                                self.on_trade(&t, events);
                            }
                        }
                        rtype::MBP_1 => {
                            if let Some(m) = rec.get::<Mbp1Msg>().cloned() {
                                self.on_mbp1(&m, events);
                            }
                        }
                        rtype::MBP_10 => {
                            if let Some(m) = rec.get::<Mbp10Msg>().cloned() {
                                self.on_mbp10(&m, events);
                            }
                        }
                        rtype::INSTRUMENT_DEF => {
                            if let Some(d) = rec.get::<InstrumentDefMsg>().cloned() {
                                self.on_definition(&d, events)?;
                            }
                        }
                        rtype::SYMBOL_MAPPING => {
                            if let Some(s) = rec.get::<SymbolMappingMsg>()
                                && self.contract.is_some()
                                && s.stype_in_symbol().ok() == Some(self.symbol.as_str())
                            {
                                self.instrument = Some(s.hd.instrument_id);
                            }
                        }
                        rtype::SYSTEM => {
                            if let Some(s) = rec.get::<SystemMsg>().cloned() {
                                self.on_system(&s, events);
                            }
                        }
                        rtype::ERROR => {
                            if let Some(e) = rec.get::<ErrorMsg>().cloned() {
                                self.on_error(&e, events)?;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

impl ProviderSession for DatabentoSession {
    fn start(&mut self, ctx: StartContext, _out: &mut Vec<Outgoing>) {
        // The gateway speaks first (greeting and challenge).
        self.now = ctx.now;
        self.ctx = Some(ctx);
    }

    fn on_frame(
        &mut self,
        frame: &Frame,
        now: UtcMillis,
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        self.now = now;
        if &frame.data[..] == CONNECTION_MARKER {
            self.new_connection();
            return Ok(());
        }
        if let Some(line) = frame.data.strip_prefix(SENT_PREFIX) {
            self.on_sent(&String::from_utf8_lossy(line), events);
            return Ok(());
        }
        let mut data: &[u8] = &frame.data;
        while self.phase != Phase::Streaming && !data.is_empty() {
            match data.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    self.line.extend_from_slice(&data[..=i]);
                    data = &data[i + 1..];
                    let line =
                        String::from_utf8_lossy(&std::mem::take(&mut self.line)).into_owned();
                    self.handle_line(&line, events, out)?;
                }
                None => {
                    self.line.extend_from_slice(data);
                    data = &[];
                }
            }
        }
        if self.phase == Phase::Streaming && !data.is_empty() {
            self.fsm.write_all(data);
            self.drain_records(events)?;
        }
        // Fewer completions than replays: settle after a quiet while (see REPLAY_SETTLE_MS).
        if !self.history_done
            && self.replays_pending > 0
            && self.completions_seen > 0
            && self
                .replay_activity
                .is_some_and(|t| now.0 - t.0 >= REPLAY_SETTLE_MS)
        {
            self.replays_pending = 0;
            self.finish_history(events);
        }
        Ok(())
    }

    /// The trades replay is back-fill until it completes.
    fn backfill_frame(&self) -> bool {
        self.phase == Phase::Streaming && !self.history_done
    }
}
