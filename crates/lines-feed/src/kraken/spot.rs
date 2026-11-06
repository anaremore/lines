//! Kraken spot (WebSocket v2) protocol logic for one connection, free of I/O.
//!
//! # Connect sequence
//! 1. `AssetPairs` (REST): the pair's `pair_decimals`, `tick_size` and `lot_decimals` must
//!    equal the catalog product's price grid, tick and quantity decimals, or the connection
//!    fails with `BadMetadata` (decision 0002: adapters validate scales). Nothing is subscribed
//!    before this passes, so every number below is parsed on a validated grid.
//! 2. WebSocket subscriptions: `book` (depth 100), `trade` (with its snapshot of the last 50
//!    trades), `ticker` (`event_trigger: bbo`).
//! 3. History: `OHLC`, then `Trades` for the forming bar (below).
//!
//! # Book and checksum
//! Every book message carries Kraken's CRC-32 of the top 10 levels each side. The session keeps
//! its own copy of the book, truncated to the subscribed depth as Kraken requires, applies each
//! message, and compares checksums. On a mismatch it counts it, clears the book at once (a book
//! known to be wrong is never shown, `docs/design.md` §7), ignores further updates and
//! resubscribes for a fresh snapshot. Every verified message is counted too.
//!
//! # History and the stitch (roadmap 4)
//! `OHLC` returns only the newest 720 candles (12 hours of 1-minute bars) whatever `since`
//! asks; anything older is reported as a history gap. Its last row is Kraken's *uncommitted*
//! forming candle; its start is the boundary `b`. The committed rows are a closed snapshot
//! with `as_of = b - 1 ms`. `Trades` (complete, paged forward from `b`, `trade_id` sequential)
//! then rebuild the bars from `b`: with `t_n` the newest REST trade time, every REST trade
//! before `t_n` goes into a closed snapshot (`as_of = t_n - 1 ms`) and trades at `t_n` follow
//! as trades. WebSocket and REST trades overlap and are de-duplicated here by `trade_id`
//! before the store's time-based stitch. If REST lags so far that it ends before the first
//! WebSocket trade (ids in between), it is re-fetched on the keepalive tick and reported as a
//! history gap if it never closes; history is complete only once that hole is closed. A
//! WebSocket trade id that skips ahead mid-stream is counted as a gap.
//!
//! Each HTTP response is interpreted by its own URL where the URL decides the meaning (OHLC's
//! `since`), so a replay decodes it the same way whatever the replaying build's history window.

use std::collections::{BTreeSet, HashMap};

use lines_core::{Aggressor, Bar, Price, Product, Qty, Side, UtcMillis};
use serde::Deserialize;
use serde_json::value::RawValue;

use super::bars_from;
use super::book::Book;
use super::num::{seconds_to_ms, units};
use crate::provider::{
    Frame, FrameKind, MarketEvent, Outgoing, ProviderSession, SessionError, StartContext,
    query_param, split_http,
};

/// Book depth subscribed (Kraken allows 10, 25, 100, 500, 1000; the checksum covers 10).
pub const BOOK_DEPTH: usize = 100;
const TRADES_PER_PAGE: usize = 1000;
const MAX_TRADE_PAGES: u32 = 20;
const MAX_FILL_ATTEMPTS: u32 = 6;
/// Trade ids remembered for de-duplication.
const EMITTED_CAP: usize = 50_000;

#[derive(Copy, Clone, Debug)]
struct RestTrade {
    id: u64,
    time: UtcMillis,
    price: Price,
    qty: Qty,
    aggressor: Option<Aggressor>,
}

pub struct SpotSession {
    ws: &'static str,
    pair: &'static str,
    spec: Product,
    price_dec: u32,
    qty_dec: u32,
    rest_url: String,
    ctx: Option<StartContext>,
    subscribed: bool,
    ready: bool,
    book: Book,
    /// False from a (re)subscribe until its snapshot: updates in between are ignored.
    book_live: bool,
    /// Start of Kraken's uncommitted candle: REST trades rebuild the bars from here.
    boundary: Option<UtcMillis>,
    forming: Vec<RestTrade>,
    trade_pages: u32,
    /// `last` cursor of the newest REST trades page.
    rest_cursor: Option<String>,
    rest_done: bool,
    rest_max: Option<(u64, UtcMillis)>,
    ws_first: Option<(u64, UtcMillis)>,
    ws_last_id: Option<u64>,
    fill_attempts: u32,
    complete: bool,
    emitted: BTreeSet<u64>,
}

// ---- wire types ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(default)]
    channel: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(borrow, default)]
    data: Option<&'a RawValue>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct BookMsg<'a> {
    symbol: String,
    #[serde(borrow, default)]
    bids: Vec<LevelMsg<'a>>,
    #[serde(borrow, default)]
    asks: Vec<LevelMsg<'a>>,
    checksum: u32,
}

#[derive(Deserialize)]
struct LevelMsg<'a> {
    #[serde(borrow)]
    price: &'a RawValue,
    #[serde(borrow)]
    qty: &'a RawValue,
}

#[derive(Deserialize)]
struct TradeMsg<'a> {
    symbol: String,
    side: String,
    #[serde(borrow)]
    price: &'a RawValue,
    #[serde(borrow)]
    qty: &'a RawValue,
    trade_id: u64,
    timestamp: String,
}

#[derive(Deserialize)]
struct TickerMsg<'a> {
    symbol: String,
    #[serde(borrow, default)]
    bid: Option<&'a RawValue>,
    #[serde(borrow, default)]
    ask: Option<&'a RawValue>,
    #[serde(borrow, default)]
    last: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Rest<'a> {
    #[serde(default)]
    error: Vec<String>,
    #[serde(borrow, default)]
    result: Option<HashMap<String, &'a RawValue>>,
}

