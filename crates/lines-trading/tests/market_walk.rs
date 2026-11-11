//! A market order, Flatten or triggered stop on a thin book (a top-of-book feed: one level a side)
//! walks the visible levels, then goes on past the deepest with modelled slippage
//! (`paper::SLIPPAGE_TICKS`, issue #94): it fills its whole quantity at once, never better than
//! the book. An option's market order is different (decision 0028): a missing side never fills, so
//! it works on until a quote has that side. Limit orders are unchanged.

use std::time::Instant;

use lines_core::{CivilDate, ContractKey, Month, Price, Qty, Right, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::trigger::{TriggerKind, TriggerSpec};
use lines_trading::{
    AccountConfig, AccountId, Command, Event, MarketInput, Money, OrderId, OrderKind, OrderSpec,
    OrderState, PaperBroker, PaperConfig, Side, Source, TradingCore, paper::SLIPPAGE_TICKS,
};

const LAT: i64 = 20;
const CME_OPEN: &str = "2026-11-10T15:00:00Z";
const RTH: &str = "2026-11-09T15:00:00Z";

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}

fn call() -> ContractKey {
    ContractKey::option(
        catalog::SPY,
        CivilDate::new(2026, 11, 9).unwrap(),
        Price(68_400),
        Right::Call,
    )
}

fn friday() -> ContractKey {
    ContractKey::option(
        catalog::SPY,
        CivilDate::new(2026, 11, 13).unwrap(),
        Price(68_400),
        Right::Call,
    )
}

fn spy() -> ContractKey {
    ContractKey::equity(catalog::SPY)
}

fn acct() -> AccountId {
    AccountId::new("paper-1")
}

/// ES price units: 6000.25 → 600025.
fn px(p: f64) -> Price {
    Price((p * 100.0).round() as i64)
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: Vec<Event>,
}

impl Rig {
    fn new(at: &str) -> Rig {
        let mut b = PaperBroker::new(PaperConfig {
            latency_ms: LAT,
            ..Default::default()
        });
        b.open_account(acct(), Money::whole(100_000));
        let mut core = TradingCore::new(Box::new(b));
        core.add_account(AccountConfig::paper("paper-1", 100_000), None);
        Rig {
            core,
            t: UtcMillis::parse_iso(at).unwrap().0,
            events: Vec::new(),
        }
    }

    fn now(&self) -> UtcMillis {
        UtcMillis(self.t)
    }

    fn collect(&mut self) {
        self.events.extend(self.core.drain());
    }

    fn cmd(&mut self, c: Command) -> Result<Vec<OrderId>, lines_trading::Reject> {
        let id = self.core.submit(self.now(), Source::Dom, Instant::now(), c);
        self.collect();
        self.events
            .iter()
            .find_map(|e| match e {
                Event::Command { id: i, result, .. } if *i == id => Some(result.clone()),
                _ => None,
            })
            .expect("answered")
    }

    fn wait(&mut self, ms: i64) {
        self.t += ms;
        self.core.tick(self.now());
        self.collect();
    }

    fn market(&mut self, c: ContractKey, inputs: &[MarketInput]) {
        self.t += 1;
        self.core.market(self.now(), c, inputs);
        self.collect();
    }

    /// A top-of-book quote: one level a side.
    fn top(&mut self, bid: (f64, i64), ask: (f64, i64)) {
        self.market(
            es(),
            &[
                MarketInput::BookClear,
                MarketInput::Book {
                    side: BookSide::Bid,
                    price: px(bid.0),
                    qty: Qty(bid.1),
                },
                MarketInput::Book {
                    side: BookSide::Ask,
                    price: px(ask.0),
                    qty: Qty(ask.1),
                },
            ],
        );
    }

    fn trade(&mut self, p: f64, q: i64) {
        let time = UtcMillis(self.t + 1);
        self.market(
            es(),
            &[MarketInput::Trade {
                time,
                price: px(p),
                qty: Qty(q),
            }],
        );
    }

