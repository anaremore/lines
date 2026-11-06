//! In-process mock of the CQG WebAPI server.
//!
//! It speaks the real protocol at the frame level: it decodes `ClientMsg` bytes and emits
//! encoded `ServerMsg` bytes built from the generated CQG types. The client cannot tell it
//! apart from a live connection, so swapping in the real server only changes the transport.
//!
//! Market state lives in [`MockMarket`] (shared across reconnects), so a forced disconnect
//! followed by a reconnect exercises gap back-fill.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use cqg_proto::{cqg, historical_2, market_data_2, metadata_2, user_session_2, web_api_2};
use lines_core::{Bar, BarSeries, Price, UtcMillis};
use parking_lot::Mutex;
use prost::Message;
use tokio::time::Instant;

use crate::transport::Transport;

#[derive(Clone, Debug)]
pub struct MockConfig {
    pub quotes_per_sec: u32,
    /// Quotes batched into one `RealTimeMarketData` frame (CQG batches too).
    pub quotes_per_frame: u32,
    pub seed: u64,
    pub history_bars: usize,
    /// Drop the connection after this long (tests reconnect + back-fill).
    pub disconnect_after: Option<Duration>,
    /// How often to push an updated server bar for the forming minute.
    pub bar_update_interval: Duration,
}

impl Default for MockConfig {
    fn default() -> Self {
        MockConfig {
            quotes_per_sec: 5_000,
            quotes_per_frame: 50,
            seed: 0x5EED_1234_ABCD_0001,
            history_bars: 3 * 1440,
            disconnect_after: None,
            bar_update_interval: Duration::from_secs(1),
        }
    }
}

pub const MOCK_CONTRACT_ID: u32 = 1;
const SCALE: f64 = 0.01; // E-mini S&P style: scaled units are cents
const TICK: i64 = 25; // 0.25 index points
/// Live trades move the price rarely; most trades print at the same level.
const LIVE_MOVE_PPM: u64 = 500;

/// Deterministic xorshift64* (no `rand` dependency; reproducible streams).
#[derive(Clone, Debug)]
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// The simulated market, shared by successive mock connections.
pub struct MockMarket {
    rng: Rng,
    price: i64,
    bars: BarSeries,
}

impl MockMarket {
    pub fn new(cfg: &MockConfig, now: UtcMillis) -> Self {
        let mut m = MockMarket {
            rng: Rng(cfg.seed | 1),
            price: 600_000, // 6000.00
            bars: BarSeries::new(UtcMillis::MINUTE, cfg.history_bars + 10_000),
        };
        // Synthesize history ending one bar before `now`.
        let first = UtcMillis(now.0 - cfg.history_bars as i64 * UtcMillis::MINUTE)
            .floor_to(UtcMillis::MINUTE);
        m.fill_bars(first, now.floor_to(UtcMillis::MINUTE));
        m
    }

    /// Random walk on the tick grid, gently pulled back toward 6000. `move_ppm` is the chance
    /// (per million) that this step changes the price at all.
    fn step(&mut self, move_ppm: u64) -> i64 {
        if self.rng.below(1_000_000) >= move_ppm {
            return self.price;
        }
        let r = self.rng.below(1000) as i64;
        let pull = if self.price > 620_000 {
            50
        } else if self.price < 580_000 {
            -50
        } else {
            0
        };
        self.price += if r < 500 + pull { -TICK } else { TICK };
        self.price
    }

    /// Generate whole bars for [from, to) where none exist (history / disconnect gaps).
    fn fill_bars(&mut self, from: UtcMillis, to: UtcMillis) {
        let mut t = from.floor_to(UtcMillis::MINUTE);
        while t < to {
            if self.bars.index_at(t).is_err() {
                let open = self.price;
                let (mut hi, mut lo) = (open, open);
                for _ in 0..40 {
                    let p = self.step(1_000_000);
                    hi = hi.max(p);
                    lo = lo.min(p);
                }
                let vol = 50 + self.rng.below(2000);
                self.bars.upsert(Bar {
                    time: t,
                    open: Price(open),
                    high: Price(hi),
                    low: Price(lo),
                    close: Price(self.price),
                    volume: vol,
                });
            }
            t = UtcMillis(t.0 + UtcMillis::MINUTE);
        }
    }
}

pub struct MockTransport {
    cfg: MockConfig,
    market: Arc<Mutex<MockMarket>>,
    outbox: VecDeque<Bytes>,
    connected_at: Instant,
    base_time: UtcMillis,
    md_started: Option<Instant>,
    quotes_sent: u64,
    bar_request_id: Option<u32>,
    next_bar_push: Instant,
    clock: fn() -> UtcMillis,
}

impl MockTransport {
    pub fn connect(cfg: MockConfig, market: Arc<Mutex<MockMarket>>) -> Self {
        Self::with_clock(cfg, market, crate::now_utc)
    }

