//! Kraken Futures perpetuals (public WebSocket v1 feed) protocol logic, free of I/O.
//!
//! # Connect sequence
//! 1. `derivatives/api/v3/instruments` (REST): the perpetual's `tickSize` and
//!    `contractValueTradePrecision` must equal the catalog product's tick and quantity
//!    decimals, or the connection fails with `BadMetadata`. Subscriptions follow only then.
//! 2. WebSocket: `book` (a full-depth snapshot, then one level per message with a sequence
//!    number), `trade` (a snapshot of the last 100 trades, newest first, then trades), `ticker`.
//! 3. History: 1-minute candles, then REST trades for the forming bars.
//!
//! # Book integrity
//! The Futures book has **no checksum**; its integrity check is the sequence number, which must
//! advance by exactly one per book message. A skipped number means a lost update: the session
//! clears the book (never shows one known to be wrong) and resubscribes for a fresh snapshot,
//! counted as a resync (not as a trade `Gap`: no trades were lost). The spot feed's CRC-32 check is the stronger test; this is the best the
//! Futures feed allows.
//!
//! # History
//! Candles (`api/charts/v1/trade/{symbol}/1m`, paged forward with `more_candles`) up to the
//! boundary `b` = one bar before the bar forming at connect (Kraken's newest candles can lag
//! their last trades), as a closed snapshot with `as_of = b - 1 ms`. The REST trade history
//! (`derivatives/api/v3/history`, newest 100 per page, paged backwards with `lastTime`) then
//! rebuilds the bars from `b`: every REST trade before the newest REST time `t_n` is a closed
//! snapshot (`as_of = t_n - 1 ms`), trades at `t_n` follow as trades. WebSocket and REST
//! trades are de-duplicated by `uid`. Futures trade ids carry no order, so continuity is
//! checked by time: if the newest REST trade is older than the oldest trade in the
//! WebSocket's snapshot, REST is re-fetched on the keepalive tick and the interval is reported
//! as a history gap if it never closes.

use std::collections::{HashSet, VecDeque};

use lines_core::{Aggressor, Bar, Price, Product, Qty, Side, UtcMillis};
use serde::Deserialize;
use serde_json::value::RawValue;

use super::bars_from;
use super::book::Book;
use super::num::{units, units_from_float_text};
use crate::provider::{
    Frame, FrameKind, MarketEvent, Outgoing, ProviderSession, SessionError, StartContext,
    query_param, split_http,
};

const MAX_CANDLE_PAGES: u32 = 30;
const MAX_TRADE_PAGES: u32 = 30;
const MAX_FILL_ATTEMPTS: u32 = 6;
const UIDS_CAP: usize = 50_000;

#[derive(Clone, Debug)]
struct Fill {
    uid: String,
    time: UtcMillis,
    price: Price,
    qty: Qty,
    aggressor: Option<Aggressor>,
}

pub struct FuturesSession {
    symbol: &'static str,
    spec: Product,
    price_dec: u32,
    qty_dec: u32,
    rest_url: String,
    ctx: Option<StartContext>,
    subscribed: bool,
    ready: bool,
    book: Book,
    book_seq: Option<u64>,
    book_live: bool,
    boundary: Option<UtcMillis>,
    candle_pages: u32,
    trade_pages: u32,
    forming: Vec<Fill>,
    rest_done: bool,
    rest_newest: Option<UtcMillis>,
    ws_oldest: Option<UtcMillis>,
    fill_attempts: u32,
    complete: bool,
    uids: HashSet<String>,
    uid_order: VecDeque<String>,
}