    fn order(&mut self, side: Side, qty: i64, kind: OrderKind) -> OrderId {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side,
            qty: Qty(qty),
            kind,
        }))
        .unwrap()[0]
    }

    fn pos(&self, c: ContractKey) -> i64 {
        self.core.position(&acct(), c)
    }

    fn state(&self, id: OrderId) -> OrderState {
        self.core.order(id).unwrap().state.clone()
    }

    fn remaining(&self, id: OrderId) -> i64 {
        self.core.order(id).unwrap().remaining().0
    }

    fn fills(&self, id: OrderId) -> Vec<(Price, i64)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Fill(f) if f.order == id => Some((f.price, f.qty.0)),
                _ => None,
            })
            .collect()
    }

    fn flatten(&mut self, c: ContractKey) -> OrderId {
        *self
            .cmd(Command::Flatten {
                account: acct(),
                contract: Some(c),
            })
            .unwrap()
            .last()
            .unwrap()
    }
}

/// A stop filled at the print (no bid to take) takes its share of the print first: a print of 3
/// that fires a stop for 2 leaves 1 for a resting limit. Several orders never fill more than printed.
#[test]
fn a_stop_filled_at_the_print_takes_its_share_first() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 10), (6000.25, 10));
    r.order(Side::Buy, 5, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.pos(es()), 5);
    let stop = r
        .cmd(Command::PlaceExit(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(2),
            kind: OrderKind::Stop {
                trigger: px(5999.00),
            },
        }))
        .unwrap()[0];
    r.wait(LAT);
    // No bid: the stop fills at the print, and the limit rests below it.
    r.market(
        es(),
        &[
            MarketInput::BookClear,
            MarketInput::Book {
                side: BookSide::Ask,
                price: px(6000.25),
                qty: Qty(10),
            },
        ],
    );
    let limit = r.order(Side::Sell, 2, OrderKind::Limit { price: px(5998.00) });
    r.wait(LAT);
    assert!(r.fills(limit).is_empty());
    r.trade(5999.00, 3);
    let sum = |f: Vec<(Price, i64)>| f.iter().map(|x| x.1).sum::<i64>();
    assert_eq!(sum(r.fills(stop)), 2);
    assert_eq!(sum(r.fills(limit)), 1, "what the stop left of the print");
}

/// A limit is as before: it takes what it crosses at its limit or better, and rests at its limit.
#[test]
fn a_limit_order_is_unchanged() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 3), (6000.25, 1));
    let id = r.order(Side::Buy, 3, OrderKind::Limit { price: px(6000.25) });
    r.wait(LAT);
    assert_eq!(r.fills(id), [(px(6000.25), 1)]);
    // A higher ask is past its limit: nothing.
    r.top((6000.25, 3), (6000.50, 5));
    assert_eq!(r.fills(id), [(px(6000.25), 1)]);
    assert_eq!(r.remaining(id), 2);
}

