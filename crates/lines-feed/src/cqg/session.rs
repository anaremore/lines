//! CQG WebAPI protocol logic for one connection, free of I/O.
//!
//! Request ids restart at 1 per connection and contract ids are re-resolved, because CQG
//! contract ids are only valid for the session that issued them. This is the CQG *edge*
//! (decision 0002): `scaled_price × correct_price_scale` is converted exactly into the
//! catalog product's units, and metadata that disagrees with the catalog is rejected.

use cqg_proto::{cqg, historical_2, market_data_2, metadata_2, user_session_2, web_api_2};
use lines_core::calendar::resolve_expiry;
use lines_core::{
    Bar, CivilDate, ContractKey, Price, PriceScale, Product, Qty, UnitConverter, UtcMillis,
};
use prost::Message;

use crate::provider::{
    Frame, FrameKind, MarketEvent, Outgoing, ProviderSession, SessionError, StartContext,
};

use historical_2::{BarUnit, TimeBar, TimeBarParameters, TimeBarRequest, time_bar_request};
use market_data_2::{
    MarketDataSubscription, Quote, RealTimeMarketData, market_data_subscription, quote,
};
use web_api_2::{ClientMsg, InformationRequest, ServerMsg};

#[derive(Clone, Debug, Default)]
pub struct LogonParams {
    pub user_name: Option<String>,
    pub password: Option<String>,
    pub private_label: Option<String>,
    pub client_app_id: Option<String>,
    pub client_version: String,
}

/// Default history window when we hold no bars.
const DEFAULT_HISTORY_MS: i64 = 3 * 24 * 60 * 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    AwaitLogon,
    AwaitSymbol,
    Live,
}

pub struct CqgSession {
    /// Product root (e.g. `EP`); requested as-is when the contract is unknown.
    root: &'static str,
    /// The contract this session is for and its CQG symbol (e.g. `EPZ6`). `None` = unknown
    /// (replay of a pre-v3 recording): request the root and accept what CQG resolves.
    expect: Option<(ContractKey, String)>,
    product: Product,
    logon: LogonParams,
    state: State,
    base_time: UtcMillis,
    next_id: u32,
    symbol_request_id: u32,
    bar_request_id: u32,
    contract_id: Option<u32>,
    /// CQG scaled units → catalog units (exact).
    price_conv: Option<UnitConverter>,
    /// CQG `correct_price_scale` the converter was built from.
    cqg_scale: f64,
    /// quote_utc_time is only sent when it changes; carry it forward (offset ms).
    last_quote_offset: i64,
    ctx: Option<StartContext>,
}

fn encode(msg: &ClientMsg) -> Outgoing {
    Outgoing::Frame(Frame::binary(msg.encode_to_vec()))
}