// ---- wire types ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Msg<'a> {
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    feed: Option<String>,
    #[serde(default)]
    product_id: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default)]
    side: Option<String>,
    #[serde(borrow, default)]
    price: Option<&'a RawValue>,
    #[serde(borrow, default)]
    qty: Option<&'a RawValue>,
    #[serde(borrow, default)]
    bids: Vec<LevelMsg<'a>>,
    #[serde(borrow, default)]
    asks: Vec<LevelMsg<'a>>,
    #[serde(borrow, default)]
    trades: Vec<WsTrade<'a>>,
    #[serde(default)]
    uid: Option<String>,
    #[serde(default)]
    time: Option<i64>,
    #[serde(borrow, default)]
    bid: Option<&'a RawValue>,
    #[serde(borrow, default)]
    ask: Option<&'a RawValue>,
    #[serde(borrow, default)]
    last: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct LevelMsg<'a> {
    #[serde(borrow)]
    price: &'a RawValue,
    #[serde(borrow)]
    qty: &'a RawValue,
}

#[derive(Deserialize)]
struct WsTrade<'a> {
    uid: String,
    side: String,
    time: i64,
    #[serde(borrow)]
    price: &'a RawValue,
    #[serde(borrow)]
    qty: &'a RawValue,
}

#[derive(Deserialize)]
struct Instruments<'a> {
    #[serde(borrow, default)]
    instruments: Vec<Instrument<'a>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Instrument<'a> {
    symbol: String,
    #[serde(borrow, default)]
    tick_size: Option<&'a RawValue>,
    #[serde(default)]
    contract_value_trade_precision: Option<i64>,
}

#[derive(Deserialize)]
struct Candles {
    #[serde(default)]
    candles: Vec<Candle>,
    #[serde(default)]
    more_candles: bool,
}

#[derive(Deserialize)]
struct Candle {
    time: i64,
    open: String,
    high: String,
    low: String,
    close: String,
    volume: String,
}

#[derive(Deserialize)]
struct History<'a> {
    #[serde(borrow, default)]
    history: Vec<RestTrade<'a>>,
}

#[derive(Deserialize)]
struct RestTrade<'a> {
    time: String,
    uid: String,
    side: String,
    #[serde(borrow)]
    price: &'a RawValue,
    #[serde(borrow)]
    size: &'a RawValue,
}

fn bad_json(what: &str, e: impl std::fmt::Display) -> SessionError {
    SessionError::Protocol(format!("unexpected Kraken Futures {what}: {e}"))
}

fn taker(side: &str) -> Option<Aggressor> {
    match side {
        "buy" => Some(Aggressor::Buy),
        "sell" => Some(Aggressor::Sell),
        _ => None,
    }
}

impl FuturesSession {
    pub fn new(symbol: &'static str, spec: Product, price_decimals: u32, rest_url: String) -> Self {
        FuturesSession {
            symbol,
            price_dec: price_decimals,
            qty_dec: spec.qty.decimals as u32,
            spec,
            rest_url,
            ctx: None,
            subscribed: false,
            ready: false,
            book: Book::new(None),
            book_seq: None,
            book_live: false,
            boundary: None,
            candle_pages: 0,
            trade_pages: 0,
            forming: Vec::new(),
            rest_done: false,
            rest_newest: None,
            ws_oldest: None,
            fill_attempts: 0,
            complete: false,
            uids: HashSet::new(),
            uid_order: VecDeque::new(),
        }
    }

    fn price(&self, raw: &str) -> Option<Price> {
        units(raw, self.price_dec).map(Price)
    }
    fn qty(&self, raw: &str) -> Option<Qty> {
        units(raw, self.qty_dec).map(Qty)
    }

    fn feed(&self, event: &str, feed: &str) -> Outgoing {
        Outgoing::Frame(Frame::text(format!(
            r#"{{"event":"{event}","feed":"{feed}","product_ids":["{}"]}}"#,
            self.symbol
        )))
    }

    fn mark(&mut self, uid: &str) -> bool {
        if !self.uids.insert(uid.to_owned()) {
            return false;
        }
        self.uid_order.push_back(uid.to_owned());
        while self.uid_order.len() > UIDS_CAP {
            if let Some(old) = self.uid_order.pop_front() {
                self.uids.remove(&old);
            }
        }
        true
    }

    fn candles_url(&self, from_s: i64, to_s: i64) -> String {
        format!(
            "{}/api/charts/v1/trade/{}/1m?from={from_s}&to={to_s}",
            self.rest_url, self.symbol
        )
    }