/// A fired option Stop whose bid is gone by the time its market order arrives: the order works on,
/// visible, and the bid's return closes the position at the bid.
#[test]
fn a_fired_option_stop_with_no_bid_sells_when_the_bid_returns() {
    let mut r = Rig::new(RTH);
    let quote = |r: &mut Rig, bid: Option<i64>, ask: Option<i64>| {
        let time = UtcMillis(r.t + 1);
        r.market(
            call(),
            &[MarketInput::OptionQuote {
                bid: bid.map(Price),
                ask: ask.map(Price),
                last: None,
                time,
            }],
        );
    };
    let print = |r: &mut Rig, cents: i64| {
        let time = UtcMillis(r.t + 1);
        r.market(
            spy(),
            &[MarketInput::Trade {
                time,
                price: Price(cents),
                qty: Qty(1),
            }],
        );
    };
    print(&mut r, 68_300);
    quote(&mut r, Some(240), Some(242));
    r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: call(),
        side: Side::Buy,
        qty: Qty(1),
        kind: OrderKind::Limit { price: Price(242) },
    }))
    .unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.pos(call()), 1);
    r.cmd(Command::ArmTrigger(TriggerSpec {
        account: acct(),
        contract: call(),
        kind: TriggerKind::Stop,
        level: Price(68_200),
    }))
    .unwrap();
    // The level breaks while the bid is there: the Stop sends its market order. Before it
    // arrives the bid is gone.
    print(&mut r, 68_100);
    quote(&mut r, None, Some(242));
    r.wait(LAT + 1);
    assert_eq!(r.pos(call()), 1, "no bid to sell to");
    let market = r
        .core
        .open_orders(&acct())
        .find(|o| o.contract == call() && matches!(o.kind, OrderKind::Market))
        .map(|o| (o.id, o.state.clone()))
        .expect("the Stop's order works on, visible");
    assert_eq!(market.1, OrderState::Working);
    quote(&mut r, Some(150), Some(152));
    r.wait(LAT + 1);
    assert_eq!(r.pos(call()), 0);
    assert_eq!(r.fills(market.0), [(Price(150), 1)]);
    assert_eq!(r.core.triggers().count(), 0);
}

/// An option's Flatten with no bid ends at the regular session's close, never filling on the
/// next morning's first quote.
#[test]
fn an_option_flatten_ends_at_the_close() {
    // A series expiring later in the week: the close ends the order, not the series.
    let mut r = Rig::new(RTH);
    let quote = |r: &mut Rig, bid: Option<i64>, ask: Option<i64>| {
        let time = UtcMillis(r.t + 1);
        r.market(
            friday(),
            &[MarketInput::OptionQuote {
                bid: bid.map(Price),
                ask: ask.map(Price),
                last: None,
                time,
            }],
        );
    };
    let time = UtcMillis(r.t + 1);
    r.market(
        spy(),
        &[MarketInput::Trade {
            time,
            price: Price(68_300),
            qty: Qty(1),
        }],
    );
    quote(&mut r, Some(240), Some(242));
    r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: friday(),
        side: Side::Buy,
        qty: Qty(1),
        kind: OrderKind::Limit { price: Price(242) },
    }))
    .unwrap();
    r.wait(LAT + 1);
    r.t = UtcMillis::parse_iso("2026-11-09T20:59:00Z").unwrap().0;
    quote(&mut r, None, Some(242));
    let flat = r.flatten(friday());
    r.wait(LAT + 1);
    assert_eq!(r.state(flat), OrderState::Working);
    // 16:00 New York: the close.
    r.t = UtcMillis::parse_iso("2026-11-09T21:00:01Z").unwrap().0;
    r.wait(1);
    assert_eq!(r.state(flat), OrderState::Cancelled);
    assert_eq!(r.pos(friday()), 1);
}

/// No book on its side and no trade yet: nothing would ever show a price, so the market order
/// ends at once rather than working on forever.
#[test]
fn a_market_order_with_no_price_at_all_is_cancelled() {
    let mut r = Rig::new(CME_OPEN);
    let id = r.order(Side::Buy, 1, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.state(id), OrderState::Cancelled);
    assert_eq!(r.pos(es()), 0);
}

/// ES prices `n` ticks (0.25) from `p`.
fn ticks(p: f64, n: i64) -> Price {
    Price(px(p).0 + 25 * n)
}

fn total(f: &[(Price, i64)]) -> i64 {
    f.iter().map(|x| x.1).sum()
}

/// Long `qty` with a stop exit for all of it at 5999.00, from a deep book.
fn long_with_stop(r: &mut Rig, qty: i64) -> OrderId {
    r.top((6000.00, 10), (6000.25, 10));
    r.order(Side::Buy, qty, OrderKind::Market);
    r.wait(LAT);
    let stop = r
        .cmd(Command::PlaceExit(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(qty),
            kind: OrderKind::Stop {
                trigger: px(5999.00),
            },
        }))
        .unwrap()[0];
    r.wait(LAT);
    stop
}

