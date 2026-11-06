//! In-process mock of the CQG WebAPI server.
//!
//! It speaks the real protocol at the frame level: it decodes `ClientMsg` bytes and emits
//! encoded `ServerMsg` bytes built from the generated CQG types. The client cannot tell it
//! apart from a live connection, so swapping in the real server only changes the transport.
//!
//! Market state lives in [`MockMarket`] (shared across reconnects), so a forced disconnect
//! followed by a reconnect exercises gap back-fill.
//!
//! Depth (roadmap 5): a one-tick-wide book of [`DEPTH`] levels per side around the walk's
//! price, sent with CQG's DOM quotes (`TYPE_BID` / `TYPE_ASK`, volume = new total, 0 removes).
//! The subscription snapshot carries the whole book; when the price moves, the book shifts and
//! the levels that changed side or fell off are sent in the same frame, so the book is never
//! crossed at a frame boundary. Trades carry CQG sales conditions (aggressor side).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use cqg_proto::{cqg, historical_2, market_data_2, metadata_2, user_session_2, web_api_2};
use lines_core::{Bar, BarSeries, Price, Qty, UtcMillis};
use parking_lot::Mutex;
use prost::Message;
use tokio::time::Instant;

use crate::provider::{Frame, FrameKind};
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
    /// Stop generating trades this long after the market was created (across reconnects; bar
    /// updates continue), so a test can let the client converge and compare with the mock.
    pub stop_trading_after: Option<Duration>,
    /// Use this market instead of creating one (tests read the mock's own bars as truth).
    pub market: Option<SharedMockMarket>,
}

/// A mock market shared between the feed and a test.
#[derive(Clone)]
pub struct SharedMockMarket(pub Arc<Mutex<MockMarket>>);

impl std::fmt::Debug for SharedMockMarket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedMockMarket")
    }
}

impl SharedMockMarket {
    pub fn new(cfg: &MockConfig, now: UtcMillis) -> Self {
        SharedMockMarket(Arc::new(Mutex::new(MockMarket::new(cfg, now))))
    }
    /// The mock exchange's own bars: every trade it generated, aggregated (ground truth).
    pub fn bars(&self) -> Vec<Bar> {
        self.0.lock().bars.bars().to_vec()
    }
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
            stop_trading_after: None,
            market: None,
        }
    }
}

/// The CQG symbol the mock resolves (CQG naming for E-mini S&P 500).
pub const MOCK_SYMBOL: &str = "EP";
/// What the root resolves to (CQG resolves a bare root to its front month).
pub const MOCK_FRONT_CONTRACT: &str = "EPZ6";

/// The mock resolves `EP` and any `EP` contract symbol (`EP` + month code + year digit).
fn mock_knows(sym: &str) -> bool {
    match sym.strip_prefix(MOCK_SYMBOL) {
        Some("") => true,
        Some(rest) => {
            let mut c = rest.chars();
            matches!((c.next(), c.next(), c.next()), (Some(m), Some(d), None)
                if lines_core::Month::from_code(m).is_some() && d.is_ascii_digit())
        }
        None => false,
    }
}
pub const MOCK_CONTRACT_ID: u32 = 1;
const SCALE: f64 = 0.01; // E-mini S&P style: scaled units are cents
const TICK: i64 = 25; // 0.25 index points
/// Live trades move the price rarely; most trades print at the same level.
const LIVE_MOVE_PPM: u64 = 500;
/// Book levels per side.
pub const DEPTH: usize = 10;

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
    /// Best bid; the best ask is one tick above.
    price: i64,
    /// Sizes by distance from the inside: `bids[k]` at `price - k ticks`, `asks[k]` at
    /// `price + (k + 1) ticks`.
    bids: [i64; DEPTH],
    asks: [i64; DEPTH],
    bars: BarSeries,
    created: Instant,
    /// Newest trade time stamped so far.
    last_trade: UtcMillis,
    /// Newest `up_to_utc_time` reported. Trades are always stamped strictly after it, so every
    /// snapshot is *closed*: it includes every trade at or before its time and none after
    /// (CQG's documented semantics, and what the client's stitch rule relies on).
    snapshot: UtcMillis,
}

impl MockMarket {
    /// Time for the next trade: now, but never at or before the last snapshot.
    fn stamp(&mut self, now: UtcMillis) -> UtcMillis {
        let t = now.max(self.last_trade).max(UtcMillis(self.snapshot.0 + 1));
        self.last_trade = t;
        t
    }

    /// Close a snapshot now: all trades stamped so far are included.
    fn close_snapshot(&mut self, now: UtcMillis) -> UtcMillis {
        self.snapshot = now.max(self.last_trade).max(self.snapshot);
        self.snapshot
    }
}

