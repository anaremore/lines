//! CQG WebAPI session logic, free of I/O.
//!
//! One `Session` per connection: request ids restart at 1 and contract ids are re-resolved,
//! because CQG contract ids are only valid for the session that issued them. Bars live in
//! the store and survive reconnects; on reconnect we request bars from the newest bar we
//! hold, which back-fills the gap.
//!
//! Because the logic is deterministic given the server frames, replaying a recording drives
//! exactly the same code path (outgoing messages are simply discarded).

use cqg_proto::{cqg, historical_2, market_data_2, metadata_2, user_session_2, web_api_2};
use lines_core::{Bar, Price, PriceFormat, UtcMillis};

use crate::store::{ChartData, FeedStatus, InstrumentInfo};

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

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub symbol: String,
    pub logon: LogonParams,
    /// How far back to request bars when we have none.
    pub history_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// Credentials/entitlement problems: retrying won't help.
    #[error("logon rejected (code {code}): {text}")]
    LogonRejected { code: u32, text: String },
    #[error("server logged us off: {0}")]
    LoggedOff(String),
    #[error("symbol resolution failed: {0}")]
    SymbolFailed(String),
    #[error("contract metadata unusable: {0}")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    AwaitLogon,
    AwaitSymbol,
    Live,
}

pub struct Session {
    cfg: SessionConfig,
    state: State,
    base_time: UtcMillis,
    next_id: u32,
    symbol_request_id: u32,
    bar_request_id: u32,
    contract_id: Option<u32>,
    format: Option<PriceFormat>,
}

const BAR_PERIOD_MS: i64 = UtcMillis::MINUTE;

impl Session {
    pub fn new(cfg: SessionConfig) -> Self {
        Session {
            cfg,
            state: State::AwaitLogon,
            base_time: UtcMillis(0),
            next_id: 1,
            symbol_request_id: 0,
            bar_request_id: 0,
            contract_id: None,
            format: None,
        }
    }