/// A 2-lot buy on a one-level book with 1 at the ask: the ask's 1, then 1 a tick above. Filled at
/// once, never left working.
#[test]
fn a_two_lot_on_a_one_lot_inside_fills_whole_a_tick_up() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 3), (6000.25, 1));
    let id = r.order(Side::Buy, 2, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.fills(id), [(px(6000.25), 1), (px(6000.50), 1)]);
    assert_eq!(r.state(id), OrderState::Filled);
    assert_eq!(r.pos(es()), 2);
}

/// A 5-lot buy on a one-level book: the inside's 2, then the thinnest visible size (2) a tick at a
/// time past it.
#[test]
fn a_five_lot_on_a_one_level_book_walks_past_it() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 3), (6000.25, 2));
    let id = r.order(Side::Buy, 5, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(
        r.fills(id),
        [(px(6000.25), 2), (px(6000.50), 2), (px(6000.75), 1)]
    );
    assert_eq!(r.state(id), OrderState::Filled);
    assert_eq!(r.pos(es()), 5);
}

/// A 5-lot sell into a 1-lot bid: one a tick, down from the bid.
#[test]
fn a_five_lot_sell_into_a_one_lot_bid_slips_a_tick_a_lot() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 1), (6000.25, 3));
    let id = r.order(Side::Sell, 5, OrderKind::Market);
    r.wait(LAT);
    let want: Vec<(Price, i64)> = (0..5).map(|n| (ticks(6000.00, -n), 1)).collect();
    assert_eq!(r.fills(id), want);
    assert_eq!(r.pos(es()), -5);
}

/// A deeper book: every visible level first, then the model from the deepest, a tick at a time
/// with the thinnest level's size (1 here, not the 4 at the inside).
#[test]
fn the_visible_levels_go_first_then_the_model() {
    let mut r = Rig::new(CME_OPEN);
    r.market(
        es(),
        &[
            MarketInput::BookClear,
            MarketInput::Book {
                side: BookSide::Ask,
                price: px(6000.25),
                qty: Qty(4),
            },
            MarketInput::Book {
                side: BookSide::Ask,
                price: px(6000.50),
                qty: Qty(1),
            },
        ],
    );
    let id = r.order(Side::Buy, 7, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(
        r.fills(id),
        [
            (px(6000.25), 4),
            (px(6000.50), 1),
            (px(6000.75), 1),
            (px(6001.00), 1)
        ]
    );
}

/// The model's end: past `SLIPPAGE_TICKS` ticks whatever is left fills at the last of them. The
/// whole quantity, never more, and never a better price as it goes.
#[test]
fn a_large_order_fills_the_rest_at_the_last_modelled_tick() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 1), (6000.25, 1));
    let id = r.order(Side::Buy, 100, OrderKind::Market);
    r.wait(LAT);
    let f = r.fills(id);
    assert_eq!(total(&f), 100);
    assert_eq!(f.len() as i64, SLIPPAGE_TICKS + 1);
    assert_eq!(f[0], (px(6000.25), 1));
    let last = f.last().unwrap();
    assert_eq!(last.0, ticks(6000.25, SLIPPAGE_TICKS));
    assert_eq!(last.1, 100 - SLIPPAGE_TICKS);
    assert!(f.windows(2).all(|w| w[1].0 > w[0].0));
    assert_eq!(r.pos(es()), 100);
}

/// Liquidity a market order took isn't there for the next one: it slips past the taken inside at
/// once instead of waiting for the level to grow.
#[test]
fn the_next_order_slips_past_what_was_taken() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 3), (6000.25, 2));
    r.order(Side::Buy, 2, OrderKind::Market);
    r.wait(LAT);
    // The same inside quoted again is the same 2, already taken.
    r.top((6000.00, 3), (6000.25, 2));
    let id = r.order(Side::Buy, 1, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.fills(id), [(px(6000.50), 1)]);
    assert_eq!(r.state(id), OrderState::Filled);
}