impl CqgSession {
    pub fn new(
        root: &'static str,
        expect: Option<(ContractKey, String)>,
        product: Product,
        logon: LogonParams,
    ) -> Self {
        CqgSession {
            root,
            expect,
            product,
            logon,
            state: State::AwaitLogon,
            base_time: UtcMillis(0),
            next_id: 1,
            symbol_request_id: 0,
            bar_request_id: 0,
            contract_id: None,
            price_conv: None,
            cqg_scale: 0.0,
            last_quote_offset: 0,
            ctx: None,
        }
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// All CQG sint64 times are ms offsets from `LogonResult.base_time`.
    fn offset_of(&self, t: UtcMillis) -> i64 {
        t.0 - self.base_time.0
    }
    fn time_at_offset(&self, offset: i64) -> UtcMillis {
        UtcMillis(self.base_time.0 + offset)
    }

    #[inline]
    fn price(&self, scaled: i64) -> Option<Price> {
        self.price_conv?.convert(scaled).map(Price)
    }

    /// CQG volume: prefer `cqg.Decimal volume`; fall back to deprecated `scaled_volume`
    /// (volume_scale assumed 1, true for futures).
    fn qty(&self, decimal: Option<&cqg::Decimal>, scaled: Option<u64>) -> Option<Qty> {
        let q = self.product.qty;
        match (decimal, scaled) {
            (Some(d), _) => q
                .from_parts(d.significand.unwrap_or(0) as i128, d.exponent.unwrap_or(0))
                .ok(),
            (None, Some(v)) => q.from_parts(v as i128, 0).ok(),
            (None, None) => Some(Qty::ZERO),
        }
    }

    fn on_logon_result(
        &mut self,
        r: &user_session_2::LogonResult,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        if r.result_code != user_session_2::logon_result::ResultCode::Success as u32 {
            return Err(SessionError::LogonRejected {
                code: r.result_code,
                text: r.text_message.clone().unwrap_or_default(),
            });
        }
        if r.protocol_version_major != cqg_proto::PROTOCOL_VERSION_MAJOR {
            return Err(SessionError::Protocol(format!(
                "server speaks protocol {}.{}, we were built for {}.{}",
                r.protocol_version_major,
                r.protocol_version_minor,
                cqg_proto::PROTOCOL_VERSION_MAJOR,
                cqg_proto::PROTOCOL_VERSION_MINOR
            )));
        }
        self.base_time = UtcMillis::parse_iso(&r.base_time)
            .ok_or_else(|| SessionError::Protocol(format!("bad base_time {:?}", r.base_time)))?;
        // A LogonResult mid-stream (e.g. a replay spanning a reconnect) starts a new session.
        self.state = State::AwaitSymbol;
        self.contract_id = None;
        self.next_id = 1;
        self.symbol_request_id = self.id();
        out.push(encode(&ClientMsg {
            information_requests: vec![InformationRequest {
                id: self.symbol_request_id,
                subscribe: Some(false),
                symbol_resolution_request: Some(metadata_2::SymbolResolutionRequest {
                    symbol: Some(
                        self.expect
                            .as_ref()
                            .map_or_else(|| self.root.to_owned(), |(_, s)| s.clone()),
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }));
        Ok(())
    }

    /// Validate CQG metadata against the catalog and build the exact unit converter.
    fn validate(&self, meta: &metadata_2::ContractMetadata) -> Result<UnitConverter, SessionError> {
        let bad = |m: String| {
            SessionError::BadMetadata(format!(
                "{} ({}): {m}",
                self.product.symbol, meta.contract_symbol
            ))
        };
        let cqg_scale = PriceScale::from_f64(meta.correct_price_scale)
            .map_err(|e| bad(format!("correct_price_scale {e}")))?;
        if let Some((_, want)) = &self.expect
            && meta.contract_symbol != *want
        {
            return Err(bad(format!(
                "asked for contract {want}, CQG resolved {}",
                meta.contract_symbol
            )));
        }
        let conv = UnitConverter::new(cqg_scale, self.product.price.scale);
        let tick = self
            .product
            .price
            .scale
            .exact_units_from_f64(meta.tick_size)
            .map_err(|e| bad(format!("tick_size {e}")))?;
        if tick != self.product.price.tick {
            return Err(bad(format!(
                "tick_size {} = {tick} units, catalog says {}",
                meta.tick_size, self.product.price.tick
            )));
        }
        if !meta.tick_sizes_by_price.is_empty() {
            tracing::warn!(
                "{}: variable tick sizes are not supported; using the catalog tick",
                meta.contract_symbol
            );
        }
        Ok(conv)
    }

    /// Decision 0013: provider expiry wins over the calendar rule; disagreement is logged.
    fn check_expiry(&self, meta: &metadata_2::ContractMetadata) {
        let (Some((key, sym)), Some(ltd)) = (&self.expect, meta.last_trading_date) else {
            return;
        };
        let provider = CivilDate::from_utc(self.time_at_offset(ltd));
        if let Some(e) = resolve_expiry(&self.product, *key, Some(provider))
            && let Some(cal) = e.calendar_disagrees
        {
            tracing::warn!(
                "{sym}: CQG last trading date {provider} differs from the calendar rule ({cal}); using CQG's"
            );
        }
    }

    fn on_symbol_report(
        &mut self,
        rep: &web_api_2::InformationReport,
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        if rep.status_code > 100 {
            return Err(SessionError::Unavailable(format!(
                "CQG symbol resolution failed: {} (code {})",
                rep.text_message.clone().unwrap_or_default(),
                rep.status_code
            )));
        }
        let Some(meta) = rep
            .symbol_resolution_report
            .as_ref()
            .map(|r| &r.contract_metadata)
        else {
            return Err(SessionError::Protocol(
                "symbol report without contract metadata".into(),
            ));
        };
        self.price_conv = Some(self.validate(meta)?);
        self.cqg_scale = meta.correct_price_scale;
        self.contract_id = Some(meta.contract_id);
        self.check_expiry(meta);

        let ctx = self.ctx.expect("start() runs before frames");
        let from = ctx.history_from.unwrap_or_else(|| {
            UtcMillis(ctx.now.0 - DEFAULT_HISTORY_MS).floor_to(ctx.bar_period_ms)
        });
        self.bar_request_id = self.id();
        let md_request_id = self.id();
        out.push(encode(&ClientMsg {
            time_bar_requests: vec![TimeBarRequest {
                request_id: self.bar_request_id,
                request_type: Some(time_bar_request::RequestType::Subscribe as u32),
                time_bar_parameters: Some(TimeBarParameters {
                    contract_id: meta.contract_id,
                    bar_unit: BarUnit::Min as u32,
                    unit_number: Some((ctx.bar_period_ms / 60_000).max(1) as u32),
                    from_utc_time: self.offset_of(from),
                    ..Default::default()
                }),
            }],
            market_data_subscriptions: vec![MarketDataSubscription {
                contract_id: meta.contract_id,
                request_id: Some(md_request_id),
                level: market_data_subscription::Level::TradesBba as u32,
                ..Default::default()
            }],
            ..Default::default()
        }));
        self.state = State::Live;
        events.push(MarketEvent::Ready);
        Ok(())
    }

    fn on_time_bar_report(&self, rep: &historical_2::TimeBarReport, events: &mut Vec<MarketEvent>) {
        if rep.status_code > 100 {
            let text = rep
                .details
                .as_ref()
                .and_then(|t| t.text.clone())
                .unwrap_or_default();
            tracing::warn!(code = rep.status_code, "time bar request failed: {text}");
            return;
        }
        let mut rejected = 0;
        for tb in &rep.time_bars {
            match self.convert_bar(tb) {
                Some(Some(bar)) => events.push(MarketEvent::Bar(bar)),
                Some(None) => {} // no trades in this bar
                None => rejected += 1,
            }
        }
        if rejected > 0 {
            events.push(MarketEvent::Rejected { count: rejected });
        }
    }

    /// `None` = unrepresentable (rejected); `Some(None)` = bar without trades.
    #[allow(deprecated)] // scaled_volume: fallback for servers that omit `volume`
    fn convert_bar(&self, tb: &TimeBar) -> Option<Option<Bar>> {
        let (Some(o), Some(h), Some(l), Some(c)) = (
            tb.scaled_open_price,
            tb.scaled_high_price,
            tb.scaled_low_price,
            tb.scaled_close_price,
        ) else {
            return Some(None);
        };
        let period = self.ctx.map_or(60_000, |c| c.bar_period_ms);
        Some(Some(Bar {
            time: self.time_at_offset(tb.bar_utc_time).floor_to(period),
            open: self.price(o)?,
            high: self.price(h)?,
            low: self.price(l)?,
            close: self.price(c)?,
            volume: self.qty(tb.volume.as_ref(), tb.scaled_volume)?,
        }))
    }

    fn on_market_data(&mut self, md: &RealTimeMarketData, events: &mut Vec<MarketEvent>) {
        if let Some(scale) = md.correct_price_scale
            && scale != self.cqg_scale
        {
            // CQG may rescale a contract mid-session; re-resolving would be needed.
            tracing::error!(
                scale,
                "contract price scale changed mid-session; dropping update"
            );
            events.push(MarketEvent::Rejected {
                count: md.quotes.len() as u64,
            });
            return;
        }
        let snapshot = md.is_snapshot.unwrap_or(false);
        let mut rejected = 0u64;
        for q in &md.quotes {
            if let Some(qt) = q.quote_utc_time {
                self.last_quote_offset = qt;
            }
            let time = self.time_at_offset(self.last_quote_offset);
            if self.on_quote(q, time, snapshot, events).is_none() {
                rejected += 1;
            }
        }
        if rejected > 0 {
            events.push(MarketEvent::Rejected { count: rejected });
        }
        if !md.corrections.is_empty() {
            // Applying corrections exactly needs trade-level history; the subscribed server
            // bar updates repair affected bars instead.
            events.push(MarketEvent::CorrectionsIgnored {
                count: md.corrections.len() as u64,
            });
        }
    }

    #[allow(deprecated)] // scaled_volume: fallback for servers that omit `volume`
    fn on_quote(
        &self,
        q: &Quote,
        time: UtcMillis,
        snapshot: bool,
        events: &mut Vec<MarketEvent>,
    ) -> Option<()> {
        let past = q.indicators.contains(&(quote::Indicator::Past as u32));
        match q.r#type {
            x if x == quote::Type::Trade as u32 => {
                let price = self.price(q.scaled_price)?;
                if snapshot || past {
                    events.push(MarketEvent::LastPrice(price));
                } else {
                    let qty = self.qty(q.volume.as_ref(), q.scaled_volume)?;
                    events.push(MarketEvent::Trade { time, price, qty });
                }
            }
            x if x == quote::Type::Bestbid as u32 => {
                events.push(MarketEvent::Bid(self.price(q.scaled_price)?))
            }
            x if x == quote::Type::Bestask as u32 => {
                events.push(MarketEvent::Ask(self.price(q.scaled_price)?))
            }
            _ => {}
        }
        Some(())
    }
}

impl ProviderSession for CqgSession {
    fn start(&mut self, ctx: StartContext, out: &mut Vec<Outgoing>) {
        self.ctx = Some(ctx);
        let l = &self.logon;
        out.push(encode(&ClientMsg {
            logon: Some(user_session_2::Logon {
                user_name: l.user_name.clone(),
                password: l.password.clone(),
                private_label: l.private_label.clone(),
                client_app_id: l.client_app_id.clone(),
                client_version: l.client_version.clone(),
                protocol_version_major: Some(cqg_proto::PROTOCOL_VERSION_MAJOR),
                protocol_version_minor: Some(cqg_proto::PROTOCOL_VERSION_MINOR),
                ..Default::default()
            }),
            ..Default::default()
        }));
    }

    fn on_frame(
        &mut self,
        frame: &Frame,
        now: UtcMillis,
        events: &mut Vec<MarketEvent>,
        out: &mut Vec<Outgoing>,
    ) -> Result<(), SessionError> {
        if frame.kind != FrameKind::Binary {
            return Err(SessionError::Protocol(format!(
                "unexpected {:?} frame from CQG",
                frame.kind
            )));
        }
        let msg = ServerMsg::decode(frame.data.clone())
            .map_err(|e| SessionError::Protocol(format!("undecodable ServerMsg: {e}")))?;
        if let Some(r) = &msg.logon_result {
            self.on_logon_result(r, out)?;
        }
        if let Some(lo) = &msg.logged_off {
            return Err(SessionError::LoggedOff(
                lo.text_message.clone().unwrap_or_default(),
            ));
        }
        if let Some(p) = &msg.ping {
            out.push(encode(&ClientMsg {
                pong: Some(user_session_2::Pong {
                    token: p.token.clone(),
                    ping_utc_time: p.ping_utc_time,
                    pong_utc_time: self.offset_of(now),
                }),
                ..Default::default()
            }));
        }
        for rep in &msg.information_reports {
            if rep.id == self.symbol_request_id && self.state == State::AwaitSymbol {
                self.on_symbol_report(rep, events, out)?;
            }
        }
        for st in &msg.market_data_subscription_statuses {
            if st.status_code > 100 {
                let text = st
                    .details
                    .as_ref()
                    .and_then(|t| t.text.clone())
                    .unwrap_or_default();
                tracing::warn!(
                    code = st.status_code,
                    "market data subscription failed: {text}"
                );
            }
        }
        for rep in &msg.time_bar_reports {
            if rep.request_id == self.bar_request_id {
                self.on_time_bar_report(rep, events);
            }
        }
        for rtmd in &msg.real_time_market_data {
            if Some(rtmd.contract_id) == self.contract_id {
                self.on_market_data(rtmd, events);
            }
        }
        Ok(())
    }

    fn keepalive(&mut self, now: UtcMillis) -> Option<Outgoing> {
        (self.state != State::AwaitLogon).then(|| {
            encode(&ClientMsg {
                ping: Some(user_session_2::Ping {
                    token: None,
                    ping_utc_time: self.offset_of(now),
                }),
                ..Default::default()
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    const BASE: &str = "2026-11-06T14:00:00";
    fn base_ms() -> i64 {
        UtcMillis::parse_iso(BASE).unwrap().0
    }

    /// A session for the product's root (contract unknown), as for a pre-v3 replay.
    fn session(id: lines_core::ProductId, _root: &str) -> CqgSession {
        let product = catalog::get(id).unwrap();
        let root = catalog::roots(id).cqg.unwrap();
        let mut s = CqgSession::new(root, None, product, LogonParams::default());
        s.start(
            StartContext {
                now: UtcMillis(base_ms() + 5_000),
                history_from: None,
                bar_period_ms: 60_000,
            },
            &mut Vec::new(),
        );
        s
    }

    fn frame(msg: ServerMsg) -> Frame {
        Frame::binary(msg.encode_to_vec())
    }

    fn decode_out(out: &[Outgoing]) -> Vec<ClientMsg> {
        out.iter()
            .map(|o| match o {
                Outgoing::Frame(f) => ClientMsg::decode(f.data.clone()).unwrap(),
                Outgoing::HttpGet(_) => panic!("CQG does not use HTTP"),
            })
            .collect()
    }

    fn logon_ok() -> ServerMsg {
        ServerMsg {
            logon_result: Some(user_session_2::LogonResult {
                result_code: 0,
                base_time: BASE.into(),
                protocol_version_major: 2,
                protocol_version_minor: 230,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn symbol_ok(id: u32, contract_id: u32, symbol: &str, scale: f64, tick: f64) -> ServerMsg {
        ServerMsg {
            information_reports: vec![web_api_2::InformationReport {
                id,
                status_code: 0,
                symbol_resolution_report: Some(metadata_2::SymbolResolutionReport {
                    contract_metadata: metadata_2::ContractMetadata {
                        contract_id,
                        contract_symbol: symbol.into(),
                        correct_price_scale: scale,
                        display_price_scale: 2,
                        tick_size: tick,
                        ..Default::default()
                    },
                    deleted: None,
                }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn trade(offset: Option<i64>, scaled: i64, vol: i64) -> Quote {
        Quote {
            r#type: quote::Type::Trade as u32,
            quote_utc_time: offset,
            scaled_price: scaled,
            volume: Some(cqg::Decimal {
                significand: Some(vol),
                exponent: Some(0),
            }),
            ..Default::default()
        }
    }

    fn feed(s: &mut CqgSession, msg: ServerMsg) -> (Vec<MarketEvent>, Vec<ClientMsg>) {
        let (mut ev, mut out) = (Vec::new(), Vec::new());
        s.on_frame(&frame(msg), UtcMillis(base_ms() + 5_000), &mut ev, &mut out)
            .unwrap();
        (ev, decode_out(&out))
    }

    #[test]
    fn handshake_and_trades_in_instrument_units() {
        let mut s = session(catalog::ES, "EP");
        let (_, out) = feed(&mut s, logon_ok());
        let req = &out[0].information_requests[0];
        assert_eq!(
            req.symbol_resolution_request
                .as_ref()
                .unwrap()
                .symbol
                .as_deref(),
            Some("EP")
        );

        let (ev, out) = feed(&mut s, symbol_ok(req.id, 77, "EPZ6", 0.01, 0.25));
        assert_eq!(ev, vec![MarketEvent::Ready]);
        assert_eq!(out[0].market_data_subscriptions[0].contract_id, 77);
        let p = out[0].time_bar_requests[0]
            .time_bar_parameters
            .as_ref()
            .unwrap();
        assert_eq!(
            s.time_at_offset(p.from_utc_time),
            UtcMillis(base_ms() + 5_000 - DEFAULT_HISTORY_MS).floor_to(60_000)
        );

        let md = ServerMsg {
            real_time_market_data: vec![RealTimeMarketData {
                contract_id: 77,
                quotes: vec![trade(Some(61_000), 492_525, 2), trade(None, 492_550, 3)],
                ..Default::default()
            }],
            ..Default::default()
        };
        let (ev, _) = feed(&mut s, md);
        let t = UtcMillis(base_ms() + 61_000);
        assert_eq!(
            ev,
            vec![
                MarketEvent::Trade {
                    time: t,
                    price: Price(492_525),
                    qty: Qty(2)
                },
                MarketEvent::Trade {
                    time: t,
                    price: Price(492_550),
                    qty: Qty(3)
                },
            ]
        );
    }

    #[test]
    fn rescales_cqg_units_and_rejects_off_grid() {
        // Pretend CQG quotes ES in 1/1000ths: values convert exactly into the catalog's 1/100.
        let mut s = session(catalog::ES, "EP");
        feed(&mut s, logon_ok());
        feed(&mut s, symbol_ok(1, 5, "EPZ6", 0.001, 0.25));
        let md = ServerMsg {
            real_time_market_data: vec![RealTimeMarketData {
                contract_id: 5,
                quotes: vec![trade(Some(0), 4_925_250, 1), trade(None, 4_925_251, 1)],
                ..Default::default()
            }],
            ..Default::default()
        };
        let (ev, _) = feed(&mut s, md);
        assert_eq!(
            ev[0],
            MarketEvent::Trade {
                time: UtcMillis(base_ms()),
                price: Price(492_525),
                qty: Qty(1)
            }
        );
        assert_eq!(
            ev[1],
            MarketEvent::Rejected { count: 1 },
            "4925.251 is not representable in cents"
        );
    }

    #[test]
    fn metadata_disagreeing_with_catalog_is_fatal() {
        let mut s = session(catalog::ES, "EP");
        feed(&mut s, logon_ok());
        let err = s
            .on_frame(
                &frame(symbol_ok(1, 5, "EPZ6", 0.01, 0.5)),
                UtcMillis(0),
                &mut Vec::new(),
                &mut Vec::new(),
            )
            .unwrap_err();
        assert!(matches!(err, SessionError::BadMetadata(_)), "{err}");
        assert!(!err.is_retryable());
    }

    #[test]
    fn treasury_32nds_convert_exactly() {
        // ZN: CQG might quote in 1/64ths; catalog unit is 1/128.
        let mut s = session(catalog::ZN, "TYA");
        feed(&mut s, logon_ok());
        feed(&mut s, symbol_ok(1, 9, "TYAZ6", 1.0 / 64.0, 1.0 / 64.0));
        let md = ServerMsg {
            real_time_market_data: vec![RealTimeMarketData {
                contract_id: 9,
                quotes: vec![trade(Some(0), 110 * 64 + 33, 1)],
                ..Default::default()
            }],
            ..Default::default()
        };
        let (ev, _) = feed(&mut s, md);
        let MarketEvent::Trade { price, .. } = ev[0] else {
            panic!("{ev:?}")
        };
        assert_eq!(price, Price((110 * 64 + 33) * 2));
        let zn = catalog::get(catalog::ZN).unwrap();
        assert_eq!(zn.price.format::<16>(price).as_str(), "110'165");
    }

    #[test]
    fn snapshot_trade_is_last_price_not_volume() {
        let mut s = session(catalog::ES, "EP");
        feed(&mut s, logon_ok());
        feed(&mut s, symbol_ok(1, 9, "EPZ6", 0.01, 0.25));
        let snap = ServerMsg {
            real_time_market_data: vec![RealTimeMarketData {
                contract_id: 9,
                is_snapshot: Some(true),
                quotes: vec![trade(Some(0), 123_400, 99)],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            feed(&mut s, snap).0,
            vec![MarketEvent::LastPrice(Price(123_400))]
        );
    }

    #[test]
    fn server_ping_gets_pong() {
        let mut s = session(catalog::ES, "EP");
        feed(&mut s, logon_ok());
        let ping = ServerMsg {
            ping: Some(user_session_2::Ping {
                token: Some("t".into()),
                ping_utc_time: 900,
            }),
            ..Default::default()
        };
        let (_, out) = feed(&mut s, ping);
        let pong = out[0].pong.as_ref().unwrap();
        assert_eq!(
            (
                pong.token.as_deref(),
                pong.ping_utc_time,
                pong.pong_utc_time
            ),
            (Some("t"), 900, 5_000)
        );
    }

    #[test]
    fn logon_rejection_is_not_retryable() {
        let mut s = session(catalog::ES, "EP");
        let mut msg = logon_ok();
        msg.logon_result.as_mut().unwrap().result_code = 101;
        msg.logon_result.as_mut().unwrap().text_message = Some("bad password".into());
        let err = s
            .on_frame(&frame(msg), UtcMillis(0), &mut Vec::new(), &mut Vec::new())
            .unwrap_err();
        assert!(!err.is_retryable());
        assert!(err.to_string().contains("bad password"));
    }

    fn contract_session(key: ContractKey, history_from: Option<UtcMillis>) -> CqgSession {
        let product = catalog::get(key.product).unwrap();
        let root = catalog::roots(key.product).cqg.unwrap();
        let sym = super::super::contract_symbol(root, key).unwrap();
        let mut s = CqgSession::new(root, Some((key, sym)), product, LogonParams::default());
        s.start(
            StartContext {
                now: UtcMillis(base_ms()),
                history_from,
                bar_period_ms: 60_000,
            },
            &mut Vec::new(),
        );
        s
    }

    #[test]
    fn requests_the_exact_contract_and_backfills() {
        let z26 = ContractKey::monthly(catalog::ES, 2026, lines_core::Month::new(12).unwrap());
        let from = UtcMillis(base_ms() + 3 * 60_000);
        let mut s = contract_session(z26, Some(from));
        let (_, out) = feed(&mut s, logon_ok());
        let req = &out[0].information_requests[0];
        assert_eq!(
            req.symbol_resolution_request
                .as_ref()
                .unwrap()
                .symbol
                .as_deref(),
            Some("EPZ6")
        );
        let (ev, out) = feed(&mut s, symbol_ok(1, 42, "EPZ6", 0.01, 0.25));
        assert_eq!(ev, vec![MarketEvent::Ready]);
        let p = out[0].time_bar_requests[0]
            .time_bar_parameters
            .as_ref()
            .unwrap();
        assert_eq!(s.time_at_offset(p.from_utc_time), from);
    }

    #[test]
    fn a_different_resolved_contract_is_rejected() {
        let z26 = ContractKey::monthly(catalog::ES, 2026, lines_core::Month::new(12).unwrap());
        let mut s = contract_session(z26, None);
        feed(&mut s, logon_ok());
        let err = s
            .on_frame(
                &frame(symbol_ok(1, 43, "EPH7", 0.01, 0.25)),
                UtcMillis(0),
                &mut Vec::new(),
                &mut Vec::new(),
            )
            .unwrap_err();
        assert!(
            matches!(err, SessionError::BadMetadata(ref m) if m.contains("EPZ6") && m.contains("EPH7")),
            "{err}"
        );
    }
}
