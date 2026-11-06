//! Coinbase Exchange protocol logic for one connection, free of I/O.
//!
//! Coinbase sends prices and sizes as decimal *strings* on the websocket and as JSON
//! *numbers* in REST candles. Both are parsed from their text exactly into the catalog
//! product's units (decision 0002); nothing goes through `f64`.

use lines_core::{Bar, Price, Product, Qty, UtcMillis};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::provider::{
    Frame, FrameKind, MarketEvent, Outgoing, ProviderSession, SessionError, StartContext,
};

/// Coinbase returns at most 300 candles per request.
const MAX_CANDLES: i64 = 300;

pub struct CoinbaseSession {
    product: String,
    /// Catalog spec of the spot product (units for exact parsing).
    spec: Product,
    rest_url: String,
    last_trade_id: Option<u64>,
    /// Candles at or after this time are the forming bar; live matches build it instead.
    history_cutoff: UtcMillis,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WsMsg {
    Subscriptions {},
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
            last_trade_id: None,
            history_cutoff: UtcMillis(i64::MAX),
        }
    }

    fn price(&self, s: &str) -> Option<Price> {
        self.spec.price.scale.parse_decimal(s).ok().map(Price)
    }

    fn qty(&self, s: &str) -> Option<Qty> {
        self.spec.qty.parse_decimal(s).ok()
    }

    /// Track trade ids: returns false for duplicates; reports gaps.
    fn sequence(&mut self, trade_id: u64, events: &mut Vec<MarketEvent>) -> bool {
        match self.last_trade_id {
            Some(last) if trade_id <= last => return false,
            Some(last) if trade_id > last + 1 => events.push(MarketEvent::Gap {
                missed: trade_id - last - 1,
            }),
            _ => {}
        }
        self.last_trade_id = Some(trade_id);
        true
    }

    fn on_text(&mut self, text: &[u8], events: &mut Vec<MarketEvent>) -> Result<(), SessionError> {
        let msg: WsMsg = serde_json::from_slice(text)
            .map_err(|e| SessionError::Protocol(format!("bad Coinbase message: {e}")))?;
        match msg {
            WsMsg::Subscriptions {} => events.push(MarketEvent::Ready),
            WsMsg::Match(m) => {
                if !self.sequence(m.trade_id, events) {
                    return Ok(());
                }
                match (
                    UtcMillis::parse_iso(&m.time),
                    self.price(&m.price),
                    self.qty(&m.size),
                ) {
                    (Some(time), Some(price), Some(qty)) => {
                        events.push(MarketEvent::Trade { time, price, qty })
                    }
                    _ => events.push(MarketEvent::Rejected { count: 1 }),
                }
            }
            WsMsg::LastMatch(m) => {
                self.last_trade_id = Some(m.trade_id);
                match self.price(&m.price) {
                    Some(p) => events.push(MarketEvent::LastPrice(p)),
                    None => events.push(MarketEvent::Rejected { count: 1 }),
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
                    self.last_trade_id = Some(h.last_trade_id);
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

    /// REST candles: `[[time_s, low, high, open, close, volume], ...]`, newest first.
    fn on_candles(
        &mut self,
        body: &[u8],
        events: &mut Vec<MarketEvent>,
    ) -> Result<(), SessionError> {
        let rows: Vec<Vec<&RawValue>> = serde_json::from_slice(body).map_err(|e| {
            let preview = String::from_utf8_lossy(&body[..body.len().min(200)]).into_owned();
            SessionError::Protocol(format!("bad Coinbase candles ({e}): {preview}"))
        })?;
        let mut bars = Vec::with_capacity(rows.len());
        let mut rejected = 0;
        for row in &rows {
            let bar = (|| {
                let [t, low, high, open, close, vol] = row.as_slice() else {
                    return None;
                };
                let time = UtcMillis(t.get().parse::<i64>().ok()? * 1000);
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
                Some(b) if b.time < self.history_cutoff => bars.push(b),
                Some(_) => {} // forming bar: built from live matches instead
                None => rejected += 1,
            }
        }
        bars.sort_by_key(|b| b.time);
        events.extend(bars.into_iter().map(MarketEvent::Bar));
        if rejected > 0 {
            events.push(MarketEvent::Rejected { count: rejected });
        }
        Ok(())
    }
}

impl ProviderSession for CoinbaseSession {
    fn start(&mut self, ctx: StartContext, out: &mut Vec<Outgoing>) {
        let product = &self.product;
        out.push(Outgoing::Frame(Frame::text(format!(
            r#"{{"type":"subscribe","product_ids":["{product}"],"channels":["matches","ticker","heartbeat"]}}"#
        ))));
        let period = ctx.bar_period_ms;
        self.history_cutoff = ctx.now.floor_to(period);
        let earliest = UtcMillis(self.history_cutoff.0 - (MAX_CANDLES - 1) * period);
        let from = ctx.history_from.map_or(earliest, |f| f.max(earliest));
        out.push(Outgoing::HttpGet(format!(
            "{}/products/{product}/candles?granularity={}&start={from}&end={}",
            self.rest_url,
            period / 1000,
            self.history_cutoff
        )));
    }

    fn on_frame(
        &mut self,
        frame: &Frame,
        _now: UtcMillis,
        events: &mut Vec<MarketEvent>,
        _out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        match frame.kind {
            FrameKind::Text => self.on_text(&frame.data, events),
            FrameKind::Http => self.on_candles(&frame.data, events),
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

    fn session() -> CoinbaseSession {
        let mut s = CoinbaseSession::new(
            "BTC-USD".into(),
            catalog::get(catalog::BTC_USD).unwrap(),
            "https://x".into(),
        );
        let mut out = Vec::new();
        s.start(
            StartContext {
                now: UtcMillis::parse_iso("2026-11-06T14:05:30Z").unwrap(),
                history_from: None,
                bar_period_ms: 60_000,
            },
            &mut out,
        );
        s
    }

    fn text(s: &mut CoinbaseSession, json: &str) -> Vec<MarketEvent> {
        let mut ev = Vec::new();
        s.on_frame(&Frame::text(json), UtcMillis(0), &mut ev, &mut Vec::new())
            .unwrap();
        ev
    }

    #[test]
    fn start_subscribes_and_requests_history() {
        let mut s = CoinbaseSession::new(
            "BTC-USD".into(),
            catalog::get(catalog::BTC_USD).unwrap(),
            "https://x".into(),
        );
        let mut out = Vec::new();
        let now = UtcMillis::parse_iso("2026-11-06T14:05:30Z").unwrap();
        s.start(
            StartContext {
                now,
                history_from: None,
                bar_period_ms: 60_000,
            },
            &mut out,
        );
        let Outgoing::Frame(f) = &out[0] else {
            panic!()
        };
        let sub: serde_json::Value = serde_json::from_slice(&f.data).unwrap();
        assert_eq!(sub["product_ids"][0], "BTC-USD");
        assert_eq!(
            out[1],
            Outgoing::HttpGet(
                "https://x/products/BTC-USD/candles?granularity=60&start=2026-11-06T09:06:00.000Z&end=2026-11-06T14:05:00.000Z"
                    .into()
            )
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
                time: UtcMillis::parse_iso("2026-11-06T14:05:31.123Z").unwrap(),
                price: Price(7_541_572),
                qty: Qty(123_456),
            }]
        );
        // Duplicate is dropped; a jump is reported as a gap.
        assert!(text(&mut s, r#"{"type":"match","trade_id":100,"size":"1","price":"1","time":"2026-11-06T14:05:31Z"}"#).is_empty());
        let ev = text(
            &mut s,
            r#"{"type":"match","trade_id":104,"size":"1","price":"1.5","time":"2026-11-06T14:05:32Z"}"#,
        );
        assert_eq!(ev[0], MarketEvent::Gap { missed: 3 });
        // Off-grid price (sub-cent) is rejected, not rounded.
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
            r#"{"type":"last_match","trade_id":10,"size":"1","price":"2","time":"2026-11-06T14:05:00Z"}"#,
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
            .on_frame(&Frame::text(r#"{"type":"error","message":"Failed to subscribe","reason":"BTC-XYZ is not a valid product"}"#), UtcMillis(0), &mut Vec::new(), &mut Vec::new())
            .unwrap_err();
        assert!(!err.is_retryable());
    }

    #[test]
    fn candles_parse_exactly_and_skip_forming_bar() {
        let mut s = session(); // cutoff 14:05:00
        let t = |hm: &str| UtcMillis::parse_iso(&format!("2026-11-06T{hm}:00Z")).unwrap();
        let body = format!(
            "[[{},75400,75500.5,75410.01,75450,1.5],[{},75415.72,75556.5,75523.22,75433.92,30.69151416],[{},1,2,1,2,1]]",
            t("14:05").0 / 1000,
            t("14:04").0 / 1000,
            t("14:03").0 / 1000,
        );
        let mut ev = Vec::new();
        s.on_frame(&Frame::http(body), UtcMillis(0), &mut ev, &mut Vec::new())
            .unwrap();
        assert_eq!(ev.len(), 2, "forming 14:05 bar excluded: {ev:?}");
        let MarketEvent::Bar(b) = ev[1] else { panic!() };
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
        let MarketEvent::Bar(first) = ev[0] else {
            panic!()
        };
        assert!(first.time < b.time, "sorted oldest first");
    }
}