/// Flatten on a one-level book: long 5, the bid has 2. One press closes it all, at the bid and
/// below; nothing works on, and nothing sells past flat.
#[test]
fn flatten_on_a_one_level_book_closes_the_five_lot_at_once() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 2), (6000.25, 10));
    r.order(Side::Buy, 5, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.pos(es()), 5);
    r.top((6000.00, 2), (6000.25, 10));
    let flat = r.flatten(es());
    r.wait(LAT);
    assert_eq!(r.pos(es()), 0);
    assert_eq!(r.state(flat), OrderState::Filled);
    assert_eq!(
        r.fills(flat),
        [(px(6000.00), 2), (px(5999.75), 2), (px(5999.50), 1)]
    );
    assert_eq!(r.core.open_orders(&acct()).count(), 0, "no remainder");
    r.top((6000.00, 2), (6000.25, 10));
    assert_eq!(r.pos(es()), 0, "never past flat");
}

/// Flatten of a 2-lot on a 1-lot bid.
#[test]
fn flatten_on_a_one_lot_bid_closes_the_two_lot() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 1), (6000.25, 10));
    r.order(Side::Buy, 2, OrderKind::Market);
    r.wait(LAT);
    r.top((6000.00, 1), (6000.25, 10));
    let flat = r.flatten(es());
    r.wait(LAT);
    assert_eq!(r.fills(flat), [(px(6000.00), 1), (px(5999.75), 1)]);
    assert_eq!(r.pos(es()), 0);
}

/// Flatten with the position's stop exit working (a broker honoring reduce-only sends the market
/// order first): it still closes all 5 at once, and the exit ends with the position.
#[test]
fn flatten_with_an_exit_working_closes_it_all() {
    let mut r = Rig::new(CME_OPEN);
    let stop = long_with_stop(&mut r, 5);
    r.top((6000.00, 1), (6000.25, 10));
    let flat = r.flatten(es());
    r.wait(LAT);
    r.wait(LAT);
    assert_eq!(total(&r.fills(flat)), 5);
    assert_eq!(r.pos(es()), 0);
    assert!(
        !r.core.order(stop).unwrap().is_open(),
        "nothing left to protect"
    );
    r.trade(5999.00, 10);
    assert_eq!(r.pos(es()), 0, "never short");
}

/// Flatten of a short on a one-level ask: buys it all back at once, at the ask and above.
#[test]
fn flatten_of_a_short_buys_it_all_back() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 10), (6000.25, 1));
    r.order(Side::Sell, 5, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.pos(es()), -5);
    r.top((6000.00, 10), (6000.25, 1));
    let flat = r.flatten(es());
    r.wait(LAT);
    let want: Vec<(Price, i64)> = (0..5).map(|n| (ticks(6000.25, n), 1)).collect();
    assert_eq!(r.fills(flat), want);
    assert_eq!(r.pos(es()), 0);
}

/// A sell larger than the position (a reversal on purpose) fills as asked.
#[test]
fn a_market_order_past_the_position_keeps_its_size() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 2), (6000.25, 10));
    r.order(Side::Buy, 1, OrderKind::Market);
    r.wait(LAT);
    let sell = r.order(Side::Sell, 4, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.fills(sell), [(px(6000.00), 2), (px(5999.75), 2)]);
    assert_eq!(r.pos(es()), -3);
}

/// A 5-lot stop on a one-level bid shows slippage: it takes the bid, then goes on past it, instead
/// of filling the rest at the trigger print.
#[test]
fn a_five_lot_stop_slips_past_a_thin_bid() {
    let mut r = Rig::new(CME_OPEN);
    let stop = long_with_stop(&mut r, 5);
    r.top((5998.75, 1), (5999.00, 10));
    r.trade(5999.00, 1);
    let want: Vec<(Price, i64)> = (0..5).map(|n| (ticks(5998.75, -n), 1)).collect();
    assert_eq!(r.fills(stop), want);
    assert!(
        r.fills(stop).iter().all(|f| f.0 < px(5999.00)),
        "no fill at the trigger print"
    );
    assert_eq!(r.pos(es()), 0);
}

