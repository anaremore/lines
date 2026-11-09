//! A market order on a one-level book (a top-of-book feed): what the inside couldn't fill works
//! on, visible, and takes later quotes and trades at their prices until done or cancelled
//! (decision 0005: market orders walk the book). Limit orders are unchanged.

use std::time::Instant;

use lines_core::{CivilDate, ContractKey, Month, Price, Qty, Right, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::trigger::{TriggerKind, TriggerSpec};
use lines_trading::{
    AccountConfig, AccountId, Command, Event, MarketInput, Money, OrderId, OrderKind, OrderSpec,
    OrderState, PaperBroker, PaperConfig, Side, Source, TradingCore,
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

/// The inside has 2; a buy of 5 takes them and the rest works on, visible, then takes the next
/// quote's ask and a trade, each at its own price.
#[test]
fn the_rest_works_on_and_fills_on_later_quotes_and_trades() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 3), (6000.25, 2));
    let id = r.order(Side::Buy, 5, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.fills(id), [(px(6000.25), 2)]);
    assert_eq!(
        r.state(id),
        OrderState::PartiallyFilled,
        "working, not cancelled"
    );
    assert_eq!(r.remaining(id), 3, "the rest is visible");
    // The ask moves up a tick with 2 more.
    r.top((6000.25, 3), (6000.50, 2));
    assert_eq!(r.fills(id), [(px(6000.25), 2), (px(6000.50), 2)]);
    // A trade prints at 6000.75: the last one goes there.
    r.trade(6000.75, 4);
    assert_eq!(
        r.fills(id),
        [(px(6000.25), 2), (px(6000.50), 2), (px(6000.75), 1)]
    );
    assert_eq!(r.state(id), OrderState::Filled);
    assert_eq!(r.pos(es()), 5);
}

/// Nothing left to take yet: the order is Working, and what the ask grows by fills it.
#[test]
fn nothing_to_take_yet_shows_as_working() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 3), (6000.25, 2));
    r.order(Side::Buy, 2, OrderKind::Market);
    r.wait(LAT);
    let id = r.order(Side::Buy, 1, OrderKind::Market);
    r.wait(LAT);
    assert!(r.fills(id).is_empty(), "the 2 were taken");
    assert_eq!(r.state(id), OrderState::Working);
    r.top((6000.00, 3), (6000.25, 3));
    assert_eq!(r.fills(id), [(px(6000.25), 1)]);
}

/// A top-of-book feed re-sends the same inside with every quote: that's the liquidity already
/// taken, not more (decision 0005: paper errs pessimistic). Only what the level grows by, or a
/// new price, is new.
#[test]
fn the_same_inside_quoted_again_is_not_new_liquidity() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 3), (6000.25, 2));
    let id = r.order(Side::Buy, 5, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.fills(id), [(px(6000.25), 2)]);
    for _ in 0..5 {
        r.top((6000.00, 3), (6000.25, 2));
    }
    assert_eq!(r.remaining(id), 3, "the same 2, shown again");
    r.top((6000.00, 3), (6000.25, 3));
    assert_eq!(r.fills(id), [(px(6000.25), 2), (px(6000.25), 1)]);
    // The level goes and comes back: new liquidity.
    r.top((6000.25, 3), (6000.50, 1));
    r.top((6000.00, 3), (6000.25, 1));
    assert_eq!(
        r.fills(id),
        [
            (px(6000.25), 2),
            (px(6000.25), 1),
            (px(6000.50), 1),
            (px(6000.25), 1)
        ]
    );
    assert_eq!(r.state(id), OrderState::Filled);
}

/// A plain market sell closing a long works on reduce-only: a stop firing on a print before the
/// bid comes back closes the rest, and the same print never fills the sell past flat.
#[test]
fn a_closing_market_order_never_reverses_the_position() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 2), (6000.25, 10));
    r.order(Side::Buy, 5, OrderKind::Market);
    r.wait(LAT);
    let stop = r
        .cmd(Command::PlaceExit(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(5),
            kind: OrderKind::Stop {
                trigger: px(5999.00),
            },
        }))
        .unwrap()[0];
    r.wait(LAT);
    let sell = r.order(Side::Sell, 5, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.fills(sell), [(px(6000.00), 2)]);
    assert_eq!(r.pos(es()), 3);
    let o = r.core.order(sell).unwrap();
    assert!(o.reduce_only, "the rest only closes");
    assert!(o.is_open());
    r.trade(5999.00, 10);
    assert_eq!(r.fills(stop), [(px(5999.00), 3)]);
    assert_eq!(r.pos(es()), 0, "never short");
    assert!(!r.core.order(sell).unwrap().is_open());
}

/// A sell larger than the position (a reversal on purpose) stays as asked: its rest isn't capped.
#[test]
fn a_market_order_past_the_position_keeps_its_size() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 2), (6000.25, 10));
    r.order(Side::Buy, 1, OrderKind::Market);
    r.wait(LAT);
    let sell = r.order(Side::Sell, 4, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.remaining(sell), 2);
    assert!(!r.core.order(sell).unwrap().reduce_only);
    r.top((5999.75, 2), (6000.00, 10));
    assert_eq!(r.pos(es()), -3);
}

/// With no inside on its side (a snapshot cleared, its bids not in yet), a print says nothing
/// about what a market order would pay: the rest waits.
#[test]
fn a_print_with_no_inside_fills_no_rest() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 3), (6000.25, 1));
    let id = r.order(Side::Buy, 3, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.remaining(id), 2);
    r.market(es(), &[MarketInput::BookClear]);
    r.trade(6000.00, 5);
    assert_eq!(
        r.remaining(id),
        2,
        "a print at the old bid isn't a buyer's price"
    );
}

