//! Coinbase Exchange protocol logic for one connection, free of I/O.
//!
//! Coinbase sends prices and sizes as decimal *strings* on the websocket and in REST trades,
//! and as JSON *numbers* in REST candles. All are parsed from their text exactly into the
//! catalog product's units (decision 0002); nothing goes through `f64`.
//!
//! # History and the stitch (roadmap 4)
//!
//! On every (re)connect, with `cutoff` = the start of the bar forming at connect time:
//! 1. **Candles** (`/products/{id}/candles`) for `[history start, cutoff)`, in pages of 300,
//!    oldest first. A candle bucket is closed at its end, so each page is a history snapshot
//!    with `as_of = page end - 1 ms`: every trade before the page end is included, none after.
//! 2. **Trades** (`/products/{id}/trades`, newest first, paged back with `after=`) until they
//!    reach `cutoff`. They rebuild the forming bar exactly, delivered as a closed snapshot:
//!    with `t_n` the newest REST trade time, the bar holds every REST trade in
//!    `[cutoff, t_n)` and `as_of = t_n - 1 ms` (REST holds *every* trade up to its newest, so
//!    the snapshot is closed; trades sharing `t_n` may still be arriving and are sent as
//!    trades). As a snapshot it replaces whatever the store had for that bar, so a reconnect
//!    inside the same minute never counts the bar's earlier trades twice.
//! 3. **Websocket** matches stream from subscription. REST and websocket trades overlap; they
//!    are de-duplicated here by `trade_id` (exact, unlike timestamps, which Coinbase repeats
//!    within a microsecond), before reaching the store's timestamp-based stitch.
//! 4. **Holes.** REST trades can lag the websocket by seconds (seen live: a REST snapshot 3 s
//!    stale, so 32 trades fell between it and the first websocket trade). The websocket's
//!    start is known from `last_match` (the newest trade before the subscription). Any id range
//!    neither source delivered, and any range the matches channel dropped mid-stream, is
//!    fetched from REST by id (`after = hi + 1`), retried on the keepalive tick while REST
//!    lags, and reported as a history gap if it never closes. History is complete only once
//!    the REST-to-websocket hole is closed.
//!
//! Depth comes from the public `level2_batch` channel (`level2` needs authentication): a full
//! `snapshot`, then `l2update` batches every 50 ms with new totals per price (0 removes).
//! Coinbase reports the *maker's* side of a match, so the aggressor is the opposite side.
//!
//! Requests are issued one at a time. Each HTTP frame carries its request URL, so a response
//! is interpreted from the URL, not from this session's own request plan: a replay decodes it
//! the same way whatever history window the replaying build is configured with (0011). Anything that can't be back-filled is
//! reported as a history gap, never invented.

use std::collections::{BTreeSet, VecDeque};

use lines_core::{Aggressor, Bar, Price, Product, Qty, Side, UtcMillis};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::provider::{
    Frame, FrameKind, MarketEvent, Outgoing, ProviderSession, SessionError, StartContext,
    query_param, split_http,
};

/// Coinbase returns at most 300 candles per request.
const MAX_CANDLES: i64 = 300;
/// Bound on candle requests per connection (15 × 300 one-minute bars ≈ 3.1 days).
const MAX_CANDLE_PAGES: usize = 15;
/// REST trades per page (Coinbase's maximum).
const TRADES_PER_PAGE: usize = 1000;
/// Bound on trade pages used to rebuild the forming bar.
const MAX_TRADE_PAGES: u32 = 10;
/// Attempts to fill one id hole before reporting it as a gap (one per keepalive tick while
/// REST lags; the next attempt is immediate when one made progress).
const MAX_FILL_ATTEMPTS: u32 = 6;

/// Trade ids `lo..=hi` that neither REST nor the websocket delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Hole {
    lo: u64,
    hi: u64,
    /// The hole between the REST snapshot and the websocket start (gates history completion).
    boundary: bool,
    attempts: u32,
    /// Roughly when (for a gap report).
    when: UtcMillis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Request {
    /// Candle buckets `[start, end_excl)`.
    Candles {
        start: UtcMillis,
        end_excl: UtcMillis,
    },
    /// Trades older than `after` (`None` = newest page).
    Trades { after: Option<u64> },
    /// Fill an id hole: trades older than `after` (= the hole's `hi + 1`).
    Fill { after: u64 },
}

pub struct CoinbaseSession {
    product: String,
    /// Catalog spec of the spot product (units for exact parsing).
    spec: Product,
    rest_url: String,
    period_ms: i64,
    /// Newest websocket trade id.
    last_trade_id: Option<u64>,
    first_ws_id: Option<u64>,
    /// Start of the bar forming at connect: candles cover everything before it.
    cutoff: UtcMillis,
    /// Requests not yet answered, in issue order; the head is in flight.
    queue: VecDeque<Request>,
    /// Websocket trade ids seen before the REST trades finished (for exact de-duplication).
    ws_ids: BTreeSet<u64>,
    /// Newest trade id covered by the REST back-fill, once it has finished.
    rest_max_id: Option<u64>,
    rest_newest_seen: Option<u64>,
    /// REST trade pages processed on this connection.
    trade_pages: u32,
    /// Newest trade id the websocket will *not* deliver: `last_match`, else first id - 1.
    boundary: Option<u64>,
    /// REST back-paging has finished: later `/trades` responses are hole fills.
    backfill_done: bool,
    boundary_checked: bool,
    holes: Vec<Hole>,
    fill_in_flight: bool,
    complete_sent: bool,
    last_ws_time: UtcMillis,
    /// The queue head has been sent.
    head_issued: bool,
    /// REST trades in the forming bar, collected across pages.
    forming: Vec<RestFill>,
    /// History older than the page budget: reported as a gap with the first response.
    unfetched: Option<(UtcMillis, UtcMillis)>,
}