/// A 2-lot stop on a 1-lot bid: the bid, then a tick below.
#[test]
fn a_two_lot_stop_on_a_one_lot_bid() {
    let mut r = Rig::new(CME_OPEN);
    let stop = long_with_stop(&mut r, 2);
    r.top((5998.75, 1), (5999.00, 10));
    r.trade(5999.00, 1);
    assert_eq!(r.fills(stop), [(px(5998.75), 1), (px(5998.50), 1)]);
    assert_eq!(r.pos(es()), 0);
}

/// A marketable limit never slips past its limit: what the book can't fill rests.
#[test]
fn a_marketable_limit_never_slips_past_its_limit() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 3), (6000.25, 1));
    let id = r.order(Side::Buy, 3, OrderKind::Limit { price: px(6000.50) });
    r.wait(LAT);
    assert_eq!(r.fills(id), [(px(6000.25), 1)]);
    assert_eq!(r.remaining(id), 2, "rests at its limit");
}

/// Ten 1-lot buys in a row (no new quote between them) on a 1-lot ask climb a tick each, as one
/// 10-lot would: modelled ticks taken are remembered within the batch.
#[test]
fn ten_one_lots_in_a_row_climb_like_one_ten_lot() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 3), (6000.25, 1));
    let ids: Vec<OrderId> = (0..10)
        .map(|_| r.order(Side::Buy, 1, OrderKind::Market))
        .collect();
    r.wait(LAT);
    let prices: Vec<Price> = ids
        .iter()
        .flat_map(|id| r.fills(*id))
        .map(|f| f.0)
        .collect();
    let want: Vec<Price> = (0..10).map(|n| ticks(6000.25, n)).collect();
    assert_eq!(prices, want);
    assert_eq!(r.pos(es()), 10);
}

/// An option's Flatten with no bid (option market orders are otherwise refused): it works on
/// (decision 0028), reduce-only so it can never reverse, and the person can cancel it.
#[test]
fn an_option_remainder_closes_reduce_only_and_can_be_cancelled() {
    let mut r = Rig::new(RTH);
    let quote = |r: &mut Rig, bid: Option<i64>, ask: Option<i64>| {
        let time = UtcMillis(r.t + 1);
        r.market(
            call(),
            &[MarketInput::OptionQuote {
                bid: bid.map(Price),
                ask: ask.map(Price),
                last: None,
                time,
            }],
        );
    };
    let time = UtcMillis(r.t + 1);
    r.market(
        spy(),
        &[MarketInput::Trade {
            time,
            price: Price(68_300),
            qty: Qty(1),
        }],
    );
    quote(&mut r, Some(240), Some(242));
    r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: call(),
        side: Side::Buy,
        qty: Qty(1),
        kind: OrderKind::Limit { price: Price(242) },
    }))
    .unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.pos(call()), 1);
    quote(&mut r, None, Some(242));
    let sell = r.flatten(call());
    r.wait(LAT + 1);
    assert_eq!(r.state(sell), OrderState::Working);
    assert!(r.core.order(sell).unwrap().reduce_only, "it only closes");
    r.cmd(Command::CancelOrder {
        account: acct(),
        order: sell,
    })
    .unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.state(sell), OrderState::Cancelled);
    quote(&mut r, Some(150), Some(152));
    assert_eq!(r.pos(call()), 1, "nothing after the cancel");
}