    fn history_url(&self, before: Option<&str>) -> String {
        match before {
            Some(t) => format!(
                "{}/derivatives/api/v3/history?symbol={}&lastTime={t}",
                self.rest_url, self.symbol
            ),
            None => format!(
                "{}/derivatives/api/v3/history?symbol={}",
                self.rest_url, self.symbol
            ),
        }
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
        if url.contains("/instruments") {
            let all: Instruments =
                serde_json::from_slice(body).map_err(|e| bad_json("instruments", e))?;
            let me = all
                .instruments
                .iter()
                .find(|i| i.symbol.eq_ignore_ascii_case(self.symbol))
                .ok_or_else(|| {
                    SessionError::Unavailable(format!("Kraken Futures lists no {}", self.symbol))
                })?;
            self.check_metadata(me)?;
            self.subscribed = true;
            out.push(self.feed("subscribe", "book"));
            out.push(self.feed("subscribe", "trade"));
            out.push(self.feed("subscribe", "ticker"));
            if let Some(ctx) = self.ctx {
                let cutoff = ctx.now.floor_to(ctx.bar_period_ms);
                let boundary = UtcMillis(cutoff.0 - ctx.bar_period_ms);
                self.boundary = Some(boundary);
                let from = ctx
                    .history_from
                    .unwrap_or(UtcMillis(ctx.now.0 - ctx.history_ms));
                if from < boundary {
                    out.push(Outgoing::HttpGet(
                        self.candles_url(from.0.div_euclid(1000), boundary.0 / 1000 - 1),
                    ));
                } else {
                    out.push(Outgoing::HttpGet(self.history_url(None)));
                }
            }
        } else if url.contains("/api/charts/") {
            self.on_candles(url, body, events, out)?;
        } else if url.contains("/history") {
            self.on_history(body, events, out)?;
        }
        Ok(())
    }

    fn check_metadata(&self, i: &Instrument) -> Result<(), SessionError> {
        let tick = i.tick_size.and_then(|t| units(t.get(), self.price_dec));
        if tick != Some(self.spec.price.tick)
            || i.contract_value_trade_precision != Some(self.qty_dec as i64)
        {
            return Err(SessionError::BadMetadata(format!(
                "Kraken Futures {} has tick {:?} and {:?} size decimals; the catalog's {} has \
                 {} units at {} decimals and {} size decimals",
                self.symbol,
                i.tick_size.map(|t| t.get()),
                i.contract_value_trade_precision,
                self.spec.symbol,
                self.spec.price.tick,
                self.price_dec,
                self.qty_dec
            )));
        }
        Ok(())
    }