impl MockMarket {
    pub fn new(cfg: &MockConfig, now: UtcMillis) -> Self {
        let mut m = MockMarket {
            rng: Rng(cfg.seed | 1),
            price: 600_000, // 6000.00
            bids: [0; DEPTH],
            asks: [0; DEPTH],
            bars: BarSeries::new(UtcMillis::MINUTE, cfg.history_bars + 10_000),
            created: Instant::now(),
            last_trade: UtcMillis(i64::MIN),
            snapshot: UtcMillis(i64::MIN + 1),
        };
        // Synthesize history ending one bar before `now`.
        let first = UtcMillis(now.0 - cfg.history_bars as i64 * UtcMillis::MINUTE)
            .floor_to(UtcMillis::MINUTE);
        m.fill_bars(first, now.floor_to(UtcMillis::MINUTE));
        for k in 0..DEPTH {
            m.bids[k] = m.size(k);
            m.asks[k] = m.size(k);
        }
        m
    }

    /// A plausible resting size, growing away from the inside.
    fn size(&mut self, k: usize) -> i64 {
        5 + self.rng.below(40 + 30 * k as u64) as i64
    }

    fn bid_price(&self, k: usize) -> i64 {
        self.price - k as i64 * TICK
    }

    fn ask_price(&self, k: usize) -> i64 {
        self.price + (k as i64 + 1) * TICK
    }

    /// The whole book as DOM quotes, plus the best bid/ask.
    fn book_quotes(&self, out: &mut Vec<market_data_2::Quote>) {
        use market_data_2::quote::Type;
        out.push(quote(Type::Bestbid, None, self.bid_price(0), self.bids[0]));
        out.push(quote(Type::Bestask, None, self.ask_price(0), self.asks[0]));
        for k in 0..DEPTH {
            out.push(quote(Type::Bid, None, self.bid_price(k), self.bids[k]));
            out.push(quote(Type::Ask, None, self.ask_price(k), self.asks[k]));
        }
    }

    /// The price just moved one tick (`up` or down; `self.price` is already the new best
    /// bid): shift the book and emit every level that changed.
    fn shift_book(&mut self, up: bool, out: &mut Vec<market_data_2::Quote>) {
        use market_data_2::quote::Type;
        if up {
            // The old best ask is now the best bid; the farthest bid falls off.
            out.push(quote(Type::Ask, None, self.bid_price(0), 0));
            out.push(quote(Type::Bid, None, self.bid_price(DEPTH), 0));
            self.bids.rotate_right(1);
            self.bids[0] = self.size(0);
            self.asks.rotate_left(1);
            self.asks[DEPTH - 1] = self.size(DEPTH - 1);
            out.push(quote(Type::Bid, None, self.bid_price(0), self.bids[0]));
            out.push(quote(
                Type::Ask,
                None,
                self.ask_price(DEPTH - 1),
                self.asks[DEPTH - 1],
            ));
        } else {
            // The old best bid is now the best ask; the farthest ask falls off.
            out.push(quote(Type::Bid, None, self.ask_price(0), 0));
            out.push(quote(Type::Ask, None, self.ask_price(DEPTH), 0));
            self.asks.rotate_right(1);
            self.asks[0] = self.size(0);
            self.bids.rotate_left(1);
            self.bids[DEPTH - 1] = self.size(DEPTH - 1);
            out.push(quote(Type::Ask, None, self.ask_price(0), self.asks[0]));
            out.push(quote(
                Type::Bid,
                None,
                self.bid_price(DEPTH - 1),
                self.bids[DEPTH - 1],
            ));
        }
        out.push(quote(Type::Bestbid, None, self.bid_price(0), self.bids[0]));
        out.push(quote(Type::Bestask, None, self.ask_price(0), self.asks[0]));
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
                    volume: Qty(vol as i64),
                });
            }
            t = UtcMillis(t.0 + UtcMillis::MINUTE);
        }
    }
}