#[derive(Deserialize)]
struct PairInfo {
    pair_decimals: u32,
    lot_decimals: u32,
    #[serde(default)]
    tick_size: Option<String>,
}

fn bad_json(what: &str, e: impl std::fmt::Display) -> SessionError {
    SessionError::Protocol(format!("unexpected Kraken {what}: {e}"))
}

fn taker(side: &str) -> Option<Aggressor> {
    match side {
        "buy" | "b" => Some(Aggressor::Buy),
        "sell" | "s" => Some(Aggressor::Sell),
        _ => None,
    }
}

impl SpotSession {
    pub fn new(
        ws: &'static str,
        pair: &'static str,
        spec: Product,
        price_decimals: u32,
        rest_url: String,
    ) -> Self {
        SpotSession {
            ws,
            pair,
            price_dec: price_decimals,
            qty_dec: spec.qty.decimals as u32,
            spec,
            rest_url,
            ctx: None,
            subscribed: false,
            ready: false,
            book: Book::new(Some(BOOK_DEPTH)),
            book_live: false,
            boundary: None,
            forming: Vec::new(),
            trade_pages: 0,
            rest_cursor: None,
            rest_done: false,
            rest_max: None,
            ws_first: None,
            ws_last_id: None,
            fill_attempts: 0,
            complete: false,
            emitted: BTreeSet::new(),
        }
    }

    fn price(&self, raw: &str) -> Option<Price> {
        units(raw, self.price_dec).map(Price)
    }
    fn qty(&self, raw: &str) -> Option<Qty> {
        units(raw, self.qty_dec).map(Qty)
    }

    fn subscribe(&self, channel: &str, extra: &str) -> Outgoing {
        Outgoing::Frame(Frame::text(format!(
            r#"{{"method":"subscribe","params":{{"channel":"{channel}","symbol":["{}"]{extra}}}}}"#,
            self.ws
        )))
    }