    pub fn with_clock(
        cfg: MockConfig,
        market: Arc<Mutex<MockMarket>>,
        clock: fn() -> UtcMillis,
    ) -> Self {
        let now = clock();
        MockTransport {
            cfg,
            market,
            outbox: VecDeque::new(),
            connected_at: Instant::now(),
            // CQG base_time has 1 s resolution.
            base_time: UtcMillis(now.0 - now.0.rem_euclid(1000)),
            md_started: None,
            quotes_sent: 0,
            bar_request_id: None,
            next_bar_push: Instant::now(),
            clock,
        }
    }

    fn off(&self, t: UtcMillis) -> i64 {
        t.0 - self.base_time.0
    }

    fn push(&mut self, msg: web_api_2::ServerMsg) {
        self.outbox.push_back(Bytes::from(msg.encode_to_vec()));
    }

    fn handle_client(&mut self, msg: web_api_2::ClientMsg) {
        let now = (self.clock)();
        if msg.logon.is_some() {
            // Market kept running while we were away: create the missing bars.
            {
                let mut m = self.market.lock();
                let from = m.bars.last().map_or(now, |b| b.time);
                m.fill_bars(from, now.floor_to(UtcMillis::MINUTE));
            }
            let (y, mo, d, h, mi, s) = self.base_time.to_civil();
            self.push(web_api_2::ServerMsg {
                logon_result: Some(user_session_2::LogonResult {
                    result_code: 0,
                    base_time: format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}"),
                    session_token: Some("mock-session".into()),
                    protocol_version_major: cqg_proto::PROTOCOL_VERSION_MAJOR,
                    protocol_version_minor: cqg_proto::PROTOCOL_VERSION_MINOR,
                    server_time: self.off(now),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        if let Some(p) = msg.ping {
            self.push(web_api_2::ServerMsg {
                pong: Some(user_session_2::Pong {
                    token: p.token,
                    ping_utc_time: p.ping_utc_time,
                    pong_utc_time: self.off(now),
                }),
                ..Default::default()
            });
        }
        for req in msg.information_requests {
            if let Some(sym) = req.symbol_resolution_request.and_then(|r| r.symbol) {
                self.push(web_api_2::ServerMsg {
                    information_reports: vec![web_api_2::InformationReport {
                        id: req.id,
                        status_code: 0,
                        symbol_resolution_report: Some(metadata_2::SymbolResolutionReport {
                            contract_metadata: metadata_2::ContractMetadata {
                                contract_id: MOCK_CONTRACT_ID,
                                contract_symbol: format!("{sym}Z6"),
                                title: format!("{sym} (mock E-mini S&P 500)"),
                                description: "Mock contract".into(),
                                correct_price_scale: SCALE,
                                display_price_scale: 2,
                                tick_size: TICK as f64 * SCALE,
                                tick_value: 12.5,
                                currency: "USD".into(),
                                cfi_code: "FFICSX".into(),
                                ..Default::default()
                            },
                            deleted: None,
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                });
            }
        }
        for req in msg.time_bar_requests {
            let Some(p) = req.time_bar_parameters else {
                continue;
            };
            let from = UtcMillis(self.base_time.0 + p.from_utc_time);
            let subscribe = req.request_type
                == Some(historical_2::time_bar_request::RequestType::Subscribe as u32);
            let bars: Vec<historical_2::TimeBar> = {
                let m = self.market.lock();
                m.bars
                    .bars()
                    .iter()
                    .filter(|b| b.time >= from)
                    .map(|b| self.to_time_bar(b))
                    .collect()
            };
            // CQG splits large reports; mimic that.
            let chunks: Vec<_> = bars.chunks(1000).map(|c| c.to_vec()).collect();
            let n = chunks.len().max(1);
            for i in 0..n {
                self.push(web_api_2::ServerMsg {
                    time_bar_reports: vec![historical_2::TimeBarReport {
                        request_id: req.request_id,
                        status_code: if subscribe { 1 } else { 0 },
                        time_bars: chunks.get(i).cloned().unwrap_or_default(),
                        is_report_complete: Some(i + 1 == n),
                        ..Default::default()
                    }],
                    ..Default::default()
                });
            }
            if subscribe {
                self.bar_request_id = Some(req.request_id);
            }
        }
        for sub in msg.market_data_subscriptions {
            self.push(web_api_2::ServerMsg {
                market_data_subscription_statuses: vec![
                    market_data_2::MarketDataSubscriptionStatus {
                        contract_id: sub.contract_id,
                        status_code: 0,
                        level: sub.level,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            });
            let price = self.market.lock().price;
            self.push(web_api_2::ServerMsg {
                real_time_market_data: vec![market_data_2::RealTimeMarketData {
                    contract_id: MOCK_CONTRACT_ID,
                    is_snapshot: Some(true),
                    quotes: vec![
                        quote(
                            market_data_2::quote::Type::Trade,
                            Some(self.off(now)),
                            price,
                            1,
                        ),
                        quote(market_data_2::quote::Type::Bestbid, None, price - TICK, 10),
                        quote(market_data_2::quote::Type::Bestask, None, price, 10),
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            });
            self.md_started = Some(Instant::now());
            self.quotes_sent = 0;
        }
    }

    fn to_time_bar(&self, b: &Bar) -> historical_2::TimeBar {
        historical_2::TimeBar {
            bar_utc_time: self.off(b.time),
            scaled_open_price: Some(b.open.0),
            scaled_high_price: Some(b.high.0),
            scaled_low_price: Some(b.low.0),
            scaled_close_price: Some(b.close.0),
            volume: Some(cqg::Decimal {
                significand: Some(b.volume as i64),
                exponent: Some(0),
            }),
            ..Default::default()
        }
    }

    /// Generate the market-data frames that are due by now.
    fn generate(&mut self) {
        let Some(started) = self.md_started else {
            return;
        };
        let due = (started.elapsed().as_secs_f64() * self.cfg.quotes_per_sec as f64) as u64;
        let per_frame = self.cfg.quotes_per_frame.max(1) as u64;
        // Cap per-call output so a stalled consumer doesn't make us build a huge burst.
        let mut budget = 200u32;
        while self.quotes_sent < due && budget > 0 {
            let n = (due - self.quotes_sent).min(per_frame);
            let now = (self.clock)();
            let mut quotes = Vec::with_capacity(n as usize);
            {
                let mut m = self.market.lock();
                for i in 0..n {
                    let time = if i == 0 {
                        Some(now.0 - self.base_time.0)
                    } else {
                        None
                    };
                    let kind = m.rng.below(10);
                    if kind < 6 {
                        // ~1 tick move per 2000 trades: realistic 1-minute ranges at 3k trades/s.
                        let p = m.step(LIVE_MOVE_PPM);
                        let v = 1 + m.rng.below(9);
                        m.bars.apply_trade(now, Price(p), v);
                        quotes.push(quote(market_data_2::quote::Type::Trade, time, p, v as i64));
                    } else if kind < 8 {
                        let p = m.price - TICK;
                        quotes.push(quote(
                            market_data_2::quote::Type::Bestbid,
                            time,
                            p,
                            1 + m.rng.below(50) as i64,
                        ));
                    } else {
                        let p = m.price + TICK;
                        quotes.push(quote(
                            market_data_2::quote::Type::Bestask,
                            time,
                            p,
                            1 + m.rng.below(50) as i64,
                        ));
                    }
                }
            }
            self.quotes_sent += n;
            self.push(web_api_2::ServerMsg {
                real_time_market_data: vec![market_data_2::RealTimeMarketData {
                    contract_id: MOCK_CONTRACT_ID,
                    quotes,
                    ..Default::default()
                }],
                ..Default::default()
            });
            budget -= 1;
        }
        if budget == 0 {
            // Consumer fell far behind: skip ahead instead of buffering unboundedly.
            self.quotes_sent = due;
        }
        if let Some(id) = self.bar_request_id
            && Instant::now() >= self.next_bar_push
        {
            self.next_bar_push = Instant::now() + self.cfg.bar_update_interval;
            let last = self.market.lock().bars.last().copied();
            if let Some(b) = last {
                let tb = self.to_time_bar(&b);
                self.push(web_api_2::ServerMsg {
                    time_bar_reports: vec![historical_2::TimeBarReport {
                        request_id: id,
                        status_code: 3, // BAR_REPORT_STATUS_CODE_UPDATE
                        time_bars: vec![tb],
                        ..Default::default()
                    }],
                    ..Default::default()
                });
            }
        }
    }
}

fn quote(
    kind: market_data_2::quote::Type,
    time: Option<i64>,
    price: i64,
    volume: i64,
) -> market_data_2::Quote {
    market_data_2::Quote {
        r#type: kind as u32,
        quote_utc_time: time,
        scaled_price: price,
        volume: Some(cqg::Decimal {
            significand: Some(volume),
            exponent: Some(0),
        }),
        ..Default::default()
    }
}

impl Transport for MockTransport {
    async fn send(&mut self, frame: Bytes) -> Result<()> {
        let msg =
            web_api_2::ClientMsg::decode(frame).map_err(|e| anyhow!("mock: bad ClientMsg: {e}"))?;
        self.handle_client(msg);
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Bytes>> {
        // Cancel-safe: state is only mutated synchronously, after the sleep completes.
        loop {
            if let Some(limit) = self.cfg.disconnect_after
                && self.connected_at.elapsed() >= limit
            {
                return Err(anyhow!("mock: simulated connection drop"));
            }
            if let Some(f) = self.outbox.pop_front() {
                return Ok(Some(f));
            }
            self.generate();
            if self.outbox.is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    }
}
