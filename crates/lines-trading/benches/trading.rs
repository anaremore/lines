//! Trading core hot paths (docs/benchmarks.md), driven exactly as `tests/trading.rs` drives
//! them: `TradingCore` with the paper broker on a manual clock, no threads.
//!
//! - `core/place_ack_cancel`: one command → event round trip each way: place a limit, the ack
//!   after the paper latency, cancel it, the cancel's result, events drained.
//! - `paper/market_with_resting`: market batches (one trade each) reaching 20 resting limits
//!   (trade-through fills, refilled as they fill), per batch.
//! - `paper/market_order_book_walk`: a market order walking a 20-level book, through its ack
//!   and fill.
//!
//! Run: `cargo bench -p lines-trading --bench trading`. Synthetic, deterministic.

use std::hint::black_box;
use std::time::Instant;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::{
    AccountConfig, AccountId, Command, Event, FillModel, MarketInput, Money, OrderKind, OrderSpec,
    PaperBroker, PaperConfig, Side, Source, TradingCore,
};

const LAT: i64 = 20;
const TICK: i64 = 25;
const MID: i64 = 600_000;

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn acct() -> AccountId {
    AccountId::new("paper-1")
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: usize,
}

impl Rig {
    fn new() -> Rig {
        let mut b = PaperBroker::new(PaperConfig {
            latency_ms: LAT,
            fill_model: FillModel::TradeThrough,
            ..Default::default()
        });
        b.open_account(acct(), Money::whole(1_000_000));
        let mut core = TradingCore::new(Box::new(b));
        core.add_account(AccountConfig::paper("paper-1", 1_000_000), None);
        let mut r = Rig {
            core,
            // Tuesday 2026-11-10 15:00 UTC: CME open.
            t: UtcMillis::parse_iso("2026-11-10T15:00:00Z").unwrap().0,
            events: 0,
        };
        // A 20-level book and a last trade, so marks, bands and market orders have prices.
        let mut book = vec![MarketInput::BookClear];
        for k in 0..20 {
            book.push(MarketInput::Book {
                side: BookSide::Bid,
                price: Price(MID - k * TICK),
                qty: Qty(50),
            });
            book.push(MarketInput::Book {
                side: BookSide::Ask,
                price: Price(MID + (k + 1) * TICK),
                qty: Qty(50),
            });
        }
        book.push(MarketInput::Trade {
            time: UtcMillis(r.t),
            price: Price(MID),
            qty: Qty(1),
        });
        r.market(&book);
        r
    }

    fn now(&self) -> UtcMillis {
        UtcMillis(self.t)
    }

    fn drain(&mut self) {
        for e in self.core.drain() {
            black_box(&e);
            if !matches!(e, Event::Account { .. }) {
                self.events += 1;
            }
        }
    }

    fn cmd(&mut self, c: Command) -> Option<lines_trading::OrderId> {
        let id = self.core.submit(self.now(), Source::Test, Instant::now(), c);
        let mut placed = None;
        for e in self.core.drain() {
            if let Event::Command { id: i, result, .. } = &e
                && *i == id
            {
                placed = result.as_ref().ok().and_then(|v| v.first().copied());
            }
            self.events += 1;
        }
        placed
    }

    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.drain();
    }

    fn market(&mut self, inputs: &[MarketInput]) {
        self.t += 1;
        self.core.market(self.now(), es(), inputs);
        self.drain();
    }

    fn place(&mut self, side: Side, qty: i64, kind: OrderKind) -> Option<lines_trading::OrderId> {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side,
            qty: Qty(qty),
            kind,
        }))
    }
}

const ROUND_TRIPS: u64 = 100;
const MARKET_BATCHES: u64 = 1_000;
const MARKET_ORDERS: u64 = 100;

fn core_round_trip(c: &mut Criterion) {
    let mut g = c.benchmark_group("core");
    g.throughput(Throughput::Elements(ROUND_TRIPS));
    g.bench_function("place_ack_cancel", |b| {
        b.iter_batched_ref(
            Rig::new,
            |r| {
                for k in 0..ROUND_TRIPS as i64 {
                    let id = r
                        .place(
                            Side::Buy,
                            1,
                            OrderKind::Limit {
                                price: Price(MID - (10 + k % 5) * TICK),
                            },
                        )
                        .expect("accepted");
                    r.wait(LAT + 1);
                    r.cmd(Command::CancelOrder {
                        account: acct(),
                        order: id,
                    });
                    r.wait(LAT + 1);
                }
            },
            BatchSize::LargeInput,
        )
    });
    g.finish();
}

fn paper_matching(c: &mut Criterion) {
    let mut g = c.benchmark_group("paper");
    g.throughput(Throughput::Elements(MARKET_BATCHES));
    g.bench_function("market_with_resting", |b| {
        b.iter_batched_ref(
            || {
                let mut r = Rig::new();
                for k in 1..=10 {
                    r.place(Side::Buy, 1, OrderKind::Limit { price: Price(MID - k * TICK) });
                    r.place(Side::Sell, 1, OrderKind::Limit { price: Price(MID + k * TICK) });
                }
                r.wait(LAT + 1);
                r
            },
            |r| {
                // A walk from 12 ticks below to 12 above and back: fills on both sides.
                let mut p = MID;
                let mut step = TICK;
                for _ in 0..MARKET_BATCHES {
                    p += step;
                    if (p - MID).abs() >= 12 * TICK {
                        step = -step;
                    }
                    let t = UtcMillis(r.t + 1);
                    r.market(&[MarketInput::Trade {
                        time: t,
                        price: Price(p),
                        qty: Qty(2),
                    }]);
                }
            },
            BatchSize::LargeInput,
        )
    });
    g.throughput(Throughput::Elements(MARKET_ORDERS));
    g.bench_function("market_order_book_walk", |b| {
        b.iter_batched_ref(
            Rig::new,
            |r| {
                for k in 0..MARKET_ORDERS {
                    let side = if k % 2 == 0 { Side::Buy } else { Side::Sell };
                    r.place(side, 75, OrderKind::Market);
                    r.wait(LAT + 1);
                }
            },
            BatchSize::LargeInput,
        )
    });
    g.finish();
}

criterion_group!(benches, core_round_trip, paper_matching);
criterion_main!(benches);