    fn book_params(&self) -> String {
        format!(r#","depth":{BOOK_DEPTH},"snapshot":true"#)
    }

    fn trades_url(&self, since_ns: &str) -> String {
        format!(
            "{}/Trades?pair={}&since={since_ns}&count={TRADES_PER_PAGE}",
            self.rest_url, self.pair
        )
    }

    /// Remember an emitted trade id (bounded: oldest ids are forgotten first).
    fn mark(&mut self, id: u64) -> bool {
        let new = self.emitted.insert(id);
        while self.emitted.len() > EMITTED_CAP {
            self.emitted.pop_first();
        }
        new
    }

    // ---- REST ---------------------------------------------------------------------------

    fn on_http(
        &mut self,
        data: &[u8],
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        let (url, body) = split_http(data);
        let url = url.unwrap_or("");
        let rest: Rest = serde_json::from_slice(body).map_err(|e| bad_json("REST body", e))?;
        if !rest.error.is_empty() && rest.result.is_none() {
            // Rate limits and transient errors: history is retried or reported, never invented.
            tracing::warn!("Kraken REST error for {url}: {:?}", rest.error);
            if url.contains("/AssetPairs") {
                return Err(SessionError::Protocol(format!(
                    "Kraken AssetPairs failed: {:?}",
                    rest.error
                )));
            }
            if url.contains("/Trades") && !self.rest_done {
                self.finish_rest(events, out);
            }
            return Ok(());
        }
        let result = rest.result.unwrap_or_default();
        // The result map has the pair (under Kraken's internal name) and possibly `last`.
        let (cursor, rows) = (
            result
                .get("last")
                .map(|r| r.get().trim_matches('"').to_owned()),
            result
                .iter()
                .find(|(k, _)| k.as_str() != "last")
                .map(|(_, v)| *v),
        );
        if url.contains("/AssetPairs") {
            let info: PairInfo = serde_json::from_str(rows.map_or("null", |r| r.get()))
                .map_err(|e| bad_json("AssetPairs", e))?;
            self.check_metadata(&info)?;
            self.subscribed = true;
            out.push(self.subscribe("book", &self.book_params()));
            out.push(self.subscribe("trade", r#","snapshot":true"#));
            out.push(self.subscribe("ticker", r#","event_trigger":"bbo""#));
            if let Some(ctx) = self.ctx {
                out.push(Outgoing::HttpGet(self.ohlc_url(ctx)));
            }
        } else if url.contains("/OHLC") {
            let rows = rows.map_or("[]", |r| r.get());
            self.on_ohlc(url, rows, events, out)?;
        } else if url.contains("/Trades") {
            let rows = rows.map_or("[]", |r| r.get());
            self.on_trades(rows, cursor, events, out)?;
        }
        Ok(())
    }

    fn check_metadata(&self, info: &PairInfo) -> Result<(), SessionError> {
        let tick = info
            .tick_size
            .as_deref()
            .and_then(|t| units(t, self.price_dec));
        if info.pair_decimals != self.price_dec
            || info.lot_decimals != self.qty_dec
            || tick.is_some_and(|t| t != self.spec.price.tick)
        {
            return Err(SessionError::BadMetadata(format!(
                "Kraken {} has {} price decimals, tick {:?}, {} lot decimals; the catalog's {} has \
                 {}, {} units, {}",
                self.pair,
                info.pair_decimals,
                info.tick_size,
                info.lot_decimals,
                self.spec.symbol,
                self.price_dec,
                self.spec.price.tick,
                self.qty_dec
            )));
        }
        Ok(())
    }

    fn ohlc_url(&self, ctx: StartContext) -> String {
        let from = ctx
            .history_from
            .unwrap_or(UtcMillis(ctx.now.0 - ctx.history_ms));
        format!(
            "{}/OHLC?pair={}&interval={}&since={}",
            self.rest_url,
            self.pair,
            (ctx.bar_period_ms / 60_000).max(1),
            from.0.div_euclid(1000)
        )
    }

    fn on_ohlc(
        &mut self,
        url: &str,
        rows: &str,
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        type Row = (i64, String, String, String, String, String, String, i64);
        let rows: Vec<Row> = serde_json::from_str(rows).map_err(|e| bad_json("OHLC", e))?;
        let from = query_param(url, "since")
            .and_then(|s| s.parse::<i64>().ok())
            .map(|s| UtcMillis(s * 1000));
        let period = query_param(url, "interval")
            .and_then(|m| m.parse::<i64>().ok())
            .map_or(60_000, |m| m * 60_000);
        let Some(last) = rows.last() else {
            // No candles at all: rebuild from trades starting at the requested time.
            self.boundary = from;
            out.push(Outgoing::HttpGet(
                self.trades_url(&from.map_or(0, |f| f.0 * 1_000_000).to_string()),
            ));
            return Ok(());
        };
        let boundary = UtcMillis(last.0 * 1000);
        let mut bars = Vec::with_capacity(rows.len());
        let mut rejected = 0;
        for r in &rows[..rows.len() - 1] {
            let time = UtcMillis(r.0 * 1000);
            if from.is_some_and(|f| time < f.floor_to(period)) {
                continue;
            }
            match (
                self.price(&r.1),
                self.price(&r.2),
                self.price(&r.3),
                self.price(&r.4),
                self.qty(&r.6),
            ) {
                (Some(open), Some(high), Some(low), Some(close), Some(volume)) => bars.push(Bar {
                    time,
                    open,
                    high,
                    low,
                    close,
                    volume,
                }),
                _ => rejected += 1,
            }
        }
        if rejected > 0 {
            events.push(MarketEvent::Rejected { count: rejected });
        }
        // Kraken keeps only the newest 720 candles: older history is missing, not invented.
        let first = UtcMillis(rows[0].0 * 1000);
        if let Some(f) = from
            && first > f.floor_to(period)
        {
            events.push(MarketEvent::HistoryGap { from: f, to: first });
        }
        events.push(MarketEvent::History {
            bars,
            as_of: Some(UtcMillis(boundary.0 - 1)),
        });
        self.boundary = Some(boundary);
        out.push(Outgoing::HttpGet(
            self.trades_url(&(boundary.0 * 1_000_000).to_string()),
        ));
        Ok(())
    }

    fn parse_rest_trades(&self, rows: &str) -> Result<(Vec<RestTrade>, u64), SessionError> {
        let rows: Vec<Vec<&RawValue>> =
            serde_json::from_str(rows).map_err(|e| bad_json("Trades", e))?;
        let mut out = Vec::with_capacity(rows.len());
        let mut rejected = 0;
        for r in &rows {
            let field = |i: usize| r.get(i).map(|v| v.get()).unwrap_or("");
            let id = field(6).parse::<u64>();
            match (
                self.price(field(0)),
                self.qty(field(1)),
                seconds_to_ms(field(2)),
                id,
            ) {
                (Some(price), Some(qty), Some(time), Ok(id)) => out.push(RestTrade {
                    id,
                    time,
                    price,
                    qty,
                    aggressor: taker(field(3).trim_matches('"')),
                }),
                _ => rejected += 1,
            }
        }
        Ok((out, rejected))
    }

    fn on_trades(
        &mut self,
        rows: &str,
        cursor: Option<String>,
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        let (trades, rejected) = self.parse_rest_trades(rows)?;
        if rejected > 0 {
            events.push(MarketEvent::Rejected { count: rejected });
        }
        let full_page = trades.len() + rejected as usize >= TRADES_PER_PAGE;
        if let Some(c) = cursor {
            self.rest_cursor = Some(c);
        }
        for t in &trades {
            if self.rest_max.is_none_or(|(id, _)| t.id > id) {
                self.rest_max = Some((t.id, t.time));
            }
        }
        if !self.rest_done {
            let boundary = self.boundary.unwrap_or(UtcMillis(i64::MIN));
            self.forming
                .extend(trades.into_iter().filter(|t| t.time >= boundary));
            self.trade_pages += 1;
            match &self.rest_cursor {
                Some(c) if full_page && self.trade_pages < MAX_TRADE_PAGES => {
                    out.push(Outgoing::HttpGet(self.trades_url(c)));
                }
                _ => self.finish_rest(events, out),
            }
        } else {
            // A hole fill: anything new is a trade after the snapshot.
            let mut fresh: Vec<_> = trades
                .into_iter()
                .filter(|t| !self.emitted.contains(&t.id))
                .collect();
            fresh.sort_by_key(|t| t.id);
            for t in fresh {
                self.mark(t.id);
                events.push(MarketEvent::Trade {
                    time: t.time,
                    price: t.price,
                    qty: t.qty,
                    aggressor: t.aggressor,
                });
            }
            self.check_hole(events);
        }
        Ok(())
    }

    /// REST caught up: deliver the bars from the boundary as a closed snapshot, then the
    /// trades at the newest REST time as trades.
    fn finish_rest(&mut self, events: &mut Vec<MarketEvent>, _out: &mut Vec<Outgoing>) {
        self.rest_done = true;
        let mut forming = std::mem::take(&mut self.forming);
        forming.sort_by_key(|t| t.id);
        forming.dedup_by_key(|t| t.id);
        if let Some(t_n) = forming.iter().map(|t| t.time).max() {
            let period = self.ctx.map_or(60_000, |c| c.bar_period_ms);
            let mut before: Vec<_> = forming
                .iter()
                .filter(|t| t.time < t_n)
                .map(|t| (t.time, t.price, t.qty))
                .collect();
            events.push(MarketEvent::History {
                bars: bars_from(&mut before, period),
                as_of: Some(UtcMillis(t_n.0 - 1)),
            });
            for t in &forming {
                if t.time < t_n {
                    self.mark(t.id);
                } else if self.mark(t.id) {
                    events.push(MarketEvent::Trade {
                        time: t.time,
                        price: t.price,
                        qty: t.qty,
                        aggressor: t.aggressor,
                    });
                }
            }
        }
        self.check_hole(events);
    }

    /// History is complete once REST reaches the first WebSocket trade (consecutive ids).
    fn check_hole(&mut self, events: &mut Vec<MarketEvent>) {
        if self.complete || !self.rest_done {
            return;
        }
        let Some((ws_id, ws_time)) = self.ws_first else {
            return; // wait for the first WebSocket trade
        };
        let covered = self.rest_max.is_some_and(|(id, _)| id + 1 >= ws_id);
        if covered {
            self.complete = true;
            events.push(MarketEvent::HistoryComplete);
        } else if self.fill_attempts >= MAX_FILL_ATTEMPTS {
            let from = self
                .rest_max
                .map(|(_, t)| t)
                .or(self.boundary)
                .unwrap_or(ws_time);
            events.push(MarketEvent::HistoryGap { from, to: ws_time });
            self.complete = true;
            events.push(MarketEvent::HistoryComplete);
        }
    }

    // ---- WebSocket ----------------------------------------------------------------------

    fn on_text(
        &mut self,
        data: &[u8],
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        let env: Envelope =
            serde_json::from_slice(data).map_err(|e| bad_json("WebSocket message", e))?;
        if let Some(method) = env.method.as_deref() {
            if env.success == Some(false) {
                return Err(SessionError::Protocol(format!(
                    "Kraken {method} failed: {}",
                    env.error.unwrap_or_default()
                )));
            }
            if method == "subscribe" && !self.ready {
                self.ready = true;
                events.push(MarketEvent::Ready);
            }
            return Ok(());
        }
        let (Some(channel), Some(data)) = (env.channel.as_deref(), env.data) else {
            return Ok(()); // heartbeat
        };
        let ws = self.ws;
        match channel {
            "book" => {
                let msgs: Vec<BookMsg> =
                    serde_json::from_str(data.get()).map_err(|e| bad_json("book", e))?;
                let snapshot = env.kind.as_deref() == Some("snapshot");
                for m in msgs.iter().filter(|m| m.symbol == ws) {
                    self.on_book(m, snapshot, events, out);
                }
            }
            "trade" => {
                let msgs: Vec<TradeMsg> =
                    serde_json::from_str(data.get()).map_err(|e| bad_json("trade", e))?;
                for m in msgs.iter().filter(|m| m.symbol == ws) {
                    self.on_trade(m, events);
                }
                self.check_hole(events);
            }
            "ticker" => {
                let msgs: Vec<TickerMsg> =
                    serde_json::from_str(data.get()).map_err(|e| bad_json("ticker", e))?;
                for m in msgs.iter().filter(|m| m.symbol == ws) {
                    if let Some(p) = m.bid.and_then(|v| self.price(v.get())) {
                        events.push(MarketEvent::Bid(p));
                    }
                    if let Some(p) = m.ask.and_then(|v| self.price(v.get())) {
                        events.push(MarketEvent::Ask(p));
                    }
                    if let Some(p) = m.last.and_then(|v| self.price(v.get())) {
                        events.push(MarketEvent::LastPrice(p));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_book(
        &mut self,
        m: &BookMsg,
        snapshot: bool,
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) {
        if snapshot {
            self.book.clear();
            events.push(MarketEvent::BookClear);
            self.book_live = true;
        } else if !self.book_live {
            return; // awaiting the snapshot of a resubscription
        }
        let mut rejected = 0;
        for (side, levels) in [(Side::Bid, &m.bids), (Side::Ask, &m.asks)] {
            for l in levels {
                match (self.price(l.price.get()), self.qty(l.qty.get())) {
                    (Some(p), Some(q)) => self.book.set(side, p.0, q.0, events),
                    _ => rejected += 1,
                }
            }
        }
        if rejected > 0 {
            events.push(MarketEvent::Rejected { count: rejected });
        }
        self.book.truncate(events);
        if self.book.checksum() == m.checksum && rejected == 0 {
            events.push(MarketEvent::BookChecksumOk);
        } else {
            tracing::warn!(
                "Kraken {} book checksum mismatch (ours {:#010x}, Kraken {:#010x}); resyncing",
                self.ws,
                self.book.checksum(),
                m.checksum
            );
            events.push(MarketEvent::BookChecksumMismatch);
            self.resync(events, out);
        }
    }

    /// Drop the book (never show one known to be wrong) and resubscribe for a fresh snapshot.
    fn resync(&mut self, events: &mut Vec<MarketEvent>, out: &mut Vec<Outgoing>) {
        self.book.clear();
        self.book_live = false;
        events.push(MarketEvent::BookClear);
        events.push(MarketEvent::BookResync);
        out.push(Outgoing::Frame(Frame::text(format!(
            r#"{{"method":"unsubscribe","params":{{"channel":"book","symbol":["{}"],"depth":{BOOK_DEPTH}}}}}"#,
            self.ws
        ))));
        out.push(self.subscribe("book", &self.book_params()));
    }

    fn on_trade(&mut self, m: &TradeMsg, events: &mut Vec<MarketEvent>) {
        let (Some(price), Some(qty), Some(time)) = (
            self.price(m.price.get()),
            self.qty(m.qty.get()),
            UtcMillis::parse_iso(&m.timestamp),
        ) else {
            events.push(MarketEvent::Rejected { count: 1 });
            return;
        };
        if self.ws_first.is_none() {
            self.ws_first = Some((m.trade_id, time));
        }
        if let Some(last) = self.ws_last_id
            && m.trade_id > last + 1
        {
            events.push(MarketEvent::Gap {
                missed: m.trade_id - last - 1,
            });
        }
        self.ws_last_id = Some(self.ws_last_id.map_or(m.trade_id, |l| l.max(m.trade_id)));
        if self.mark(m.trade_id) {
            events.push(MarketEvent::Trade {
                time,
                price,
                qty,
                aggressor: taker(&m.side),
            });
        }
    }
}

impl ProviderSession for SpotSession {
    fn start(&mut self, ctx: StartContext, out: &mut Vec<Outgoing>) {
        self.ctx = Some(ctx);
        out.push(Outgoing::HttpGet(format!(
            "{}/AssetPairs?pair={}",
            self.rest_url, self.pair
        )));
    }

    fn on_frame(
        &mut self,
        frame: &Frame,
        _now: UtcMillis,
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        match frame.kind {
            FrameKind::Http => self.on_http(&frame.data, events, out),
            FrameKind::Text => {
                if !self.subscribed {
                    // Status messages arrive before our subscriptions; nothing else can.
                    return Ok(());
                }
                self.on_text(&frame.data, events, out)
            }
            FrameKind::Binary => Ok(()),
        }
    }

    /// Re-fetch REST trades while it lags behind the WebSocket (one request per tick).
    fn keepalive(&mut self, _now: UtcMillis) -> Option<Outgoing> {
        if self.complete || !self.rest_done || self.ws_first.is_none() {
            return None;
        }
        self.fill_attempts += 1;
        let cursor = self
            .rest_cursor
            .clone()
            .unwrap_or_else(|| self.boundary.map_or(0, |b| b.0 * 1_000_000).to_string());
        Some(Outgoing::HttpGet(self.trades_url(&cursor)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;
    use crate::kraken::num::book_checksum;

    const REST: &str = "https://rest.test/0/public";

    fn session() -> SpotSession {
        SpotSession::new(
            "BTC/USD",
            "XBTUSD",
            catalog::get(catalog::BTC_USD_KR).unwrap(),
            1,
            REST.into(),
        )
    }

    fn ctx(history_from: Option<UtcMillis>) -> StartContext {
        StartContext {
            now: UtcMillis(1_793_988_130_000),
            history_from,
            bar_period_ms: 60_000,
            history_ms: 3 * 86_400_000,
        }
    }

    fn http(url: &str, body: &str) -> Frame {
        Frame::http(format!("{url}\n{body}").into_bytes())
    }

    fn feed(
        s: &mut SpotSession,
        f: Frame,
    ) -> Result<(Vec<MarketEvent>, Vec<Outgoing>), SessionError> {
        let (mut ev, mut out) = (Vec::new(), Vec::new());
        s.on_frame(&f, UtcMillis(0), &mut ev, &mut out)?;
        Ok((ev, out))
    }

    fn text(s: &mut SpotSession, json: &str) -> (Vec<MarketEvent>, Vec<Outgoing>) {
        feed(s, Frame::text(json)).unwrap()
    }

    const PAIRS_OK: &str = r#"{"error":[],"result":{"XXBTZUSD":{"altname":"XBTUSD","wsname":"XBT/USD","pair_decimals":1,"lot_decimals":8,"tick_size":"0.1"}}}"#;

    /// A started, validated session (history from `history_from`).
    fn ready(history_from: Option<UtcMillis>) -> (SpotSession, Vec<Outgoing>) {
        let mut s = session();
        let mut out = Vec::new();
        s.start(ctx(history_from), &mut out);
        assert_eq!(
            out,
            [Outgoing::HttpGet(format!("{REST}/AssetPairs?pair=XBTUSD"))]
        );
        let (_, out) = feed(
            &mut s,
            http(&format!("{REST}/AssetPairs?pair=XBTUSD"), PAIRS_OK),
        )
        .unwrap();
        (s, out)
    }

    /// A book message with the checksum of `book` (asks ascending, bids descending), as
    /// (price, qty) in units. `levels` is the JSON for the message's own bids/asks.
    fn book_msg(kind: &str, bids: &str, asks: &str, check: u32) -> String {
        format!(
            r#"{{"channel":"book","type":"{kind}","data":[{{"symbol":"BTC/USD","bids":[{bids}],"asks":[{asks}],"checksum":{check},"timestamp":"2026-11-06T18:01:13.437282Z"}}]}}"#
        )
    }

    fn crc(asks: &[(i64, i64)], bids: &[(i64, i64)]) -> u32 {
        book_checksum(asks.iter().copied(), bids.iter().copied())
    }

    fn lvl(side: Side, price: i64, qty: i64) -> MarketEvent {
        MarketEvent::BookLevel {
            side,
            price: Price(price),
            qty: Qty(qty),
            orders: 0,
        }
    }

    #[test]
    fn metadata_gates_subscriptions() {
        let (_, out) = ready(None);
        let frames: Vec<_> = out
            .iter()
            .filter_map(|o| match o {
                Outgoing::Frame(f) => Some(String::from_utf8(f.data.to_vec()).unwrap()),
                _ => None,
            })
            .collect();
        assert_eq!(frames.len(), 3);
        assert!(frames[0].contains(r#""channel":"book""#) && frames[0].contains(r#""depth":100"#));
        assert!(
            frames[1].contains(r#""channel":"trade""#) && frames[1].contains(r#""snapshot":true"#)
        );
        assert!(frames[2].contains(r#""channel":"ticker""#) && frames[2].contains("bbo"));
        assert!(frames.iter().all(|f| f.contains(r#""symbol":["BTC/USD"]"#)));
        // History asks for 3 days back (OHLC will only have 12 hours of it).
        assert_eq!(
            out.last(),
            Some(&Outgoing::HttpGet(format!(
                "{REST}/OHLC?pair=XBTUSD&interval=1&since={}",
                (1_793_988_130_000i64 - 3 * 86_400_000) / 1000
            )))
        );

        // Kraken changing the pair's grid is fatal, not silently misparsed.
        let mut s = session();
        s.start(ctx(None), &mut Vec::new());
        let changed = PAIRS_OK.replace(r#""pair_decimals":1"#, r#""pair_decimals":2"#);
        let err = feed(
            &mut s,
            http(&format!("{REST}/AssetPairs?pair=XBTUSD"), &changed),
        )
        .unwrap_err();
        assert!(matches!(err, SessionError::BadMetadata(_)), "{err:?}");
        let mut s = session();
        s.start(ctx(None), &mut Vec::new());
        let changed = PAIRS_OK.replace(r#""lot_decimals":8"#, r#""lot_decimals":6"#);
        assert!(
            feed(
                &mut s,
                http(&format!("{REST}/AssetPairs?pair=XBTUSD"), &changed)
            )
            .is_err()
        );
        // Nothing before validation is decoded.
        let mut s = session();
        s.start(ctx(None), &mut Vec::new());
        assert_eq!(text(&mut s, &book_msg("snapshot", "", "", 0)).0, []);
    }

    #[test]
    fn book_is_exact_truncated_and_verified() {
        let (mut s, _) = ready(None);
        let asks = [(756_029, 149_890_116), (756_037, 13_879_137)];
        let bids = [(756_028, 7_607_609)];
        let (ev, out) = text(
            &mut s,
            &book_msg(
                "snapshot",
                r#"{"price":75602.8,"qty":0.07607609}"#,
                r#"{"price":75602.9,"qty":1.49890116},{"price":75603.7,"qty":0.13879137}"#,
                crc(&asks, &bids),
            ),
        );
        assert!(out.is_empty());
        assert_eq!(
            ev,
            [
                MarketEvent::BookClear,
                lvl(Side::Bid, 756_028, 7_607_609),
                lvl(Side::Ask, 756_029, 149_890_116),
                lvl(Side::Ask, 756_037, 13_879_137),
                MarketEvent::BookChecksumOk,
            ]
        );
        // An update: bid size changes, best ask removed.
        let asks = [(756_037, 13_879_137)];
        let bids = [(756_028, 55_008_441)];
        let (ev, _) = text(
            &mut s,
            &book_msg(
                "update",
                r#"{"price":75602.8,"qty":0.55008441}"#,
                r#"{"price":75602.9,"qty":0.0}"#,
                crc(&asks, &bids),
            ),
        );
        assert_eq!(
            ev,
            [
                lvl(Side::Bid, 756_028, 55_008_441),
                lvl(Side::Ask, 756_029, 0),
                MarketEvent::BookChecksumOk,
            ]
        );
        // Kraken leaves levels pushed past depth 100 to the client: we drop them ourselves.
        let many: String = (0..101)
            .map(|i| format!(r#"{{"price":{}.0,"qty":0.1}}"#, 70_000 - i))
            .collect::<Vec<_>>()
            .join(",");
        let kept: Vec<_> = (0..100).map(|i| ((70_000 - i) * 10, 10_000_000)).collect();
        let (ev, _) = text(&mut s, &book_msg("snapshot", &many, "", crc(&[], &kept)));
        assert!(
            ev.contains(&lvl(Side::Bid, 699_000, 0)),
            "101st level truncated"
        );
        assert_eq!(ev.last(), Some(&MarketEvent::BookChecksumOk));
        assert_eq!(s.book.levels(Side::Bid).len(), 100);
    }

    #[test]
    fn checksum_mismatch_clears_resubscribes_and_ignores_until_a_new_snapshot() {
        let (mut s, _) = ready(None);
        let asks = [(1_001, 100_000_000)];
        let bids = [(1_000, 100_000_000)];
        text(
            &mut s,
            &book_msg(
                "snapshot",
                r#"{"price":100.0,"qty":1.0}"#,
                r#"{"price":100.1,"qty":1.0}"#,
                crc(&asks, &bids),
            ),
        );
        // A corrupted update: the checksum is for a different book.
        let (ev, out) = text(
            &mut s,
            &book_msg("update", r#"{"price":100.0,"qty":2.0}"#, "", 12_345),
        );
        assert_eq!(
            ev,
            [
                lvl(Side::Bid, 1_000, 200_000_000),
                MarketEvent::BookChecksumMismatch,
                MarketEvent::BookClear,
                MarketEvent::BookResync,
            ]
        );
        let sent: Vec<_> = out
            .iter()
            .map(|o| match o {
                Outgoing::Frame(f) => String::from_utf8(f.data.to_vec()).unwrap(),
                o => panic!("{o:?}"),
            })
            .collect();
        assert!(sent[0].starts_with(r#"{"method":"unsubscribe""#) && sent[0].contains("book"));
        assert!(
            sent[1].starts_with(r#"{"method":"subscribe""#)
                && sent[1].contains(r#""snapshot":true"#)
        );
        assert!(s.book.is_empty(), "a book known to be wrong is never kept");
        // Updates still in flight from the old subscription are ignored.
        let (ev, out) = text(
            &mut s,
            &book_msg("update", r#"{"price":99.0,"qty":1.0}"#, "", 0),
        );
        assert!(ev.is_empty() && out.is_empty());
        // The fresh snapshot restores a verified book.
        let (ev, _) = text(
            &mut s,
            &book_msg(
                "snapshot",
                r#"{"price":100.0,"qty":2.0}"#,
                r#"{"price":100.1,"qty":1.0}"#,
                crc(&asks, &[(1_000, 200_000_000)]),
            ),
        );
        assert_eq!(ev.first(), Some(&MarketEvent::BookClear));
        assert_eq!(ev.last(), Some(&MarketEvent::BookChecksumOk));
    }

    #[test]
    fn off_grid_values_are_rejected_and_resynced() {
        let (mut s, _) = ready(None);
        let (ev, _) = text(
            &mut s,
            &book_msg("snapshot", r#"{"price":100.05,"qty":1.0}"#, "", 0),
        );
        assert!(ev.contains(&MarketEvent::Rejected { count: 1 }));
        assert!(
            ev.contains(&MarketEvent::BookResync),
            "an unrepresentable level resyncs"
        );
    }

    fn trade(id: u64, price: &str, qty: &str, side: &str, ts: &str) -> String {
        format!(
            r#"{{"channel":"trade","type":"update","data":[{{"symbol":"BTC/USD","side":"{side}","price":{price},"qty":{qty},"ord_type":"market","trade_id":{id},"timestamp":"{ts}"}}]}}"#
        )
    }

    #[test]
    fn trades_are_exact_with_taker_side_and_gaps_counted() {
        let (mut s, _) = ready(None);
        let (ev, _) = text(
            &mut s,
            &trade(
                10,
                "75602.8",
                "0.01322747",
                "sell",
                "2026-11-06T18:01:14.138026Z",
            ),
        );
        assert_eq!(
            ev,
            [MarketEvent::Trade {
                time: UtcMillis::parse_iso("2026-11-06T18:01:14.138Z").unwrap(),
                price: Price(756_028),
                qty: Qty(1_322_747),
                aggressor: Some(Aggressor::Sell),
            }]
        );
        assert_eq!(
            text(
                &mut s,
                &trade(10, "75602.8", "0.1", "buy", "2026-11-06T18:01:15Z")
            )
            .0,
            [],
            "duplicate id"
        );
        let (ev, _) = text(
            &mut s,
            &trade(13, "75603.0", "0.1", "buy", "2026-11-06T18:01:16Z"),
        );
        assert_eq!(ev[0], MarketEvent::Gap { missed: 2 });
        assert!(matches!(
            ev[1],
            MarketEvent::Trade {
                aggressor: Some(Aggressor::Buy),
                ..
            }
        ));
    }

    fn ohlc_row(t: i64, p: &str) -> String {
        format!(r#"[{t},"{p}","{p}","{p}","{p}","{p}","0.50000000",3]"#)
    }

    fn rest_trade(id: u64, secs: &str, price: &str) -> String {
        format!(r#"["{price}","0.10000000",{secs},"b","l","",{id}]"#)
    }

    #[test]
    fn history_ohlc_then_trades_rebuild_the_forming_bar() {
        let (mut s, out) = ready(None);
        let Some(Outgoing::HttpGet(ohlc)) = out.last().cloned() else {
            panic!()
        };
        // Three committed candles and Kraken's uncommitted one (its start is the boundary).
        let b = 1_793_988_060i64;
        let body = format!(
            r#"{{"error":[],"result":{{"XXBTZUSD":[{},{},{},{}],"last":{}}}}}"#,
            ohlc_row(b - 180, "100.0"),
            ohlc_row(b - 120, "100.1"),
            ohlc_row(b - 60, "100.2"),
            ohlc_row(b, "100.3"),
            b - 60
        );
        let (ev, out) = feed(&mut s, http(&ohlc, &body)).unwrap();
        // Older than Kraken's 720-candle window: reported, not invented.
        assert!(matches!(ev[0], MarketEvent::HistoryGap { .. }));
        let MarketEvent::History { bars, as_of } = &ev[1] else {
            panic!("{ev:?}")
        };
        assert_eq!(bars.len(), 3, "the uncommitted candle is not history");
        assert_eq!(bars[2].close, Price(1_002));
        assert_eq!(bars[2].volume, Qty(50_000_000));
        assert_eq!(*as_of, Some(UtcMillis(b * 1000 - 1)));
        assert_eq!(
            out,
            [Outgoing::HttpGet(format!(
                "{REST}/Trades?pair=XBTUSD&since={}&count=1000",
                b * 1_000_000_000
            ))]
        );

        // REST trades from the boundary: 3 before the newest time, 2 at it.
        let Outgoing::HttpGet(trades_url) = out[0].clone() else {
            panic!()
        };
        let rows = [
            rest_trade(500, &format!("{b}.100"), "100.3"),
            rest_trade(501, &format!("{}.250", b + 30), "100.5"),
            rest_trade(502, &format!("{}.999", b + 61), "100.4"),
            rest_trade(503, &format!("{}.700", b + 62), "100.6"),
            rest_trade(504, &format!("{}.700", b + 62), "100.7"),
        ]
        .join(",");
        // The websocket already delivered 504 (its snapshot starts there).
        let (ev, _) = text(
            &mut s,
            &trade(504, "100.7", "0.1", "buy", "2026-11-06T18:02:02.700Z"),
        );
        assert_eq!(ev.len(), 1);
        let body = format!(r#"{{"error":[],"result":{{"XXBTZUSD":[{rows}],"last":"1"}}}}"#);
        let (ev, out) = feed(&mut s, http(&trades_url, &body)).unwrap();
        assert!(out.is_empty(), "a short page means REST has caught up");
        let MarketEvent::History { bars, as_of } = &ev[0] else {
            panic!("{ev:?}")
        };
        assert_eq!(*as_of, Some(UtcMillis((b + 62) * 1000 + 700 - 1)));
        assert_eq!(bars.len(), 2);
        assert_eq!((bars[0].open, bars[0].close), (Price(1_003), Price(1_005)));
        assert_eq!(bars[1].close, Price(1_004));
        // 503 is at the newest time: a trade. 504 came from the websocket already.
        let trades: Vec<_> = ev
            .iter()
            .filter(|e| matches!(e, MarketEvent::Trade { .. }))
            .collect();
        assert_eq!(trades.len(), 1);
        assert!(matches!(
            trades[0],
            MarketEvent::Trade {
                price: Price(1_006),
                ..
            }
        ));
        assert_eq!(
            ev.last(),
            Some(&MarketEvent::HistoryComplete),
            "REST reached the websocket"
        );
    }

    #[test]
    fn rest_lagging_behind_the_websocket_is_refetched_then_reported() {
        let (mut s, out) = ready(Some(UtcMillis(1_793_988_000_000)));
        let Some(Outgoing::HttpGet(ohlc)) = out.last().cloned() else {
            panic!()
        };
        let b = 1_793_988_060i64;
        let body = format!(
            r#"{{"error":[],"result":{{"XXBTZUSD":[{},{}],"last":0}}}}"#,
            ohlc_row(b - 60, "100.0"),
            ohlc_row(b, "100.0")
        );
        let (ev, out) = feed(&mut s, http(&ohlc, &body)).unwrap();
        assert!(
            !ev.iter()
                .any(|e| matches!(e, MarketEvent::HistoryGap { .. })),
            "within the window"
        );
        let Outgoing::HttpGet(trades_url) = out[0].clone() else {
            panic!()
        };
        // The websocket starts at id 600, REST ends at 590: ids 591..599 are missing so far.
        text(
            &mut s,
            &trade(600, "100.0", "0.1", "buy", "2026-11-06T18:02:10Z"),
        );
        let body = format!(
            r#"{{"error":[],"result":{{"XXBTZUSD":[{}],"last":"77"}}}}"#,
            rest_trade(590, &format!("{b}.5"), "100.0")
        );
        let (ev, _) = feed(&mut s, http(&trades_url, &body)).unwrap();
        assert!(!ev.contains(&MarketEvent::HistoryComplete));
        // Each keepalive re-fetches from the cursor.
        let fill = s.keepalive(UtcMillis(0));
        assert_eq!(
            fill,
            Some(Outgoing::HttpGet(format!(
                "{REST}/Trades?pair=XBTUSD&since=77&count=1000"
            )))
        );
        // REST catches up with the missing ids: they arrive as trades and history completes.
        let rows: Vec<_> = (591..=600)
            .map(|id| rest_trade(id, &format!("{}.5", b + 5), "100.0"))
            .collect();
        let body = format!(
            r#"{{"error":[],"result":{{"XXBTZUSD":[{}],"last":"78"}}}}"#,
            rows.join(",")
        );
        let Some(Outgoing::HttpGet(url)) = fill else {
            panic!()
        };
        let (ev, _) = feed(&mut s, http(&url, &body)).unwrap();
        let trades = ev
            .iter()
            .filter(|e| matches!(e, MarketEvent::Trade { .. }))
            .count();
        assert_eq!(trades, 9, "591..599; 600 was already delivered");
        assert_eq!(ev.last(), Some(&MarketEvent::HistoryComplete));
        assert_eq!(s.keepalive(UtcMillis(0)), None);

        // If REST never catches up, the hole is reported after a bounded number of tries.
        let (mut s, out) = ready(Some(UtcMillis(1_793_988_000_000)));
        let Some(Outgoing::HttpGet(ohlc)) = out.last().cloned() else {
            panic!()
        };
        let body = format!(
            r#"{{"error":[],"result":{{"XXBTZUSD":[{}],"last":0}}}}"#,
            ohlc_row(b, "100.0")
        );
        let (_, out) = feed(&mut s, http(&ohlc, &body)).unwrap();
        let Outgoing::HttpGet(trades_url) = out[0].clone() else {
            panic!()
        };
        text(
            &mut s,
            &trade(600, "100.0", "0.1", "buy", "2026-11-06T18:02:10Z"),
        );
        let empty = r#"{"error":[],"result":{"XXBTZUSD":[],"last":"5"}}"#;
        feed(&mut s, http(&trades_url, empty)).unwrap();
        let mut reported = Vec::new();
        while let Some(Outgoing::HttpGet(url)) = s.keepalive(UtcMillis(0)) {
            let (ev, _) = feed(&mut s, http(&url, empty)).unwrap();
            reported.extend(ev);
        }
        assert!(
            reported
                .iter()
                .any(|e| matches!(e, MarketEvent::HistoryGap { .. }))
        );
        assert_eq!(reported.last(), Some(&MarketEvent::HistoryComplete));
    }
}