/// A REST trade inside the forming bar.
#[derive(Copy, Clone, Debug)]
struct RestFill {
    id: u64,
    time: UtcMillis,
    price: Price,
    qty: Qty,
    aggressor: Option<Aggressor>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WsMsg {
    Subscriptions {},
    Snapshot(L2Snapshot),
    L2update(L2Update),
    Match(MatchMsg),
    LastMatch(MatchMsg),
    Ticker(TickerMsg),
    Heartbeat(HeartbeatMsg),
    Error {
        message: String,
        #[serde(default)]
        reason: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct MatchMsg {
    trade_id: u64,
    time: String,
    price: String,
    size: String,
    /// The maker's side.
    #[serde(default)]
    side: Option<String>,
}

/// One REST trade (`/products/{id}/trades`).
#[derive(Deserialize)]
struct RestTrade {
    trade_id: u64,
    time: String,
    price: String,
    size: String,
    /// The maker's side.
    #[serde(default)]
    side: Option<String>,
}

/// `level2_batch` snapshot: the whole book as `[price, size]`.
#[derive(Deserialize)]
struct L2Snapshot {
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
}

/// `level2_batch` update: `[side, price, new size]`.
#[derive(Deserialize)]
struct L2Update {
    changes: Vec<[String; 3]>,
}

/// Coinbase reports the maker's side; the aggressor took the other side.
fn aggressor(maker_side: Option<&str>) -> Option<Aggressor> {
    match maker_side? {
        "buy" => Some(Aggressor::Sell),
        "sell" => Some(Aggressor::Buy),
        _ => None,
    }
}

#[derive(Deserialize)]
struct TickerMsg {
    #[serde(default)]
    best_bid: Option<String>,
    #[serde(default)]
    best_ask: Option<String>,
}

#[derive(Deserialize)]
struct HeartbeatMsg {
    last_trade_id: u64,
}

impl CoinbaseSession {
    pub fn new(product: String, spec: Product, rest_url: String) -> Self {
        CoinbaseSession {
            product,
            spec,
            rest_url,
            period_ms: UtcMillis::MINUTE,
            last_trade_id: None,
            first_ws_id: None,
            cutoff: UtcMillis(i64::MAX),
            queue: VecDeque::new(),
            ws_ids: BTreeSet::new(),
            rest_max_id: None,
            rest_newest_seen: None,
            trade_pages: 0,
            boundary: None,
            backfill_done: false,
            boundary_checked: false,
            holes: Vec::new(),
            fill_in_flight: false,
            complete_sent: false,
            last_ws_time: UtcMillis(i64::MIN),
            head_issued: false,
            forming: Vec::new(),
            unfetched: None,
        }
    }

    fn price(&self, s: &str) -> Option<Price> {
        self.spec.price.scale.parse_decimal(s).ok().map(Price)
    }

    fn qty(&self, s: &str) -> Option<Qty> {
        self.spec.qty.parse_decimal(s).ok()
    }

    fn url(&self, r: &Request) -> String {
        let product = &self.product;
        match *r {
            // `end` is inclusive (the bucket starting at `end` is returned).
            Request::Candles { start, end_excl } => format!(
                "{}/products/{product}/candles?granularity={}&start={start}&end={}",
                self.rest_url,
                self.period_ms / 1000,
                UtcMillis(end_excl.0 - self.period_ms)
            ),
            Request::Trades { after: None, .. } => {
                format!(
                    "{}/products/{product}/trades?limit={TRADES_PER_PAGE}",
                    self.rest_url
                )
            }
            Request::Trades { after: Some(id) } | Request::Fill { after: id } => format!(
                "{}/products/{product}/trades?limit={TRADES_PER_PAGE}&after={id}",
                self.rest_url
            ),
        }
    }

    /// Issue the head request unless it is already in flight.
    fn issue_head(&mut self, out: &mut Vec<Outgoing>) {
        if !self.head_issued
            && let Some(r) = self.queue.front()
        {
            out.push(Outgoing::HttpGet(self.url(r)));
            self.head_issued = true;
        }
    }

    /// Track websocket trade ids: returns false for duplicates; reports gaps.
    fn sequence(&mut self, trade_id: u64, events: &mut Vec<MarketEvent>) -> bool {
        if self.rest_max_id.is_some_and(|m| trade_id <= m) {
            return false; // already delivered by the REST back-fill
        }
        match self.last_trade_id {
            Some(last) if trade_id <= last => return false,
            Some(last) if trade_id > last + 1 => {
                // Dropped by the matches channel: count it, and fetch it from REST.
                events.push(MarketEvent::Gap {
                    missed: trade_id - last - 1,
                });
                self.add_hole(last + 1, trade_id - 1, false, self.last_ws_time);
            }
            _ => {}
        }
        self.last_trade_id = Some(trade_id);
        self.first_ws_id.get_or_insert(trade_id);
        if self.rest_max_id.is_none() {
            self.ws_ids.insert(trade_id);
        }
        true
    }

    fn add_hole(&mut self, lo: u64, hi: u64, boundary: bool, when: UtcMillis) {
        if lo <= hi {
            self.holes.push(Hole {
                lo,
                hi,
                boundary,
                attempts: 0,
                when,
            });
        }
    }

    /// Request the first open hole. Fills start only after back-paging has finished, so
    /// `/trades` responses are unambiguous: back-pages first, then fills.
    fn maybe_fill(&mut self, out: &mut Vec<Outgoing>) {
        if !self.backfill_done || self.fill_in_flight {
            return;
        }
        let Some(after) = self.holes.first().map(|h| h.hi + 1) else {
            return;
        };
        self.fill_in_flight = true;
        self.queue.push_back(Request::Fill { after });
        self.issue_head(out);
    }

    /// Once both the REST snapshot and the websocket start are known, record the hole
    /// between them, if any.
    fn check_boundary(&mut self, events: &mut Vec<MarketEvent>, out: &mut Vec<Outgoing>) {
        if self.boundary_checked || !self.backfill_done {
            return;
        }
        let Some(b) = self.boundary else { return };
        self.boundary_checked = true;
        if let Some(r) = self.rest_newest_seen {
            self.add_hole(r + 1, b, true, self.cutoff);
            self.maybe_fill(out);
        }
        self.maybe_complete(events);
    }

    fn maybe_complete(&mut self, events: &mut Vec<MarketEvent>) {
        if !self.complete_sent && self.boundary_checked && !self.holes.iter().any(|h| h.boundary) {
            self.complete_sent = true;
            events.push(MarketEvent::HistoryComplete);
        }
    }

    fn on_text(
        &mut self,
        text: &[u8],
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        let msg: WsMsg = serde_json::from_slice(text)
            .map_err(|e| SessionError::Protocol(format!("bad Coinbase message: {e}")))?;
        match msg {
            WsMsg::Subscriptions {} => events.push(MarketEvent::Ready),
            WsMsg::Match(m) => {
                if !self.sequence(m.trade_id, events) {
                    return Ok(());
                }
                if self.boundary.is_none() {
                    self.boundary = Some(m.trade_id - 1);
                    self.check_boundary(events, out);
                }
                match (
                    UtcMillis::parse_iso(&m.time),
                    self.price(&m.price),
                    self.qty(&m.size),
                ) {
                    (Some(time), Some(price), Some(qty)) => {
                        self.last_ws_time = time;
                        events.push(MarketEvent::Trade {
                            time,
                            price,
                            qty,
                            aggressor: aggressor(m.side.as_deref()),
                        })
                    }
                    _ => events.push(MarketEvent::Rejected { count: 1 }),
                }
                self.maybe_fill(out);
            }
            WsMsg::LastMatch(m) => {
                // Not a new trade: the newest one before the subscription. Everything up to it
                // must come from REST; the websocket starts after it.
                if self.boundary.is_none() {
                    self.boundary = Some(m.trade_id);
                    self.check_boundary(events, out);
                }
                match self.price(&m.price) {
                    Some(p) => events.push(MarketEvent::LastPrice(p)),
                    None => events.push(MarketEvent::Rejected { count: 1 }),
                }
            }
            WsMsg::Snapshot(b) => {
                events.push(MarketEvent::BookClear);
                let mut rejected = 0;
                for (side, levels) in [(Side::Bid, &b.bids), (Side::Ask, &b.asks)] {
                    for [p, q] in levels {
                        match (self.price(p), self.qty(q)) {
                            (Some(price), Some(qty)) => events.push(MarketEvent::BookLevel {
                                side,
                                price,
                                qty,
                                orders: 0,
                            }),
                            _ => rejected += 1,
                        }
                    }
                }
                if rejected > 0 {
                    events.push(MarketEvent::Rejected { count: rejected });
                }
            }
            WsMsg::L2update(u) => {
                let mut rejected = 0;
                for [side, p, q] in &u.changes {
                    let side = match side.as_str() {
                        "buy" => Side::Bid,
                        "sell" => Side::Ask,
                        _ => {
                            rejected += 1;
                            continue;
                        }
                    };
                    match (self.price(p), self.qty(q)) {
                        (Some(price), Some(qty)) => events.push(MarketEvent::BookLevel {
                            side,
                            price,
                            qty,
                            orders: 0,
                        }),
                        _ => rejected += 1,
                    }
                }
                if rejected > 0 {
                    events.push(MarketEvent::Rejected { count: rejected });
                }
            }
            WsMsg::Ticker(t) => {
                for (side, value) in [(true, t.best_bid), (false, t.best_ask)] {
                    let Some(v) = value else { continue };
                    match self.price(&v) {
                        Some(p) if side => events.push(MarketEvent::Bid(p)),
                        Some(p) => events.push(MarketEvent::Ask(p)),
                        None => events.push(MarketEvent::Rejected { count: 1 }),
                    }
                }
            }
            WsMsg::Heartbeat(h) => {
                if let Some(last) = self.last_trade_id
                    && h.last_trade_id > last
                {
                    // The matches channel may drop messages; heartbeats reveal it.
                    events.push(MarketEvent::Gap {
                        missed: h.last_trade_id - last,
                    });
                    self.add_hole(last + 1, h.last_trade_id, false, self.last_ws_time);
                    self.last_trade_id = Some(h.last_trade_id);
                    self.maybe_fill(out);
                }
            }
            WsMsg::Error { message, reason } => {
                return Err(SessionError::Unavailable(format!(
                    "Coinbase: {message}{}",
                    reason.map(|r| format!(" ({r})")).unwrap_or_default()
                )));
            }
            WsMsg::Other => {}
        }
        Ok(())
    }

    fn bad_json(what: &str, body: &[u8], e: serde_json::Error) -> SessionError {
        let preview = String::from_utf8_lossy(&body[..body.len().min(200)]).into_owned();
        SessionError::Protocol(format!("bad Coinbase {what} ({e}): {preview}"))
    }

    /// REST candles: `[[time_s, low, high, open, close, volume], ...]`, newest first.
    fn on_candles(
        &mut self,
        body: &[u8],
        start: UtcMillis,
        end_excl: UtcMillis,
        events: &mut Vec<MarketEvent>,
    ) -> Result<(), SessionError> {
        let rows: Vec<Vec<&RawValue>> =
            serde_json::from_slice(body).map_err(|e| Self::bad_json("candles", body, e))?;
        let mut bars = Vec::with_capacity(rows.len());
        let mut rejected = 0;
        for row in &rows {
            let bar = (|| {
                let [t, low, high, open, close, vol] = row.as_slice() else {
                    return None;
                };
                // A bucket start: whole seconds on the bar period's grid. Anything else
                // (overflowing or unaligned) is not a candle we asked for.
                let time = UtcMillis(t.get().parse::<i64>().ok()?.checked_mul(1000)?);
                if self.period_ms <= 0 || time.0.rem_euclid(self.period_ms) != 0 {
                    return None;
                }
                Some(Bar {
                    time,
                    open: self.price(open.get())?,
                    high: self.price(high.get())?,
                    low: self.price(low.get())?,
                    close: self.price(close.get())?,
                    volume: self.qty(vol.get())?,
                })
            })();
            match bar {
                Some(b) if b.time >= start && b.time < end_excl => bars.push(b),
                Some(_) => {} // outside the window (e.g. the forming bar at `cutoff`)
                None => rejected += 1,
            }
        }
        bars.sort_by_key(|b| b.time);
        // Candle buckets are closed at their end: everything before `end_excl` is included.
        events.push(MarketEvent::History {
            bars,
            as_of: Some(UtcMillis(end_excl.0 - 1)),
        });
        if rejected > 0 {
            events.push(MarketEvent::Rejected { count: rejected });
        }
        Ok(())
    }

    /// REST trades, newest first. Rebuilds the forming bar; pages back until `cutoff`.
    fn on_trades(
        &mut self,
        body: &[u8],
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        self.trade_pages += 1;
        let page = self.trade_pages - 1;
        let trades: Vec<RestTrade> =
            serde_json::from_slice(body).map_err(|e| Self::bad_json("trades", body, e))?;
        if let Some(newest) = trades.first() {
            self.rest_newest_seen.get_or_insert(newest.trade_id);
        }
        let mut reached_cutoff = false;
        let mut rejected = 0;
        for t in &trades {
            match (
                UtcMillis::parse_iso(&t.time),
                self.price(&t.price),
                self.qty(&t.size),
            ) {
                (Some(time), _, _) if time < self.cutoff => reached_cutoff = true,
                (Some(time), Some(price), Some(qty)) => self.forming.push(RestFill {
                    id: t.trade_id,
                    time,
                    price,
                    qty,
                    aggressor: aggressor(t.side.as_deref()),
                }),
                _ => rejected += 1,
            }
        }
        if rejected > 0 {
            events.push(MarketEvent::Rejected { count: rejected });
        }
        let full = trades.len() >= TRADES_PER_PAGE;
        let oldest = trades.last();
        if !reached_cutoff && full && page + 1 < MAX_TRADE_PAGES {
            let after = oldest.map(|t| t.trade_id);
            self.queue.push_back(Request::Trades { after });
            return Ok(());
        }
        if !reached_cutoff && full {
            // Ran out of pages before reaching the bar start: that stretch is missing.
            let to = oldest
                .and_then(|t| UtcMillis::parse_iso(&t.time))
                .unwrap_or(self.cutoff);
            events.push(MarketEvent::HistoryGap {
                from: self.cutoff,
                to,
            });
        }
        self.emit_forming_bar(events);
        // Back-fill done. Websocket trades at or below this id are duplicates from now on.
        let max_id = self
            .rest_newest_seen
            .unwrap_or(0)
            .max(self.ws_ids.last().copied().unwrap_or(0));
        self.rest_max_id = Some(max_id);
        self.ws_ids.clear();
        // REST delivered every id up to its newest: mid-stream holes below it are closed.
        if let Some(r) = self.rest_newest_seen {
            self.holes.retain_mut(|h| {
                h.lo = h.lo.max(r + 1);
                h.lo <= h.hi
            });
        }
        self.backfill_done = true;
        self.check_boundary(events, out);
        self.maybe_fill(out);
        Ok(())
    }

    /// The forming bar from REST trades, as a closed snapshot (module docs, step 2). Trades at
    /// the newest REST millisecond follow as trades, minus those the websocket already sent.
    fn emit_forming_bar(&mut self, events: &mut Vec<MarketEvent>) {
        let mut trades = std::mem::take(&mut self.forming);
        let Some(newest) = trades.iter().map(|t| t.time).max() else {
            return;
        };
        trades.sort_by_key(|t| (t.time, t.id));
        let mut bar: Option<Bar> = None;
        for t in trades.iter().filter(|t| t.time < newest) {
            match &mut bar {
                None => {
                    bar = Some(Bar {
                        time: t.time.floor_to(self.period_ms),
                        open: t.price,
                        high: t.price,
                        low: t.price,
                        close: t.price,
                        volume: t.qty,
                    })
                }
                Some(b) => {
                    b.high = b.high.max(t.price);
                    b.low = b.low.min(t.price);
                    b.close = t.price;
                    b.volume = b.volume.saturating_add(t.qty);
                }
            }
        }
        if let Some(b) = bar {
            events.push(MarketEvent::History {
                bars: vec![b],
                as_of: Some(UtcMillis(newest.0 - 1)),
            });
        }
        for t in trades.iter().filter(|t| t.time == newest) {
            if !self.ws_ids.contains(&t.id) {
                events.push(MarketEvent::Trade {
                    time: t.time,
                    price: t.price,
                    qty: t.qty,
                    aggressor: t.aggressor,
                });
            }
        }
    }

    /// A hole fill (`after = hi + 1`, newest first): emit the trades inside holes, shrink the
    /// holes, and give up on a hole still open after `MAX_FILL_ATTEMPTS`.
    fn on_fill(
        &mut self,
        body: &[u8],
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        self.fill_in_flight = false;
        let trades: Vec<RestTrade> =
            serde_json::from_slice(body).map_err(|e| Self::bad_json("trades", body, e))?;
        let mut got = BTreeSet::new();
        let mut fresh = Vec::new();
        let mut rejected = 0;
        for t in &trades {
            let in_hole = self
                .holes
                .iter()
                .any(|h| h.lo <= t.trade_id && t.trade_id <= h.hi);
            if !in_hole || !got.insert(t.trade_id) {
                continue;
            }
            match (
                UtcMillis::parse_iso(&t.time),
                self.price(&t.price),
                self.qty(&t.size),
            ) {
                // Before the forming bar: already in the candles.
                (Some(time), _, _) if time < self.cutoff => {}
                (Some(time), Some(price), Some(qty)) => fresh.push((
                    t.trade_id,
                    MarketEvent::Trade {
                        time,
                        price,
                        qty,
                        aggressor: aggressor(t.side.as_deref()),
                    },
                )),
                _ => rejected += 1,
            }
        }
        fresh.sort_by_key(|(id, _)| *id);
        events.extend(fresh.into_iter().map(|(_, e)| e));
        if rejected > 0 {
            events.push(MarketEvent::Rejected { count: rejected });
        }
        // Shrink the holes by what arrived (a hole may split).
        let mut next = Vec::with_capacity(self.holes.len());
        for (i, h) in std::mem::take(&mut self.holes).into_iter().enumerate() {
            // The hole just requested (the first) counts an attempt.
            let attempts = h.attempts + u32::from(i == 0);
            let mut lo = None;
            for id in h.lo..=h.hi + 1 {
                let present = id > h.hi || got.contains(&id);
                match (present, lo) {
                    (false, None) => lo = Some(id),
                    (true, Some(l)) => {
                        next.push(Hole {
                            lo: l,
                            hi: id - 1,
                            attempts,
                            ..h.clone()
                        });
                        lo = None;
                    }
                    _ => {}
                }
            }
        }
        let (dead, alive): (Vec<_>, Vec<_>) = next
            .into_iter()
            .partition(|h| h.attempts >= MAX_FILL_ATTEMPTS);
        for h in dead {
            tracing::warn!(
                "trades {}..={} could not be fetched; reporting a gap",
                h.lo,
                h.hi
            );
            events.push(MarketEvent::HistoryGap {
                from: h.when,
                to: h.when,
            });
        }
        self.holes = alive;
        self.maybe_complete(events);
        if !got.is_empty() {
            // Progress: more may be available now (a large hole). Otherwise wait for the
            // keepalive tick while REST catches up.
            self.maybe_fill(out);
        }
        Ok(())
    }
}

impl ProviderSession for CoinbaseSession {
    /// Retry an open id hole (REST lags the websocket by seconds).
    fn keepalive(&mut self, _now: UtcMillis) -> Option<Outgoing> {
        if !self.backfill_done || self.fill_in_flight || !self.queue.is_empty() {
            return None;
        }
        let after = self.holes.first()?.hi + 1;
        self.fill_in_flight = true;
        let r = Request::Fill { after };
        let get = Outgoing::HttpGet(self.url(&r));
        self.queue.push_back(r);
        self.head_issued = true;
        Some(get)
    }

    fn start(&mut self, ctx: StartContext, out: &mut Vec<Outgoing>) {
        let product = &self.product;
        out.push(Outgoing::Frame(Frame::text(format!(
            r#"{{"type":"subscribe","product_ids":["{product}"],"channels":["matches","ticker","heartbeat","level2_batch"]}}"#
        ))));
        let p = ctx.bar_period_ms;
        self.period_ms = p;
        self.cutoff = ctx.now.floor_to(p);
        let wanted = ctx
            .history_from
            .unwrap_or_else(|| UtcMillis(self.cutoff.0 - ctx.history_ms).floor_to(p));
        // Candle pages, built backwards from the cutoff, then issued oldest first.
        let mut pages = Vec::new();
        let mut end_excl = self.cutoff;
        while end_excl > wanted && pages.len() < MAX_CANDLE_PAGES {
            let start = UtcMillis((end_excl.0 - MAX_CANDLES * p).max(wanted.0));
            pages.push(Request::Candles { start, end_excl });
            end_excl = start;
        }
        // History is bounded: anything older than the page budget is not requested, and is
        // reported as missing (e.g. a local store whose newest bar is older than this).
        if end_excl > wanted {
            self.unfetched = Some((wanted, end_excl));
        }
        self.queue.extend(pages.into_iter().rev());
        self.queue.push_back(Request::Trades { after: None });
        self.issue_head(out);
    }

    fn on_frame(
        &mut self,
        frame: &Frame,
        _now: UtcMillis,
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        match frame.kind {
            FrameKind::Text => self.on_text(&frame.data, events, out),
            FrameKind::Http => {
                // Live: this answers the head request. Replay: the queue is irrelevant.
                self.queue.pop_front();
                if let Some((from, to)) = self.unfetched.take() {
                    events.push(MarketEvent::HistoryGap { from, to });
                }
                self.head_issued = false;
                let (url, body) = split_http(&frame.data);
                match url {
                    Some(u) if u.contains("/candles") => {
                        let window = (|| {
                            let start = UtcMillis::parse_iso(query_param(u, "start")?)?;
                            let end = UtcMillis::parse_iso(query_param(u, "end")?)?;
                            // The URL comes off the wire (or a recording): checked arithmetic.
                            let g = query_param(u, "granularity")?
                                .parse::<i64>()
                                .ok()
                                .filter(|g| *g > 0)?
                                .checked_mul(1000)?;
                            Some((start, UtcMillis(end.0.checked_add(g)?)))
                        })()
                        .ok_or_else(|| SessionError::Protocol(format!("bad candles URL {u}")))?;
                        self.on_candles(body, window.0, window.1, events)?
                    }
                    Some(u) if u.contains("/trades") && self.backfill_done => {
                        self.on_fill(body, events, out)?
                    }
                    Some(u) if u.contains("/trades") => self.on_trades(body, events, out)?,
                    Some(u) => {
                        return Err(SessionError::Protocol(format!(
                            "unexpected HTTP response for {u}"
                        )));
                    }
                    None => {
                        // Pre-URL recordings held one candles response covering history up to
                        // the cutoff, and nothing else.
                        self.on_candles(body, UtcMillis(i64::MIN), self.cutoff, events)?;
                        self.queue.clear();
                        self.complete_sent = true;
                        events.push(MarketEvent::HistoryComplete);
                    }
                }
                self.issue_head(out);
                Ok(())
            }
            FrameKind::Binary => Err(SessionError::Protocol(
                "unexpected binary frame from Coinbase".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    const NOW: &str = "2026-11-06T14:05:30Z";

    fn start(history_from: Option<UtcMillis>, history_ms: i64) -> (CoinbaseSession, Vec<Outgoing>) {
        let mut s = CoinbaseSession::new(
            "BTC-USD".into(),
            catalog::get(catalog::BTC_USD).unwrap(),
            "https://x".into(),
        );
        let mut out = Vec::new();
        s.start(
            StartContext {
                now: UtcMillis::parse_iso(NOW).unwrap(),
                history_from,
                bar_period_ms: 60_000,
                history_ms,
            },
            &mut out,
        );
        (s, out)
    }

    fn session() -> CoinbaseSession {
        start(None, 300 * 60_000).0
    }

    /// An HTTP frame answering the session's head request, as the transport builds it.
    fn reply(s: &CoinbaseSession, body: &str) -> Frame {
        let url = s.url(s.queue.front().expect("a request is in flight"));
        Frame::http(format!(
            "{url}
{body}"
        ))
    }

    fn feed(s: &mut CoinbaseSession, f: Frame) -> (Vec<MarketEvent>, Vec<Outgoing>) {
        let (mut ev, mut out) = (Vec::new(), Vec::new());
        s.on_frame(&f, UtcMillis(0), &mut ev, &mut out).unwrap();
        (ev, out)
    }

    fn text(s: &mut CoinbaseSession, json: &str) -> Vec<MarketEvent> {
        feed(s, Frame::text(json)).0
    }

    fn iso(s: &str) -> UtcMillis {
        UtcMillis::parse_iso(s).unwrap()
    }

    #[test]
    fn start_subscribes_and_requests_candles_then_trades_one_at_a_time() {
        let (mut s, out) = start(None, 300 * 60_000);
        let Outgoing::Frame(f) = &out[0] else {
            panic!()
        };
        let sub: serde_json::Value = serde_json::from_slice(&f.data).unwrap();
        assert_eq!(sub["product_ids"][0], "BTC-USD");
        assert_eq!(out.len(), 2, "only the first HTTP request is issued");
        assert_eq!(
            out[1],
            Outgoing::HttpGet(
                "https://x/products/BTC-USD/candles?granularity=60&start=2026-11-06T09:05:00.000Z&end=2026-11-06T14:04:00.000Z".into()
            )
        );
        let (ev, out) = {
            let f = reply(&s, "[]");
            feed(&mut s, f)
        };
        assert_eq!(
            ev,
            vec![MarketEvent::History {
                bars: vec![],
                as_of: Some(UtcMillis(iso("2026-11-06T14:05:00Z").0 - 1))
            }]
        );
        assert_eq!(
            out,
            vec![Outgoing::HttpGet(
                "https://x/products/BTC-USD/trades?limit=1000".into()
            )]
        );
    }

    #[test]
    fn long_history_is_paged_and_bounded() {
        let (s, _) = start(None, 1000 * 60_000);
        let candle_pages = s
            .queue
            .iter()
            .filter(|r| matches!(r, Request::Candles { .. }))
            .count();
        assert_eq!(candle_pages, 4, "1000 minutes = 4 pages of up to 300");
        let (s, _) = start(None, 100 * 24 * 60 * 60_000);
        let candle_pages = s
            .queue
            .iter()
            .filter(|r| matches!(r, Request::Candles { .. }))
            .count();
        assert_eq!(candle_pages, MAX_CANDLE_PAGES);
        // What the page budget can't reach is reported missing with the first response.
        let (mut s, _) = start(None, 100 * 24 * 60 * 60_000);
        let f = reply(&s, "[]");
        let (ev, _) = feed(&mut s, f);
        assert!(matches!(ev[0], MarketEvent::HistoryGap { .. }), "{ev:?}");
        // Reconnect within the same minute: only the forming bar's trades.
        let (s, _) = start(Some(iso("2026-11-06T14:05:00Z")), 1000 * 60_000);
        assert_eq!(s.queue.len(), 1);
    }

    #[test]
    fn rest_trades_rebuild_the_forming_bar_without_duplicates() {
        let mut s = session();
        {
            let f = reply(&s, "[]");
            feed(&mut s, f)
        }; // candles
        // Websocket trade 103 arrives before the REST trades response.
        let ev = text(
            &mut s,
            r#"{"type":"match","trade_id":103,"size":"1","price":"5","time":"2026-11-06T14:05:31Z"}"#,
        );
        assert_eq!(ev.len(), 1);
        let rest = r#"[
            {"trade_id":103,"side":"buy","size":"1","price":"5","time":"2026-11-06T14:05:31Z"},
            {"trade_id":102,"side":"buy","size":"2","price":"4","time":"2026-11-06T14:05:10Z"},
            {"trade_id":101,"side":"buy","size":"3","price":"3","time":"2026-11-06T14:05:00.000Z"},
            {"trade_id":100,"side":"buy","size":"9","price":"2","time":"2026-11-06T14:04:59.999Z"}
        ]"#;
        let f = reply(&s, rest);
        let (ev, out) = feed(&mut s, f);
        assert!(out.is_empty());
        // The forming bar as a closed snapshot: 101 (exactly on the bar boundary, so in the
        // forming bar) and 102; 100 is before the cutoff (in the candles). 103 is at the newest
        // REST millisecond and came via the websocket, so nothing is sent as a trade.
        assert!(
            !ev.iter().any(|e| matches!(e, MarketEvent::Trade { .. })),
            "{ev:?}"
        );
        assert_eq!(
            ev[0],
            MarketEvent::History {
                bars: vec![Bar {
                    time: iso("2026-11-06T14:05:00Z"),
                    open: Price(300),
                    high: Price(400),
                    low: Price(300),
                    close: Price(400),
                    volume: Qty(5_0000_0000),
                }],
                as_of: Some(UtcMillis(iso("2026-11-06T14:05:31Z").0 - 1)),
            }
        );
        assert_eq!(ev.last(), Some(&MarketEvent::HistoryComplete));
        // A late websocket duplicate of a REST trade is dropped.
        assert!(text(&mut s, r#"{"type":"match","trade_id":102,"size":"2","price":"4","time":"2026-11-06T14:05:10Z"}"#).is_empty());
        assert_eq!(text(&mut s, r#"{"type":"match","trade_id":104,"size":"1","price":"6","time":"2026-11-06T14:05:40Z"}"#).len(), 1);
    }

    #[test]
    fn trades_page_back_to_the_cutoff_or_report_a_gap() {
        let mut s = session();
        {
            let f = reply(&s, "[]");
            feed(&mut s, f)
        };
        let page = |first_id: u64| {
            let rows: Vec<String> = (0..TRADES_PER_PAGE as u64)
                .map(|i| {
                    format!(
                        r#"{{"trade_id":{},"size":"1","price":"1","time":"2026-11-06T14:05:20Z"}}"#,
                        first_id - i
                    )
                })
                .collect();
            format!("[{}]", rows.join(","))
        };
        text(
            &mut s,
            r#"{"type":"last_match","trade_id":50000,"size":"1","price":"1","time":"2026-11-06T14:05:20Z"}"#,
        );
        let f = reply(&s, &page(50_000));
        let (_, out) = feed(&mut s, f);
        assert_eq!(
            out,
            vec![Outgoing::HttpGet(
                "https://x/products/BTC-USD/trades?limit=1000&after=49001".into()
            )]
        );
        let mut last = Vec::new();
        for p in 1..MAX_TRADE_PAGES as u64 {
            let f = reply(&s, &page(50_000 - p * 1000));
            let (ev, _) = feed(&mut s, f);
            last = ev;
        }
        assert!(
            last.iter()
                .any(|e| matches!(e, MarketEvent::HistoryGap { .. })),
            "page budget exhausted"
        );
        assert_eq!(last.last(), Some(&MarketEvent::HistoryComplete));
    }

    fn trade_rows(ids: &[u64]) -> String {
        let rows: Vec<String> = ids
            .iter()
            .map(|id| {
                format!(
                    r#"{{"trade_id":{id},"size":"1","price":"1","time":"2026-11-06T14:05:{}Z"}}"#,
                    10 + id % 40
                )
            })
            .collect();
        format!("[{}]", rows.join(","))
    }

    #[test]
    fn rest_to_websocket_hole_gates_completion_until_filled() {
        let mut s = session();
        let f = reply(&s, "[]");
        feed(&mut s, f); // candles
        text(
            &mut s,
            r#"{"type":"last_match","trade_id":105,"size":"1","price":"1","time":"2026-11-06T14:05:20Z"}"#,
        );
        // REST is stale: newest 102, so 103..=105 came from neither source.
        let f = reply(&s, &trade_rows(&[102, 101]));
        let (ev, out) = feed(&mut s, f);
        assert!(!ev.contains(&MarketEvent::HistoryComplete));
        assert_eq!(
            out,
            vec![Outgoing::HttpGet(
                "https://x/products/BTC-USD/trades?limit=1000&after=106".into()
            )]
        );
        // Still stale: nothing new, wait for the keepalive tick.
        let f = reply(&s, &trade_rows(&[102, 101]));
        let (ev, out) = feed(&mut s, f);
        assert!(ev.is_empty() && out.is_empty(), "{ev:?}");
        let Some(Outgoing::HttpGet(url)) = s.keepalive(UtcMillis(0)) else {
            panic!()
        };
        assert!(url.ends_with("after=106"));
        assert_eq!(s.keepalive(UtcMillis(0)), None, "one fill in flight");
        let f = reply(&s, &trade_rows(&[105, 104, 103, 102]));
        let (ev, _) = feed(&mut s, f);
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, MarketEvent::Trade { .. }))
                .count(),
            3
        );
        assert_eq!(ev.last(), Some(&MarketEvent::HistoryComplete));
        assert_eq!(s.keepalive(UtcMillis(0)), None, "no holes left");
    }

    #[test]
    fn dropped_matches_are_fetched_by_id_or_reported() {
        let mut s = session();
        let f = reply(&s, "[]");
        feed(&mut s, f);
        text(
            &mut s,
            r#"{"type":"last_match","trade_id":99,"size":"1","price":"1","time":"2026-11-06T14:05:20Z"}"#,
        );
        let f = reply(&s, &trade_rows(&[99]));
        let (ev, _) = feed(&mut s, f);
        assert_eq!(ev.last(), Some(&MarketEvent::HistoryComplete));
        text(
            &mut s,
            r#"{"type":"match","trade_id":100,"size":"1","price":"1","time":"2026-11-06T14:05:31Z"}"#,
        );
        let (ev, out) = feed(
            &mut s,
            Frame::text(
                r#"{"type":"match","trade_id":104,"size":"1","price":"1","time":"2026-11-06T14:05:32Z"}"#,
            ),
        );
        assert_eq!(ev[0], MarketEvent::Gap { missed: 3 });
        assert_eq!(
            out,
            vec![Outgoing::HttpGet(
                "https://x/products/BTC-USD/trades?limit=1000&after=104".into()
            )]
        );
        // Partly filled: 102 arrives, 101 and 103 stay open.
        let f = reply(&s, &trade_rows(&[102]));
        let (ev, out) = feed(&mut s, f);
        assert_eq!(ev.len(), 1);
        assert_eq!(out.len(), 1, "progress: retry at once");
        // REST never has 101 or 103: each is retried, then reported.
        let mut gaps = 0;
        for _ in 0..3 * MAX_FILL_ATTEMPTS {
            if s.queue.is_empty() && s.keepalive(UtcMillis(0)).is_none() {
                break;
            }
            let f = reply(&s, "[]");
            let (ev, _) = feed(&mut s, f);
            gaps += ev
                .iter()
                .filter(|e| matches!(e, MarketEvent::HistoryGap { .. }))
                .count();
        }
        assert!(s.holes.is_empty(), "{:?}", s.holes);
        assert_eq!(gaps, 2);
        // A late websocket duplicate of a filled id is still dropped.
        assert!(text(&mut s, r#"{"type":"match","trade_id":102,"size":"1","price":"1","time":"2026-11-06T14:05:31Z"}"#).is_empty());
    }

    #[test]
    fn level2_batch_snapshot_and_updates_are_exact() {
        let mut s = session();
        let ev = text(
            &mut s,
            r#"{"type":"snapshot","product_id":"BTC-USD","bids":[["75415.71","0.5"],["75415.70","1.25"]],"asks":[["75415.72","0.00000001"]]}"#,
        );
        let lvl = |side, p, q| MarketEvent::BookLevel {
            side,
            price: Price(p),
            qty: Qty(q),
            orders: 0,
        };
        assert_eq!(
            ev,
            vec![
                MarketEvent::BookClear,
                lvl(Side::Bid, 7_541_571, 50_000_000),
                lvl(Side::Bid, 7_541_570, 125_000_000),
                lvl(Side::Ask, 7_541_572, 1),
            ]
        );
        let ev = text(
            &mut s,
            r#"{"type":"l2update","product_id":"BTC-USD","time":"2026-11-06T14:05:31.1Z","changes":[["buy","75415.71","0"],["sell","75415.73","2"],["sell","75415.735","1"]]}"#,
        );
        assert_eq!(
            ev,
            vec![
                lvl(Side::Bid, 7_541_571, 0),
                lvl(Side::Ask, 7_541_573, 200_000_000),
                MarketEvent::Rejected { count: 1 },
            ],
            "half a cent is not representable: counted, never rounded"
        );
    }

    #[test]
    fn matches_are_exact_and_sequenced() {
        let mut s = session();
        let ev = text(
            &mut s,
            r#"{"type":"match","trade_id":100,"maker_order_id":"a","taker_order_id":"b","side":"buy","size":"0.00123456","price":"75415.72","product_id":"BTC-USD","sequence":5,"time":"2026-11-06T14:05:31.123456Z"}"#,
        );
        assert_eq!(
            ev,
            vec![MarketEvent::Trade {
                time: iso("2026-11-06T14:05:31.123Z"),
                price: Price(7_541_572),
                qty: Qty(123_456),
                // The maker was buying: a seller hit the bid.
                aggressor: Some(Aggressor::Sell),
            }]
        );
        assert!(text(&mut s, r#"{"type":"match","trade_id":100,"size":"1","price":"1","time":"2026-11-06T14:05:31Z"}"#).is_empty());
        let ev = text(
            &mut s,
            r#"{"type":"match","trade_id":104,"size":"1","price":"1.5","time":"2026-11-06T14:05:32Z"}"#,
        );
        assert_eq!(ev[0], MarketEvent::Gap { missed: 3 });
        let ev = text(
            &mut s,
            r#"{"type":"match","trade_id":105,"size":"1","price":"1.005","time":"2026-11-06T14:05:33Z"}"#,
        );
        assert_eq!(ev, vec![MarketEvent::Rejected { count: 1 }]);
    }

    #[test]
    fn ticker_heartbeat_subscriptions_error() {
        let mut s = session();
        assert_eq!(
            text(&mut s, r#"{"type":"subscriptions","channels":[]}"#),
            vec![MarketEvent::Ready]
        );
        assert_eq!(
            text(
                &mut s,
                r#"{"type":"ticker","price":"10","best_bid":"75415.71","best_ask":"75415.72","product_id":"BTC-USD"}"#
            ),
            vec![
                MarketEvent::Bid(Price(7_541_571)),
                MarketEvent::Ask(Price(7_541_572))
            ]
        );
        text(
            &mut s,
            r#"{"type":"match","trade_id":10,"size":"1","price":"2","time":"2026-11-06T14:05:40Z"}"#,
        );
        assert_eq!(
            text(
                &mut s,
                r#"{"type":"heartbeat","last_trade_id":13,"product_id":"BTC-USD","sequence":1,"time":"x"}"#
            ),
            vec![MarketEvent::Gap { missed: 3 }]
        );
        assert!(text(&mut s, r#"{"type":"something_new"}"#).is_empty());
        let err = s
            .on_frame(
                &Frame::text(r#"{"type":"error","message":"Failed to subscribe","reason":"BTC-XYZ is not a valid product"}"#),
                UtcMillis(0),
                &mut Vec::new(),
                &mut Vec::new(),
            )
            .unwrap_err();
        assert!(!err.is_retryable());
    }

    #[test]
    fn candles_parse_exactly_within_their_window() {
        let mut s = session(); // window [09:05, 14:05)
        let t = |hm: &str| iso(&format!("2026-11-06T{hm}:00Z"));
        let body = format!(
            "[[{},75400,75500.5,75410.01,75450,1.5],[{},75415.72,75556.5,75523.22,75433.92,30.69151416],[{},1,2,1,2,1]]",
            t("14:05").0 / 1000,
            t("14:04").0 / 1000,
            t("14:03").0 / 1000,
        );
        let f = reply(&s, &body);
        let (ev, _) = feed(&mut s, f);
        let MarketEvent::History { bars, as_of } = &ev[0] else {
            panic!("{ev:?}")
        };
        assert_eq!(bars.len(), 2, "forming 14:05 bar excluded");
        assert_eq!(*as_of, Some(UtcMillis(t("14:05").0 - 1)));
        let b = bars[1];
        assert_eq!(b.time, t("14:04"));
        assert_eq!(
            (b.low, b.high, b.open, b.close),
            (
                Price(7_541_572),
                Price(7_555_650),
                Price(7_552_322),
                Price(7_543_392)
            )
        );
        assert_eq!(b.volume, Qty(3_069_151_416));
        assert!(bars[0].time < b.time, "sorted oldest first");
    }

    #[test]
    fn candles_url_with_a_hostile_granularity_is_a_protocol_error() {
        // Fuzz regression (libFuzzer, coinbase_session and recording): `granularity * 1000`
        // overflowed.
        for g in ["9223372036854775807", "0", "-60"] {
            let mut s = session();
            let f = Frame::http(format!(
                "https://x/products/BTC-USD/candles?granularity={g}&start=2026-11-06T14:00:00Z&end=2026-11-06T14:01:00Z
[]"
            ));
            let r = s.on_frame(&f, UtcMillis(0), &mut Vec::new(), &mut Vec::new());
            assert!(matches!(r, Err(SessionError::Protocol(_))), "{g}: {r:?}");
        }
    }

    #[test]
    fn candles_off_the_bar_grid_or_overflowing_are_rejected() {
        // Fuzz regression: an unaligned candle time reached BarSeries::upsert (a debug
        // assertion; a misaligned bar in release), and a huge one overflowed `* 1000`.
        let mut s = session(); // window [09:05, 14:05), 1-minute bars
        let t = iso("2026-11-06T14:00:00Z").0 / 1000;
        let body = format!(
            "[[{},1,2,1,2,1],[{},1,2,1,2,1],[9223372036854775807,1,2,1,2,1]]",
            t,
            t + 20
        );
        let f = reply(&s, &body);
        let (ev, _) = feed(&mut s, f);
        let MarketEvent::History { bars, .. } = &ev[0] else {
            panic!("{ev:?}")
        };
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].time.0, t * 1000);
        assert_eq!(ev[1], MarketEvent::Rejected { count: 2 });
    }
}