    fn on_candles(
        &mut self,
        url: &str,
        body: &[u8],
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        let page: Candles = serde_json::from_slice(body).map_err(|e| bad_json("candles", e))?;
        // The request's own range decides the snapshot (0011: interpret by URL).
        let to = query_param(url, "to")
            .and_then(|s| s.parse::<i64>().ok())
            .map(|s| UtcMillis((s + 1) * 1000));
        let mut bars = Vec::with_capacity(page.candles.len());
        let mut rejected = 0;
        for c in &page.candles {
            let time = UtcMillis(c.time);
            if to.is_some_and(|t| time >= t) {
                continue;
            }
            match (
                self.price(&c.open),
                self.price(&c.high),
                self.price(&c.low),
                self.price(&c.close),
                // Kraken computes candle volume as a binary-float sum and prints it to 16
                // significant digits ("0.3359999999999999" for 0.336). Only this field is
                // snapped, and only within float noise of the grid (as Rithmic prices are,
                // decision 0002); anything genuinely off-grid is still rejected.
                units_from_float_text(&c.volume, self.qty_dec).map(Qty),
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
        let newest = bars.last().map(|b| b.time);
        // A page is closed at its own end: the requested `to` if it is the last page, else the
        // end of its newest candle (the next page continues from there).
        let end = match (page.more_candles, newest) {
            (true, Some(n)) => Some(UtcMillis(n.0 + 60_000)),
            _ => to,
        };
        if let Some(end) = end {
            events.push(MarketEvent::History {
                bars,
                as_of: Some(UtcMillis(end.0 - 1)),
            });
        }
        self.candle_pages += 1;
        match (page.more_candles, newest, to) {
            (true, Some(n), Some(t))
                if self.candle_pages < MAX_CANDLE_PAGES && n.0 + 60_000 < t.0 =>
            {
                out.push(Outgoing::HttpGet(
                    self.candles_url((n.0 + 60_000) / 1000, t.0 / 1000 - 1),
                ));
            }
            _ => out.push(Outgoing::HttpGet(self.history_url(None))),
        }
        Ok(())
    }

    fn parse_rest(&self, body: &[u8]) -> Result<(Vec<Fill>, u64, Option<String>), SessionError> {
        let page: History = serde_json::from_slice(body).map_err(|e| bad_json("history", e))?;
        let mut fills = Vec::with_capacity(page.history.len());
        let mut rejected = 0;
        let mut oldest_raw: Option<(UtcMillis, String)> = None;
        for t in &page.history {
            let time = UtcMillis::parse_iso(&t.time);
            match (self.price(t.price.get()), self.qty(t.size.get()), time) {
                (Some(price), Some(qty), Some(time)) => {
                    if oldest_raw.as_ref().is_none_or(|(o, _)| time < *o) {
                        oldest_raw = Some((time, t.time.clone()));
                    }
                    fills.push(Fill {
                        uid: t.uid.clone(),
                        time,
                        price,
                        qty,
                        aggressor: taker(&t.side),
                    })
                }
                _ => rejected += 1,
            }
        }
        Ok((fills, rejected, oldest_raw.map(|(_, s)| s)))
    }

    fn on_history(
        &mut self,
        body: &[u8],
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        let (fills, rejected, oldest_raw) = self.parse_rest(body)?;
        if rejected > 0 {
            events.push(MarketEvent::Rejected { count: rejected });
        }
        if let Some(n) = fills.iter().map(|f| f.time).max()
            && self.rest_newest.is_none_or(|r| n > r)
        {
            self.rest_newest = Some(n);
        }
        if self.rest_done {
            // Re-fetch while REST lagged: new fills are trades after the snapshot.
            let mut fresh: Vec<_> = fills
                .into_iter()
                .filter(|f| !self.uids.contains(&f.uid))
                .collect();
            fresh.sort_by_key(|f| f.time);
            for f in fresh {
                self.mark(&f.uid);
                events.push(MarketEvent::Trade {
                    time: f.time,
                    price: f.price,
                    qty: f.qty,
                    aggressor: f.aggressor,
                });
            }
            self.check_hole(events);
            return Ok(());
        }
        let boundary = self.boundary.unwrap_or(UtcMillis(i64::MIN));
        let oldest = fills.iter().map(|f| f.time).min();
        self.forming
            .extend(fills.into_iter().filter(|f| f.time >= boundary));
        self.trade_pages += 1;
        match (oldest, oldest_raw) {
            (Some(o), Some(raw)) if o >= boundary && self.trade_pages < MAX_TRADE_PAGES => {
                out.push(Outgoing::HttpGet(self.history_url(Some(&raw))));
            }
            (Some(o), _) if o >= boundary => {
                // Paged as far as allowed without reaching the boundary.
                events.push(MarketEvent::HistoryGap {
                    from: boundary,
                    to: o,
                });
                self.finish_rest(events);
            }
            _ => self.finish_rest(events),
        }
        Ok(())
    }

    fn finish_rest(&mut self, events: &mut Vec<MarketEvent>) {
        self.rest_done = true;
        let forming = std::mem::take(&mut self.forming);
        if let Some(t_n) = forming.iter().map(|f| f.time).max() {
            let period = self.ctx.map_or(60_000, |c| c.bar_period_ms);
            let mut seen = HashSet::new();
            let unique: Vec<_> = forming
                .into_iter()
                .filter(|f| seen.insert(f.uid.clone()))
                .collect();
            let mut before: Vec<_> = unique
                .iter()
                .filter(|f| f.time < t_n)
                .map(|f| (f.time, f.price, f.qty))
                .collect();
            events.push(MarketEvent::History {
                bars: bars_from(&mut before, period),
                as_of: Some(UtcMillis(t_n.0 - 1)),
            });
            let mut at: Vec<_> = Vec::new();
            for f in unique {
                if f.time < t_n {
                    self.mark(&f.uid);
                } else if self.mark(&f.uid) {
                    at.push(f);
                }
            }
            for f in at {
                events.push(MarketEvent::Trade {
                    time: f.time,
                    price: f.price,
                    qty: f.qty,
                    aggressor: f.aggressor,
                });
            }
        }
        self.check_hole(events);
    }

    fn check_hole(&mut self, events: &mut Vec<MarketEvent>) {
        if self.complete || !self.rest_done {
            return;
        }
        let Some(ws_oldest) = self.ws_oldest else {
            return;
        };
        if self.rest_newest.is_some_and(|r| r >= ws_oldest) {
            self.complete = true;
            events.push(MarketEvent::HistoryComplete);
        } else if self.fill_attempts >= MAX_FILL_ATTEMPTS {
            let from = self.rest_newest.or(self.boundary).unwrap_or(ws_oldest);
            events.push(MarketEvent::HistoryGap {
                from,
                to: ws_oldest,
            });
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
        let m: Msg = serde_json::from_slice(data).map_err(|e| bad_json("message", e))?;
        if let Some(event) = m.event.as_deref() {
            match event {
                "error" | "alert" => {
                    return Err(SessionError::Protocol(format!(
                        "Kraken Futures {event}: {}",
                        m.message.unwrap_or_default()
                    )));
                }
                "subscribed" if !self.ready => {
                    self.ready = true;
                    events.push(MarketEvent::Ready);
                }
                _ => {}
            }
            return Ok(());
        }
        if m.product_id
            .as_deref()
            .is_some_and(|p| !p.eq_ignore_ascii_case(self.symbol))
        {
            return Ok(());
        }
        match m.feed.as_deref() {
            Some("book_snapshot") => {
                self.book.clear();
                events.push(MarketEvent::BookClear);
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
                self.book_seq = m.seq;
                self.book_live = true;
            }
            Some("book") => {
                if !self.book_live {
                    return Ok(());
                }
                let (Some(seq), Some(prev)) = (m.seq, self.book_seq) else {
                    return Ok(());
                };
                if seq <= prev {
                    return Ok(()); // a duplicate or a late message from before a resync
                }
                if seq != prev + 1 {
                    tracing::warn!(
                        "Kraken Futures {} book sequence jumped {prev} -> {seq}; resyncing",
                        self.symbol
                    );
                    // Counted as a resync, not a `Gap`: a lost *book* message loses no trades
                    // (`Gap` means dropped trades to the store and the local data store).
                    self.resync(events, out);
                    return Ok(());
                }
                self.book_seq = Some(seq);
                let side = match m.side.as_deref() {
                    Some("buy") => Side::Bid,
                    Some("sell") => Side::Ask,
                    _ => return Ok(()),
                };
                match (
                    m.price.and_then(|p| self.price(p.get())),
                    m.qty.and_then(|q| self.qty(q.get())),
                ) {
                    (Some(p), Some(q)) => self.book.set(side, p.0, q.0, events),
                    _ => {
                        // A level we can't represent leaves the book unknowable: resync.
                        events.push(MarketEvent::Rejected { count: 1 });
                        self.resync(events, out);
                    }
                }
            }
            Some("trade_snapshot") => {
                let mut trades: Vec<_> = m.trades.iter().collect();
                trades.sort_by_key(|t| t.time);
                if let Some(t) = trades.first() {
                    let oldest = UtcMillis(t.time);
                    self.ws_oldest = Some(self.ws_oldest.map_or(oldest, |o| o.min(oldest)));
                }
                for t in trades {
                    self.on_trade(&t.uid, &t.side, t.time, t.price.get(), t.qty.get(), events);
                }
                self.check_hole(events);
            }
            Some("trade") => {
                if let (Some(uid), Some(side), Some(time), Some(price), Some(qty)) =
                    (&m.uid, &m.side, m.time, m.price, m.qty)
                {
                    if self.ws_oldest.is_none() {
                        self.ws_oldest = Some(UtcMillis(time));
                    }
                    self.on_trade(uid, side, time, price.get(), qty.get(), events);
                    self.check_hole(events);
                }
            }
            Some("ticker") => {
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
            _ => {}
        }
        Ok(())
    }

    fn on_trade(
        &mut self,
        uid: &str,
        side: &str,
        time: i64,
        price: &str,
        qty: &str,
        events: &mut Vec<MarketEvent>,
    ) {
        let (Some(price), Some(qty)) = (self.price(price), self.qty(qty)) else {
            events.push(MarketEvent::Rejected { count: 1 });
            return;
        };
        if self.mark(uid) {
            events.push(MarketEvent::Trade {
                time: UtcMillis(time),
                price,
                qty,
                aggressor: taker(side),
            });
        }
    }

    fn resync(&mut self, events: &mut Vec<MarketEvent>, out: &mut Vec<Outgoing>) {
        self.book.clear();
        self.book_live = false;
        self.book_seq = None;
        events.push(MarketEvent::BookClear);
        events.push(MarketEvent::BookResync);
        out.push(self.feed("unsubscribe", "book"));
        out.push(self.feed("subscribe", "book"));
    }
}

impl ProviderSession for FuturesSession {
    fn start(&mut self, ctx: StartContext, out: &mut Vec<Outgoing>) {
        self.ctx = Some(ctx);
        out.push(Outgoing::HttpGet(format!(
            "{}/derivatives/api/v3/instruments",
            self.rest_url
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
            FrameKind::Text if self.subscribed => self.on_text(&frame.data, events, out),
            _ => Ok(()),
        }
    }

    fn keepalive(&mut self, _now: UtcMillis) -> Option<Outgoing> {
        if self.complete || !self.rest_done || self.ws_oldest.is_none() {
            return None;
        }
        self.fill_attempts += 1;
        Some(Outgoing::HttpGet(self.history_url(None)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    const REST: &str = "https://futures.test";
    const NOW: i64 = 1_793_988_130_000;

    fn session() -> FuturesSession {
        FuturesSession::new(
            "PF_XBTUSD",
            catalog::get(catalog::BTC_PERP_KR).unwrap(),
            0,
            REST.into(),
        )
    }

    fn http(url: &str, body: &str) -> Frame {
        Frame::http(format!("{url}\n{body}").into_bytes())
    }

    fn feed(
        s: &mut FuturesSession,
        f: Frame,
    ) -> Result<(Vec<MarketEvent>, Vec<Outgoing>), SessionError> {
        let (mut ev, mut out) = (Vec::new(), Vec::new());
        s.on_frame(&f, UtcMillis(0), &mut ev, &mut out)?;
        Ok((ev, out))
    }

    fn text(s: &mut FuturesSession, json: &str) -> (Vec<MarketEvent>, Vec<Outgoing>) {
        feed(s, Frame::text(json)).unwrap()
    }

    const INSTRUMENTS: &str = r#"{"result":"success","instruments":[{"symbol":"PF_ETHUSD","tickSize":0.1,"contractValueTradePrecision":3},{"symbol":"PF_XBTUSD","type":"flexible_futures","tickSize":1,"contractValueTradePrecision":4}]}"#;

    fn ready(history_from: Option<UtcMillis>) -> (FuturesSession, Vec<Outgoing>) {
        let mut s = session();
        let mut out = Vec::new();
        s.start(
            StartContext {
                now: UtcMillis(NOW),
                history_from,
                bar_period_ms: 60_000,
                history_ms: 3 * 86_400_000,
            },
            &mut out,
        );
        let url = format!("{REST}/derivatives/api/v3/instruments");
        assert_eq!(out, [Outgoing::HttpGet(url.clone())]);
        let (_, out) = feed(&mut s, http(&url, INSTRUMENTS)).unwrap();
        (s, out)
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
        let subs: Vec<_> = out
            .iter()
            .filter_map(|o| match o {
                Outgoing::Frame(f) => Some(String::from_utf8(f.data.to_vec()).unwrap()),
                _ => None,
            })
            .collect();
        assert_eq!(subs.len(), 3);
        for (s, feed) in subs.iter().zip(["book", "trade", "ticker"]) {
            assert_eq!(
                *s,
                format!(r#"{{"event":"subscribe","feed":"{feed}","product_ids":["PF_XBTUSD"]}}"#)
            );
        }
        let mut s = session();
        s.start(
            StartContext {
                now: UtcMillis(NOW),
                history_from: None,
                bar_period_ms: 60_000,
                history_ms: 0,
            },
            &mut Vec::new(),
        );
        let changed = INSTRUMENTS.replace(r#""tickSize":1,"#, r#""tickSize":0.5,"#);
        let err = feed(
            &mut s,
            http(&format!("{REST}/derivatives/api/v3/instruments"), &changed),
        )
        .unwrap_err();
        assert!(matches!(err, SessionError::BadMetadata(_)), "{err:?}");
    }

    #[test]
    fn book_sequence_gap_clears_and_resubscribes() {
        let (mut s, _) = ready(None);
        let (ev, _) = text(
            &mut s,
            r#"{"feed":"book_snapshot","product_id":"PF_XBTUSD","seq":100,"bids":[{"price":75647.0,"qty":0.0001}],"asks":[{"price":75648.0,"qty":0.1436}]}"#,
        );
        assert_eq!(
            ev,
            [
                MarketEvent::BookClear,
                lvl(Side::Bid, 75_647, 1),
                lvl(Side::Ask, 75_648, 1_436)
            ]
        );
        let (ev, _) = text(
            &mut s,
            r#"{"feed":"book","product_id":"PF_XBTUSD","side":"buy","seq":101,"price":75611.0,"qty":0.5156}"#,
        );
        assert_eq!(ev, [lvl(Side::Bid, 75_611, 5_156)]);
        // Message 102 never arrived.
        let (ev, out) = text(
            &mut s,
            r#"{"feed":"book","product_id":"PF_XBTUSD","side":"sell","seq":103,"price":75650.0,"qty":0}"#,
        );
        assert_eq!(ev, [MarketEvent::BookClear, MarketEvent::BookResync]);
        assert_eq!(out.len(), 2, "unsubscribe + subscribe book");
        assert!(s.book.is_empty());
        let (ev, _) = text(
            &mut s,
            r#"{"feed":"book","product_id":"PF_XBTUSD","side":"buy","seq":104,"price":1.0,"qty":1}"#,
        );
        assert!(ev.is_empty(), "ignored until the fresh snapshot");
    }

    #[test]
    fn trades_are_deduplicated_by_uid() {
        let (mut s, _) = ready(None);
        let (ev, _) = text(
            &mut s,
            r#"{"feed":"trade_snapshot","product_id":"PF_XBTUSD","trades":[{"uid":"b","side":"buy","time":2000,"qty":0.0045,"price":75647.0,"seq":2},{"uid":"a","side":"sell","time":1000,"qty":0.0002,"price":75646.0,"seq":1}]}"#,
        );
        assert_eq!(
            ev,
            [
                MarketEvent::Trade {
                    time: UtcMillis(1000),
                    price: Price(75_646),
                    qty: Qty(2),
                    aggressor: Some(Aggressor::Sell)
                },
                MarketEvent::Trade {
                    time: UtcMillis(2000),
                    price: Price(75_647),
                    qty: Qty(45),
                    aggressor: Some(Aggressor::Buy)
                }
            ],
            "oldest first"
        );
        let again = r#"{"feed":"trade","product_id":"PF_XBTUSD","uid":"b","side":"buy","time":2000,"qty":0.0045,"price":75647.0}"#;
        assert_eq!(text(&mut s, again).0, []);
    }

    #[test]
    fn candles_then_rest_trades_rebuild_the_forming_bars() {
        let (mut s, out) = ready(Some(UtcMillis(NOW - 5 * 60_000)));
        let Some(Outgoing::HttpGet(candles)) = out.last().cloned() else {
            panic!()
        };
        let cutoff = NOW - NOW % 60_000;
        let boundary = cutoff - 60_000;
        assert!(
            candles.contains(&format!("to={}", boundary / 1000 - 1)),
            "{candles}"
        );
        let c = |t: i64, p: &str| {
            format!(
                r#"{{"time":{t},"open":"{p}","high":"{p}","low":"{p}","close":"{p}","volume":"0.3359999999999999"}}"#
            )
        };
        let body = format!(
            r#"{{"candles":[{},{}],"more_candles":false}}"#,
            c(boundary - 120_000, "75600"),
            c(boundary - 60_000, "75601")
        );
        let (ev, out) = feed(&mut s, http(&candles, &body)).unwrap();
        let MarketEvent::History { bars, as_of } = &ev[0] else {
            panic!("{ev:?}")
        };
        assert_eq!(bars.len(), 2);
        assert_eq!(
            bars[0].volume,
            Qty(3_360),
            "float-summed volume snapped within noise"
        );
        assert_eq!(*as_of, Some(UtcMillis(boundary - 1)));
        let Outgoing::HttpGet(hist) = out[0].clone() else {
            panic!()
        };
        assert_eq!(
            hist,
            format!("{REST}/derivatives/api/v3/history?symbol=PF_XBTUSD")
        );

        // The websocket's trade snapshot starts before REST's newest trade: contiguous.
        let iso = |ms: i64| lines_core::UtcMillis(ms).to_string();
        text(
            &mut s,
            &format!(
                r#"{{"feed":"trade_snapshot","product_id":"PF_XBTUSD","trades":[{{"uid":"w1","side":"buy","time":{},"qty":0.0001,"price":75700.0}}]}}"#,
                boundary + 70_000
            ),
        );
        let row = |uid: &str, ms: i64, p: i64| {
            format!(
                r#"{{"time":"{}","trade_id":1,"price":{p},"size":0.0001,"side":"buy","type":"fill","uid":"{uid}"}}"#,
                iso(ms)
            )
        };
        // Newest page first: it doesn't reach the boundary, so the next page is requested.
        let page1 = format!(
            r#"{{"result":"success","history":[{},{}]}}"#,
            row("w1", boundary + 70_000, 75_700),
            row("r3", boundary + 65_000, 75_690)
        );
        let (_, out) = feed(&mut s, http(&hist, &page1)).unwrap();
        let Some(Outgoing::HttpGet(older)) = out.first().cloned() else {
            panic!()
        };
        assert!(older.contains("&lastTime="), "{older}");
        let page2 = format!(
            r#"{{"result":"success","history":[{},{}]}}"#,
            row("r2", boundary + 5_000, 75_650),
            row("r1", boundary - 1_000, 75_640)
        );
        let (ev, _) = feed(&mut s, http(&older, &page2)).unwrap();
        let MarketEvent::History { bars, as_of } = &ev[0] else {
            panic!("{ev:?}")
        };
        assert_eq!(*as_of, Some(UtcMillis(boundary + 70_000 - 1)));
        assert_eq!(
            bars.len(),
            2,
            "r2 in the boundary bar, r3 in the next; r1 is before it"
        );
        assert_eq!(bars[0].close, Price(75_650));
        assert_eq!(bars[1].close, Price(75_690));
        assert!(
            !ev.iter().any(|e| matches!(e, MarketEvent::Trade { .. })),
            "w1 (at the newest time) came from the websocket already"
        );
        assert_eq!(ev.last(), Some(&MarketEvent::HistoryComplete));
    }
}