/// A book cleared for a snapshot never sends the rest to the last trade's price.
#[test]
fn a_cleared_book_fills_nothing_at_the_last_price() {
    let mut r = Rig::new(CME_OPEN);
    r.trade(6000.00, 1);
    r.top((6000.00, 3), (6000.25, 1));
    let id = r.order(Side::Buy, 3, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.remaining(id), 2);
    r.market(es(), &[MarketInput::BookClear]);
    assert_eq!(r.remaining(id), 2);
    r.market(
        es(),
        &[MarketInput::Book {
            side: BookSide::Bid,
            price: px(6000.00),
            qty: Qty(5),
        }],
    );
    assert_eq!(r.remaining(id), 2, "a bid is no ask to buy from");
}

/// Market orders working come first on a trade; a resting limit gets only what is left.
#[test]
fn a_working_market_order_is_first_in_line_on_a_trade() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 3), (6000.25, 1));
    let mkt = r.order(Side::Buy, 3, OrderKind::Market);
    let lmt = r.order(Side::Buy, 1, OrderKind::Limit { price: px(6000.75) });
    r.wait(LAT);
    assert_eq!(r.remaining(mkt), 2);
    r.trade(6000.50, 2);
    assert_eq!(r.fills(mkt).last(), Some(&(px(6000.50), 2)));
    assert!(r.fills(lmt).is_empty(), "nothing left of the print");
}

/// The person can cancel what works on.
#[test]
fn the_rest_can_be_cancelled() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 3), (6000.25, 1));
    let id = r.order(Side::Buy, 3, OrderKind::Market);
    r.wait(LAT);
    r.cmd(Command::CancelOrder {
        account: acct(),
        order: id,
    })
    .unwrap();
    r.wait(LAT);
    assert_eq!(r.state(id), OrderState::Cancelled);
    r.top((6000.00, 3), (6000.25, 5));
    assert_eq!(r.pos(es()), 1, "nothing more after the cancel");
}

/// Flatten on a one-level book: long 5, the bid has 2. It sells 2, works on, and the next quotes
/// close the rest; never short.
#[test]
fn flatten_on_a_one_level_book_closes_the_whole_position() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 2), (6000.25, 10));
    r.order(Side::Buy, 5, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.pos(es()), 5);
    r.top((6000.00, 2), (6000.25, 10));
    let flat = r.flatten(es());
    r.wait(LAT);
    assert_eq!(r.pos(es()), 3, "the inside's 2");
    assert_eq!(r.state(flat), OrderState::PartiallyFilled);
    r.top((5999.75, 2), (6000.00, 10));
    assert_eq!(r.pos(es()), 1);
    r.top((5999.50, 4), (5999.75, 10));
    assert_eq!(r.pos(es()), 0);
    assert_eq!(r.state(flat), OrderState::Filled);
    r.top((5999.50, 4), (5999.75, 10));
    assert_eq!(r.pos(es()), 0, "never past flat");
}

/// The Flatten's rest is reduce-only: when something else closes part of the position (here a
/// stop exit), it shrinks, so it never sells past flat.
#[test]
fn the_rest_of_a_flatten_shrinks_with_the_position() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 2), (6000.25, 10));
    r.order(Side::Buy, 5, OrderKind::Market);
    r.wait(LAT);
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
    r.top((6000.00, 2), (6000.25, 10));
    let flat = r.flatten(es());
    r.wait(LAT);
    assert_eq!(r.pos(es()), 3);
    assert_eq!(r.remaining(flat), 3);
    // A print at 5999.00 fires the stop (2 at the print): the position is 1, so is the rest, which
    // takes the print too.
    r.trade(5999.00, 10);
    assert_eq!(r.fills(stop), [(px(5999.00), 2)]);
    assert_eq!(r.pos(es()), 0, "never short");
    assert_eq!(r.state(flat), OrderState::Filled);
    let sold: i64 = r.fills(flat).iter().map(|f| f.1).sum();
    assert_eq!(sold, 3, "the 2 from the book and the 1 left");
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

/// A print at the bid never fills a working buy: a real market order pays the ask or more.
#[test]
fn a_print_on_the_far_side_never_fills_the_rest() {
    let mut r = Rig::new(CME_OPEN);
    r.top((6000.00, 3), (6000.25, 2));
    let id = r.order(Side::Buy, 5, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.remaining(id), 3);
    r.trade(6000.00, 10);
    assert_eq!(
        r.remaining(id),
        3,
        "a seller hit the bid: nothing for a buyer"
    );
    r.trade(6000.25, 10);
    assert_eq!(r.fills(id), [(px(6000.25), 2), (px(6000.25), 3)]);
}

/// The CME's daily close ends a working remainder: nothing fills at the reopen's gap, live or on
/// a replay that jumps past the close.
#[test]
fn the_session_close_ends_the_rest() {
    let mut r = Rig::new("2026-11-10T21:59:00Z");
    r.top((6000.00, 3), (6000.25, 2));
    let id = r.order(Side::Buy, 5, OrderKind::Market);
    r.wait(LAT);
    assert_eq!(r.remaining(id), 3);
    // A replay's next data is the reopen, an hour later.
    r.t = UtcMillis::parse_iso("2026-11-10T23:00:00Z").unwrap().0;
    r.top((6010.00, 3), (6010.25, 50));
    assert_eq!(r.state(id), OrderState::Cancelled);
    assert_eq!(r.pos(es()), 2, "only what filled before the close");
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