pub struct MockTransport {
    cfg: MockConfig,
    market: Arc<Mutex<MockMarket>>,
    outbox: VecDeque<Frame>,
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
        self.outbox
            .push_back(Frame::binary(Bytes::from(msg.encode_to_vec())));
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
                if !mock_knows(&sym) {
                    self.push(web_api_2::ServerMsg {
                        information_reports: vec![web_api_2::InformationReport {
                            id: req.id,
                            status_code: 104, // STATUS_CODE_NOT_FOUND
                            text_message: Some(format!(
                                "mock knows only {MOCK_SYMBOL} and its contracts, not {sym}"
                            )),
                            ..Default::default()
                        }],
                        ..Default::default()
                    });
                    continue;
                }
                self.push(web_api_2::ServerMsg {
                    information_reports: vec![web_api_2::InformationReport {
                        id: req.id,
                        status_code: 0,
                        symbol_resolution_report: Some(metadata_2::SymbolResolutionReport {
                            contract_metadata: metadata_2::ContractMetadata {
                                contract_id: MOCK_CONTRACT_ID,
                                contract_symbol: if sym == MOCK_SYMBOL {
                                    MOCK_FRONT_CONTRACT.to_owned()
                                } else {
                                    sym.clone()
                                },
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
            let (bars, up_to) = {
                let mut m = self.market.lock();
                let up_to = m.close_snapshot(now);
                let bars: Vec<historical_2::TimeBar> = m
                    .bars
                    .bars()
                    .iter()
                    .filter(|b| b.time >= from)
                    .map(|b| self.to_time_bar(b))
                    .collect();
                (bars, up_to)
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
                        up_to_utc_time: (i + 1 == n).then(|| self.off(up_to)),
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
            let mut quotes = Vec::with_capacity(2 * DEPTH + 3);
            {
                let m = self.market.lock();
                quotes.push(quote(
                    market_data_2::quote::Type::Trade,
                    Some(self.off(now)),
                    m.price,
                    1,
                ));
                let dom = market_data_2::market_data_subscription::Level::TradesBbaDom as u32;
                if sub.level == dom {
                    m.book_quotes(&mut quotes);
                }
            }
            self.push(web_api_2::ServerMsg {
                real_time_market_data: vec![market_data_2::RealTimeMarketData {
                    contract_id: MOCK_CONTRACT_ID,
                    is_snapshot: Some(true),
                    quotes,
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
                significand: Some(b.volume.0),
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
        let trading = self
            .cfg
            .stop_trading_after
            .is_none_or(|d| self.market.lock().created.elapsed() < d);
        while trading && self.quotes_sent < due && budget > 0 {
            let n = (due - self.quotes_sent).min(per_frame);
            let mut quotes = Vec::with_capacity(n as usize);
            {
                let mut m = self.market.lock();
                // One timestamp per frame, strictly after the last snapshot.
                let now = m.stamp((self.clock)());
                for i in 0..n {
                    let time = if i == 0 {
                        Some(now.0 - self.base_time.0)
                    } else {
                        None
                    };
                    let kind = m.rng.below(10);
                    if kind < 6 {
                        // ~1 tick move per 2000 trades: realistic 1-minute ranges at 3k trades/s.
                        let old = m.price;
                        let bid = m.step(LIVE_MOVE_PPM);
                        if bid != old {
                            m.shift_book(bid > old, &mut quotes);
                        }
                        // Buyers lift the offer, sellers hit the bid.
                        let buy = m.rng.below(2) == 0;
                        let p = if buy { bid + TICK } else { bid };
                        let v = 1 + m.rng.below(9);
                        m.bars.apply_trade(now, Price(p), Qty(v as i64));
                        let mut q = quote(market_data_2::quote::Type::Trade, time, p, v as i64);
                        q.sales_condition = Some(if buy {
                            market_data_2::quote::SalesCondition::BuySideAggressor
                        } else {
                            market_data_2::quote::SalesCondition::SellSideAggressor
                        } as u32);
                        quotes.push(q);
                    } else {
                        // A resting size changes somewhere in the book.
                        use market_data_2::quote::Type;
                        let k = m.rng.below(DEPTH as u64) as usize;
                        let size = m.size(k);
                        let (kind, best, price) = if m.rng.below(2) == 0 {
                            m.bids[k] = size;
                            (Type::Bid, Type::Bestbid, m.bid_price(k))
                        } else {
                            m.asks[k] = size;
                            (Type::Ask, Type::Bestask, m.ask_price(k))
                        };
                        quotes.push(quote(kind, time, price, size));
                        if k == 0 {
                            quotes.push(quote(best, None, price, size));
                        }
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
            let (last, up_to) = {
                let mut m = self.market.lock();
                let up_to = m.close_snapshot((self.clock)());
                (m.bars.last().copied(), up_to)
            };
            if let Some(b) = last {
                let tb = self.to_time_bar(&b);
                self.push(web_api_2::ServerMsg {
                    time_bar_reports: vec![historical_2::TimeBarReport {
                        request_id: id,
                        status_code: 3, // BAR_REPORT_STATUS_CODE_UPDATE
                        time_bars: vec![tb],
                        up_to_utc_time: Some(self.off(up_to)),
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
    async fn send(&mut self, frame: Frame) -> Result<()> {
        if frame.kind != FrameKind::Binary {
            return Err(anyhow!("mock: CQG only speaks binary frames"));
        }
        let msg = web_api_2::ClientMsg::decode(frame.data)
            .map_err(|e| anyhow!("mock: bad ClientMsg: {e}"))?;
        self.handle_client(msg);
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Frame>> {
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