    pub fn is_live(&self) -> bool {
        self.state == State::Live
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Server-relative time offset (all CQG sint64 times are ms from `base_time`).
    pub fn to_offset(&self, t: UtcMillis) -> i64 {
        t.0 - self.base_time.0
    }
    pub fn from_offset(&self, offset: i64) -> UtcMillis {
        UtcMillis(self.base_time.0 + offset)
    }

    pub fn logon_msg(&self) -> ClientMsg {
        let l = &self.cfg.logon;
        ClientMsg {
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
        }
    }

    /// Keep-alive / latency probe. Only meaningful after logon (needs base_time).
    pub fn ping_msg(&self, now: UtcMillis) -> Option<ClientMsg> {
        (self.state != State::AwaitLogon).then(|| ClientMsg {
            ping: Some(user_session_2::Ping {
                token: None,
                ping_utc_time: self.to_offset(now),
            }),
            ..Default::default()
        })
    }

    /// Apply one decoded server message. Replies are appended to `out`.
    pub fn on_server_msg(
        &mut self,
        msg: &ServerMsg,
        now: UtcMillis,
        data: &mut ChartData,
        out: &mut Vec<ClientMsg>,
    ) -> Result<(), SessionError> {
        if let Some(r) = &msg.logon_result {
            self.on_logon_result(r, now, data, out)?;
        }
        if let Some(lo) = &msg.logged_off {
            return Err(SessionError::LoggedOff(
                lo.text_message.clone().unwrap_or_default(),
            ));
        }
        if let Some(p) = &msg.ping {
            out.push(ClientMsg {
                pong: Some(user_session_2::Pong {
                    token: p.token.clone(),
                    ping_utc_time: p.ping_utc_time,
                    pong_utc_time: self.to_offset(now),
                }),
                ..Default::default()
            });
        }
        for rep in &msg.information_reports {
            if rep.id == self.symbol_request_id && self.state == State::AwaitSymbol {
                self.on_symbol_report(rep, now, data, out)?;
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
                self.on_time_bar_report(rep, data);
            }
        }
        for rtmd in &msg.real_time_market_data {
            if Some(rtmd.contract_id) == self.contract_id {
                self.on_market_data(rtmd, data);
            }
        }
        Ok(())
    }

    fn on_logon_result(
        &mut self,
        r: &user_session_2::LogonResult,
        now: UtcMillis,
        data: &mut ChartData,
        out: &mut Vec<ClientMsg>,
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
        let _ = now;
        data.status = FeedStatus::Resolving;
        out.push(ClientMsg {
            information_requests: vec![InformationRequest {
                id: self.symbol_request_id,
                subscribe: Some(false),
                symbol_resolution_request: Some(metadata_2::SymbolResolutionRequest {
                    symbol: Some(self.cfg.symbol.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        });
        Ok(())
    }

    fn on_symbol_report(
        &mut self,
        rep: &web_api_2::InformationReport,
        now: UtcMillis,
        data: &mut ChartData,
        out: &mut Vec<ClientMsg>,
    ) -> Result<(), SessionError> {
        if rep.status_code > 100 {
            return Err(SessionError::SymbolFailed(format!(
                "{} (code {})",
                rep.text_message.clone().unwrap_or_default(),
                rep.status_code
            )));
        }
        let Some(meta) = rep
            .symbol_resolution_report
            .as_ref()
            .map(|r| &r.contract_metadata)
        else {
            return Err(SessionError::SymbolFailed(
                "report without contract metadata".into(),
            ));
        };
        let format = PriceFormat::from_cqg(
            meta.correct_price_scale,
            meta.display_price_scale,
            meta.tick_size,
        )
        .map_err(|e| SessionError::BadMetadata(e.to_string()))?;
        if let Some(prev) = &data.instrument
            && (prev.format != format || prev.contract_symbol != meta.contract_symbol)
        {
            // Different contract (e.g. front-month roll) or rescaled prices: old bars are
            // not comparable. Start over rather than mix them.
            tracing::warn!(old = %prev.contract_symbol, new = %meta.contract_symbol, "contract changed; clearing bars");
            data.series.clear();
        }
        self.contract_id = Some(meta.contract_id);
        self.format = Some(format);
        data.instrument = Some(InstrumentInfo {
            requested_symbol: self.cfg.symbol.clone(),
            contract_symbol: meta.contract_symbol.clone(),
            title: meta.title.clone(),
            contract_id: meta.contract_id,
            format,
        });

        // Back-fill from our newest bar (it may be incomplete, so re-request it), else history.
        let from = match data.series.last() {
            Some(b) => b.time,
            None => UtcMillis(now.0 - self.cfg.history_ms).floor_to(BAR_PERIOD_MS),
        };
        self.bar_request_id = self.id();
        let md_request_id = self.id();
        out.push(ClientMsg {
            time_bar_requests: vec![TimeBarRequest {
                request_id: self.bar_request_id,
                request_type: Some(time_bar_request::RequestType::Subscribe as u32),
                time_bar_parameters: Some(TimeBarParameters {
                    contract_id: meta.contract_id,
                    bar_unit: BarUnit::Min as u32,
                    unit_number: Some(1),
                    from_utc_time: self.to_offset(from),
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
        });
        self.state = State::Live;
        data.status = FeedStatus::Live;
        Ok(())
    }

    fn on_time_bar_report(&mut self, rep: &historical_2::TimeBarReport, data: &mut ChartData) {
        if rep.status_code > 100 {
            let text = rep
                .details
                .as_ref()
                .and_then(|t| t.text.clone())
                .unwrap_or_default();
            tracing::warn!(code = rep.status_code, "time bar request failed: {text}");
            return;
        }
        for tb in &rep.time_bars {
            if let Some(bar) = self.convert_bar(tb) {
                data.series.upsert(bar);
            }
        }
    }

    #[allow(deprecated)] // scaled_volume: fallback for servers that omit `volume`
    fn convert_bar(&self, tb: &TimeBar) -> Option<Bar> {
        // Bars with no trades have no prices; nothing to draw.
        Some(Bar {
            time: self.from_offset(tb.bar_utc_time).floor_to(BAR_PERIOD_MS),
            open: Price(tb.scaled_open_price?),
            high: Price(tb.scaled_high_price?),
            low: Price(tb.scaled_low_price?),
            close: Price(tb.scaled_close_price?),
            volume: volume_of(tb.volume.as_ref(), tb.scaled_volume),
        })
    }

    fn on_market_data(&mut self, md: &RealTimeMarketData, data: &mut ChartData) {
        if let (Some(scale), Some(fmt)) = (md.correct_price_scale, self.format)
            && (scale - fmt.scale.num() as f64 / fmt.scale.den() as f64).abs()
                > f64::EPSILON * scale
        {
            // CQG may rescale a contract mid-session; supporting that means re-resolving.
            tracing::error!(
                scale,
                "contract price scale changed mid-session; ignoring update"
            );
            return;
        }
        let snapshot = md.is_snapshot.unwrap_or(false);
        // quote_utc_time is only present when it differs from the previous quote's.
        let mut t = data.last_quote_time.map(|t| self.to_offset(t)).unwrap_or(0);
        for q in &md.quotes {
            if let Some(qt) = q.quote_utc_time {
                t = qt;
            }
            self.on_quote(q, self.from_offset(t), snapshot, data);
        }
        data.last_quote_time = Some(self.from_offset(t));
        data.stats.quotes_in += md.quotes.len() as u64;
        // Corrections need trade-level history to apply exactly; server bar updates
        // (subscribed above) repair the affected bars instead. Count them for visibility.
        data.stats.corrections_ignored += md.corrections.len() as u64;
    }

    #[allow(deprecated)] // scaled_volume: fallback for servers that omit `volume`
    fn on_quote(&self, q: &Quote, time: UtcMillis, snapshot: bool, data: &mut ChartData) {
        let price = Price(q.scaled_price);
        let past = q.indicators.contains(&(quote::Indicator::Past as u32));
        match q.r#type {
            x if x == quote::Type::Trade as u32 => {
                data.last_trade = Some(price);
                if !snapshot && !past {
                    data.series.apply_trade(
                        time,
                        price,
                        volume_of(q.volume.as_ref(), q.scaled_volume),
                    );
                    data.stats.trades_in += 1;
                }
            }
            x if x == quote::Type::Bestbid as u32 => data.bid = Some(price),
            x if x == quote::Type::Bestask as u32 => data.ask = Some(price),
            _ => {}
        }
    }
}

/// CQG volume: prefer `cqg.Decimal volume`, fall back to deprecated `scaled_volume`
/// (assumes volume_scale 1, true for futures). Fractional volume is truncated.
pub fn volume_of(decimal: Option<&cqg::Decimal>, scaled: Option<u64>) -> u64 {
    match decimal {
        Some(d) => {
            let sig = d.significand.unwrap_or(0).max(0) as u128;
            let exp = d.exponent.unwrap_or(0);
            let v = if exp >= 0 {
                sig.saturating_mul(10u128.saturating_pow(exp as u32))
            } else {
                sig / 10u128.saturating_pow(exp.unsigned_abs())
            };
            v.min(u64::MAX as u128) as u64
        }
        None => scaled.unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ChartStore;

    fn cfg() -> SessionConfig {
        SessionConfig {
            symbol: "EP".into(),
            logon: LogonParams::default(),
            history_ms: 60 * 60_000,
        }
    }

    fn logon_ok(base: &str) -> ServerMsg {
        ServerMsg {
            logon_result: Some(user_session_2::LogonResult {
                result_code: 0,
                base_time: base.into(),
                protocol_version_major: 2,
                protocol_version_minor: 230,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn symbol_ok(id: u32, contract_id: u32, symbol: &str) -> ServerMsg {
        ServerMsg {
            information_reports: vec![web_api_2::InformationReport {
                id,
                status_code: 0,
                symbol_resolution_report: Some(metadata_2::SymbolResolutionReport {
                    contract_metadata: metadata_2::ContractMetadata {
                        contract_id,
                        contract_symbol: symbol.into(),
                        title: "E-mini".into(),
                        correct_price_scale: 0.01,
                        display_price_scale: 2,
                        tick_size: 0.25,
                        ..Default::default()
                    },
                    deleted: None,
                }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn trade(offset: Option<i64>, price: i64, vol: i64) -> Quote {
        Quote {
            r#type: quote::Type::Trade as u32,
            quote_utc_time: offset,
            scaled_price: price,
            volume: Some(cqg::Decimal {
                significand: Some(vol),
                exponent: Some(0),
            }),
            ..Default::default()
        }
    }

    const BASE: &str = "2026-11-06T14:00:00";
    fn base_ms() -> i64 {
        UtcMillis::parse_iso(BASE).unwrap().0
    }

    #[test]
    fn full_handshake_and_trades() {
        let store = ChartStore::new(60_000, 1000);
        let mut d = store.read();
        let mut s = Session::new(cfg());
        let now = UtcMillis(base_ms() + 5_000);
        let mut out = Vec::new();

        assert!(s.logon_msg().logon.is_some());
        s.on_server_msg(&logon_ok(BASE), now, &mut d, &mut out)
            .unwrap();
        let req = &out[0].information_requests[0];
        assert_eq!(
            req.symbol_resolution_request
                .as_ref()
                .unwrap()
                .symbol
                .as_deref(),
            Some("EP")
        );

        let id = req.id;
        out.clear();
        s.on_server_msg(&symbol_ok(id, 77, "EPZ6"), now, &mut d, &mut out)
            .unwrap();
        assert!(s.is_live());
        assert_eq!(d.instrument.as_ref().unwrap().format.tick, 25);
        let sub = &out[0];
        assert_eq!(sub.market_data_subscriptions[0].contract_id, 77);
        let bars_req = &sub.time_bar_requests[0];
        let p = bars_req.time_bar_parameters.as_ref().unwrap();
        // No bars yet: request history_ms back, as an offset from base_time.
        assert_eq!(
            s.from_offset(p.from_utc_time),
            UtcMillis(now.0 - 3_600_000).floor_to(60_000)
        );

        // Trades: second quote inherits the first quote's time.
        let md = ServerMsg {
            real_time_market_data: vec![RealTimeMarketData {
                contract_id: 77,
                quotes: vec![
                    trade(Some(61_000), 500_000, 2),
                    trade(None, 500_025, 3),
                    trade(Some(125_000), 499_975, 1),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        s.on_server_msg(&md, now, &mut d, &mut out).unwrap();
        let bars = d.series.bars();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].time, UtcMillis(base_ms() + 60_000));
        assert_eq!(
            (bars[0].open, bars[0].close, bars[0].volume),
            (Price(500_000), Price(500_025), 5)
        );
        assert_eq!(bars[1].time, UtcMillis(base_ms() + 120_000));
        assert_eq!(d.last_trade, Some(Price(499_975)));
        assert_eq!(d.stats.trades_in, 3);

        // Market data for another contract id is ignored.
        let other = ServerMsg {
            real_time_market_data: vec![RealTimeMarketData {
                contract_id: 5,
                quotes: vec![trade(Some(1), 1, 1)],
                ..Default::default()
            }],
            ..Default::default()
        };
        s.on_server_msg(&other, now, &mut d, &mut out).unwrap();
        assert_eq!(d.stats.trades_in, 3);
    }

    #[test]
    fn snapshot_trades_do_not_count_as_volume() {
        let store = ChartStore::new(60_000, 1000);
        let mut d = store.read();
        let mut s = Session::new(cfg());
        let mut out = Vec::new();
        let now = UtcMillis(base_ms());
        s.on_server_msg(&logon_ok(BASE), now, &mut d, &mut out)
            .unwrap();
        s.on_server_msg(&symbol_ok(1, 9, "EPZ6"), now, &mut d, &mut out)
            .unwrap();
        let snap = ServerMsg {
            real_time_market_data: vec![RealTimeMarketData {
                contract_id: 9,
                is_snapshot: Some(true),
                quotes: vec![trade(Some(0), 123_400, 99)],
                ..Default::default()
            }],
            ..Default::default()
        };
        s.on_server_msg(&snap, now, &mut d, &mut out).unwrap();
        assert_eq!(d.last_trade, Some(Price(123_400)));
        assert!(d.series.is_empty());
    }

    #[test]
    fn server_ping_gets_pong() {
        let store = ChartStore::new(60_000, 10);
        let mut d = store.read();
        let mut s = Session::new(cfg());
        let mut out = Vec::new();
        let now = UtcMillis(base_ms() + 1_000);
        s.on_server_msg(&logon_ok(BASE), now, &mut d, &mut out)
            .unwrap();
        out.clear();
        let ping = ServerMsg {
            ping: Some(user_session_2::Ping {
                token: Some("t".into()),
                ping_utc_time: 900,
            }),
            ..Default::default()
        };
        s.on_server_msg(&ping, now, &mut d, &mut out).unwrap();
        let pong = out[0].pong.as_ref().unwrap();
        assert_eq!(
            (
                pong.token.as_deref(),
                pong.ping_utc_time,
                pong.pong_utc_time
            ),
            (Some("t"), 900, 1_000)
        );
    }

    #[test]
    fn logon_rejection_is_not_retryable() {
        let store = ChartStore::new(60_000, 10);
        let mut d = store.read();
        let mut s = Session::new(cfg());
        let mut msg = logon_ok(BASE);
        msg.logon_result.as_mut().unwrap().result_code = 101;
        msg.logon_result.as_mut().unwrap().text_message = Some("bad password".into());
        let err = s
            .on_server_msg(&msg, UtcMillis(0), &mut d, &mut Vec::new())
            .unwrap_err();
        assert!(!err.is_retryable());
        assert!(err.to_string().contains("bad password"));
    }

    #[test]
    fn reconnect_backfills_from_last_bar_and_contract_roll_clears() {
        let store = ChartStore::new(60_000, 1000);
        let mut d = store.read();
        let now = UtcMillis(base_ms() + 10 * 60_000);
        d.series
            .apply_trade(UtcMillis(base_ms() + 3 * 60_000 + 5), Price(100), 1);

        // Reconnect with the same contract (new session-scoped id): keep bars, back-fill.
        d.instrument = Some(InstrumentInfo {
            requested_symbol: "EP".into(),
            contract_symbol: "EPZ6".into(),
            title: String::new(),
            contract_id: 1,
            format: PriceFormat::from_cqg(0.01, 2, 0.25).unwrap(),
        });
        let mut s = Session::new(cfg());
        let mut out = Vec::new();
        s.on_server_msg(&logon_ok(BASE), now, &mut d, &mut out)
            .unwrap();
        out.clear();
        s.on_server_msg(&symbol_ok(1, 42, "EPZ6"), now, &mut d, &mut out)
            .unwrap();
        let p = out[0].time_bar_requests[0]
            .time_bar_parameters
            .as_ref()
            .unwrap();
        assert_eq!(
            s.from_offset(p.from_utc_time),
            UtcMillis(base_ms() + 3 * 60_000)
        );
        assert_eq!(d.series.len(), 1);

        // Front-month roll: different contract => bars cleared.
        let mut s = Session::new(cfg());
        s.on_server_msg(&logon_ok(BASE), now, &mut d, &mut out)
            .unwrap();
        s.on_server_msg(&symbol_ok(1, 43, "EPH7"), now, &mut d, &mut out)
            .unwrap();
        assert!(d.series.is_empty());
    }

    #[test]
    fn decimal_volume() {
        let d = |s, e| cqg::Decimal {
            significand: Some(s),
            exponent: Some(e),
        };
        assert_eq!(volume_of(Some(&d(5, 0)), None), 5);
        assert_eq!(volume_of(Some(&d(5, 2)), None), 500);
        assert_eq!(volume_of(Some(&d(1500, -3)), None), 1);
        assert_eq!(volume_of(None, Some(7)), 7);
        assert_eq!(volume_of(None, None), 0);
    }
}