/// A paper broker restored with a future's market buy of 5 that has filled 2, as an earlier
/// version (which let a remainder work on) could have saved it.
fn saved_remainder(at: &str) -> (PaperBroker, UtcMillis) {
    use lines_trading::paper::PaperOrder;
    use lines_trading::{Order, Role};
    let now = UtcMillis::parse_iso(at).unwrap();
    let mut b = PaperBroker::new(PaperConfig::default());
    b.open_account(acct(), Money::whole(100_000));
    let mut state = b.state();
    state.orders.push(PaperOrder {
        order: Order {
            id: OrderId(7),
            account: acct(),
            contract: es(),
            side: Side::Buy,
            qty: Qty(5),
            kind: OrderKind::Market,
            state: OrderState::PartiallyFilled,
            filled: Qty(2),
            fill_value: 2 * i128::from(px(6000.00).0),
            role: Role::Entry,
            parent: None,
            oco: None,
            triggered: false,
            created: now,
            updated: now,
            reduce_only: false,
            overfill: Qty(0),
        },
        queue_ahead: 0,
        held: false,
    });
    (PaperBroker::from_state(PaperConfig::default(), state), now)
}

fn feed(b: &mut PaperBroker, now: UtcMillis, inputs: &[MarketInput]) {
    use lines_trading::Broker;
    for i in inputs {
        b.market(now, es(), i);
    }
}

fn book(bid: (f64, i64), ask: (f64, i64)) -> [MarketInput; 3] {
    [
        MarketInput::BookClear,
        MarketInput::Book {
            side: BookSide::Bid,
            price: px(bid.0),
            qty: Qty(bid.1),
        },
        MarketInput::Book {
            side: BookSide::Ask,
            price: px(ask.0),
            qty: Qty(ask.1),
        },
    ]
}

fn print(p: f64, q: i64) -> MarketInput {
    MarketInput::Trade {
        time: UtcMillis(0),
        price: px(p),
        qty: Qty(q),
    }
}

/// The saved order's fills and whether it was cancelled, from the broker's events so far.
fn saved_events(b: &mut PaperBroker, now: UtcMillis) -> (Vec<(Price, i64)>, bool) {
    use lines_trading::{Broker, BrokerEvent};
    let mut out = Vec::new();
    b.poll(now, &mut out);
    let fills = out
        .iter()
        .filter_map(|e| match e {
            BrokerEvent::Fill { id, price, qty, .. } if *id == OrderId(7) => Some((*price, qty.0)),
            _ => None,
        })
        .collect();
    let cancelled = out
        .iter()
        .any(|e| matches!(e, BrokerEvent::Cancelled { id, .. } if *id == OrderId(7)));
    (fills, cancelled)
}

/// A saved remainder fills whole on the next book batch, past a thin ask with the model; never
/// left open.
#[test]
fn a_saved_future_remainder_fills_whole_on_the_next_book() {
    let (mut b, now) = saved_remainder(CME_OPEN);
    feed(&mut b, now, &book((6000.00, 3), (6000.25, 1)));
    let (fills, _) = saved_events(&mut b, now);
    assert_eq!(
        fills,
        [(px(6000.25), 1), (px(6000.50), 1), (px(6000.75), 1)]
    );
}

/// Before its batch is walked, a print takes the saved remainder first, at the print and up to
/// what it printed, but only at or beyond the inside on its side: a print at the bid never fills
/// a buy. With no inside at all, a print fills nothing.
#[test]
fn a_print_fills_a_saved_remainder_only_at_or_beyond_the_ask() {
    let (mut b, now) = saved_remainder(CME_OPEN);
    feed(&mut b, now, &[print(6000.00, 9)]);
    feed(&mut b, now, &book((6000.00, 3), (6000.25, 1)));
    feed(&mut b, now, &[print(6000.00, 9), print(6000.25, 2)]);
    let (fills, _) = saved_events(&mut b, now);
    assert_eq!(fills[0], (px(6000.25), 2), "the print at the ask, first");
    assert_eq!(fills.iter().map(|f| f.1).sum::<i64>(), 3, "never more");
}

/// The session's close ends a saved remainder: nothing fills at the reopen.
#[test]
fn the_session_close_ends_a_saved_remainder() {
    let (mut b, _) = saved_remainder("2026-11-10T21:59:00Z");
    let reopen = UtcMillis::parse_iso("2026-11-10T23:00:00Z").unwrap();
    feed(&mut b, reopen, &book((6010.00, 3), (6010.25, 50)));
    let (fills, cancelled) = saved_events(&mut b, reopen);
    assert!(fills.is_empty());
    assert!(cancelled);
}

/// A stop fired by a print that comes before its batch's book update: the stale bid above the
/// print isn't taken, and no fill is better than the print.
#[test]
fn a_stop_fired_before_the_book_catches_up_never_beats_the_print() {
    let mut r = Rig::new(CME_OPEN);
    let stop = long_with_stop(&mut r, 5);
    r.top((6000.00, 1), (6000.25, 10));
    let time = UtcMillis(r.t + 1);
    r.market(
        es(),
        &[
            MarketInput::Trade {
                time,
                price: px(5999.00),
                qty: Qty(1),
            },
            MarketInput::BookClear,
            MarketInput::Book {
                side: BookSide::Bid,
                price: px(5998.75),
                qty: Qty(1),
            },
        ],
    );
    let want: Vec<(Price, i64)> = (0..5).map(|n| (ticks(5999.00, -n), 1)).collect();
    assert_eq!(r.fills(stop), want);
    assert_eq!(r.pos(es()), 0);
}

/// The slippage cap per product: 20 ticks for futures and equities; for spot and perpetual
/// products 10 bp of the reference price in whole ticks, rounded up, never under 20.
#[test]
fn the_slippage_cap_is_ten_basis_points_on_crypto() {
    use lines_trading::paper::slippage_ticks;
    // ES (a quarter-point tick, 25 units): 20 ticks whatever the price.
    assert_eq!(slippage_ticks(false, Some(px(6000.25)), 25), SLIPPAGE_TICKS);
    // BTC/USD.KR at 70,000.0 (a 0.1 tick: one unit): 10 bp is $70, 700 ticks.
    assert_eq!(slippage_ticks(true, Some(Price(700_000)), 1), 700);
    // Rounded up to whole ticks.
    assert_eq!(slippage_ticks(true, Some(Price(700_001)), 1), 701);
    // Never under 20 ticks: a cheap product, or no reference price yet.
    assert_eq!(slippage_ticks(true, Some(Price(1_000)), 1), SLIPPAGE_TICKS);
    assert_eq!(slippage_ticks(true, None, 1), SLIPPAGE_TICKS);
}

/// A BTC/USD.KR market buy past a one-unit ask walks 700 ticks (10 bp of 70,000.0), the rest at
/// the last of them: the whole quantity, never more, never a better price as it goes.
#[test]
fn a_crypto_market_order_slips_ten_basis_points() {
    let btc = ContractKey::spot(catalog::BTC_USD_KR);
    let mut r = Rig::new(CME_OPEN);
    r.market(
        btc,
        &[
            MarketInput::BookClear,
            MarketInput::Book {
                side: BookSide::Bid,
                price: Price(699_990),
                qty: Qty(5),
            },
            MarketInput::Book {
                side: BookSide::Ask,
                price: Price(700_000),
                qty: Qty(1),
            },
        ],
    );
    let id = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: btc,
            side: Side::Buy,
            qty: Qty(1_000),
            kind: OrderKind::Market,
        }))
        .unwrap()[0];
    r.wait(LAT);
    let f = r.fills(id);
    assert_eq!(f.iter().map(|x| x.1).sum::<i64>(), 1_000);
    assert_eq!(f.first(), Some(&(Price(700_000), 1)));
    assert_eq!(
        f.last(),
        Some(&(Price(700_700), 1_000 - 700)),
        "the 700th tick: $70 up"
    );
    assert_eq!(f.len(), 701);
    assert!(f.windows(2).all(|w| w[1].0 > w[0].0));
    assert_eq!(r.pos(btc), 1_000);
}
