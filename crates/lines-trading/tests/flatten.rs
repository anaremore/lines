//! A Flatten that can't close leaves the protection in place. On a broker honoring reduce-only the
//! market order goes first and exits stay until the position is flat; on one that doesn't, orders
//! are cancelled first and the market order waits for the answers. Cancel All is unchanged.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lines_core::{CivilDate, ContractKey, Month, Price, Qty, Right, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::broker::{BrokerEvent, BrokerRequest, BrokerSnapshot};
use lines_trading::trigger::{TriggerKind, TriggerSpec, TriggerState};
use lines_trading::{
    AccountConfig, AccountId, Broker, BrokerCaps, Command, Event, MarketInput, Money, OrderId,
    OrderKind, OrderSpec, OrderState, PaperBroker, PaperConfig, Side, Source, TradingCore,
};

const LAT: i64 = 20;
const CME_OPEN: &str = "2026-11-10T15:00:00Z";
/// The CME's daily halt, the same day.
const CME_HALT: &str = "2026-11-10T22:30:00Z";
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
fn spy() -> ContractKey {
    ContractKey::equity(catalog::SPY)
}
fn acct() -> AccountId {
    AccountId::new("paper-1")
}

/// The paper broker, optionally saying it doesn't honor reduce-only, and able to hold back cancels.
struct Venue {
    inner: PaperBroker,
    oco: bool,
    /// Cancels to lose before answering any.
    lose_cancels: Arc<AtomicUsize>,
    /// Market orders placed.
    markets: Arc<AtomicUsize>,
    /// Cancels asked for.
    cancels: Arc<AtomicUsize>,
    /// Lose market orders placed, as a venue that never answers.
    hold_places: Arc<AtomicBool>,
    /// The connection is down.
    down: Arc<AtomicBool>,
    /// Lose cancels of these orders.
    lose_ids: Arc<Mutex<Vec<OrderId>>>,
    /// Events to report as if from the venue.
    inject: Arc<Mutex<Vec<BrokerEvent>>>,
}
impl Broker for Venue {
    fn connected(&self) -> bool {
        !self.down.load(Ordering::Relaxed)
    }
    fn name(&self) -> &str {
        "venue"
    }
    fn capabilities(&self) -> BrokerCaps {
        let mut c = self.inner.capabilities();
        c.oco = self.oco;
        c
    }
    fn request(&mut self, now: UtcMillis, req: BrokerRequest) {
        match &req {
            BrokerRequest::Place(o) if matches!(o.kind, OrderKind::Market) => {
                self.markets.fetch_add(1, Ordering::Relaxed);
                if self.hold_places.load(Ordering::Relaxed) {
                    return;
                }
            }
            BrokerRequest::Cancel { id } => {
                self.cancels.fetch_add(1, Ordering::Relaxed);
                if self.lose_ids.lock().unwrap().contains(id) {
                    return;
                }
                let left = self.lose_cancels.load(Ordering::Relaxed);
                if left > 0 {
                    self.lose_cancels.store(left - 1, Ordering::Relaxed);
                    return;
                }
            }
            _ => {}
        }
        self.inner.request(now, req)
    }
    fn market(&mut self, now: UtcMillis, contract: ContractKey, input: &MarketInput) {
        self.inner.market(now, contract, input)
    }
    fn poll(&mut self, now: UtcMillis, out: &mut Vec<BrokerEvent>) {
        self.inner.poll(now, out);
        out.append(&mut self.inject.lock().unwrap());
    }
    fn next_wake(&self) -> Option<UtcMillis> {
        self.inner.next_wake()
    }
    fn snapshot(&self) -> BrokerSnapshot {
        self.inner.snapshot()
    }
    fn simulated(&self) -> bool {
        true
    }
}

struct Rig {
    core: TradingCore,
    t: i64,
    events: Vec<Event>,
    lose_cancels: Arc<AtomicUsize>,
    markets: Arc<AtomicUsize>,
    cancels: Arc<AtomicUsize>,
    hold_places: Arc<AtomicBool>,
    down: Arc<AtomicBool>,
    lose_ids: Arc<Mutex<Vec<OrderId>>>,
    inject: Arc<Mutex<Vec<BrokerEvent>>>,
}
impl Rig {
    fn new(oco: bool, at: &str) -> Rig {
        let mut inner = PaperBroker::new(PaperConfig {
            latency_ms: LAT,
            ..Default::default()
        });
        inner.open_account(acct(), Money::whole(100_000));
        let lose_cancels = Arc::new(AtomicUsize::new(0));
        let markets = Arc::new(AtomicUsize::new(0));
        let cancels = Arc::new(AtomicUsize::new(0));
        let hold_places = Arc::new(AtomicBool::new(false));
        let down = Arc::new(AtomicBool::new(false));
        let lose_ids = Arc::new(Mutex::new(Vec::new()));
        let inject = Arc::new(Mutex::new(Vec::new()));
        let mut core = TradingCore::new(Box::new(Venue {
            inner,
            oco,
            lose_cancels: lose_cancels.clone(),
            markets: markets.clone(),
            cancels: cancels.clone(),
            hold_places: hold_places.clone(),
            down: down.clone(),
            lose_ids: lose_ids.clone(),
            inject: inject.clone(),
        }));
        core.add_account(AccountConfig::paper("paper-1", 100_000), None);
        Rig {
            core,
            t: UtcMillis::parse_iso(at).unwrap().0,
            events: Vec::new(),
            lose_cancels,
            markets,
            cancels,
            hold_places,
            down,
            lose_ids,
            inject,
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
    fn book(&mut self, c: ContractKey, bid: i64) {
        self.t += 1;
        let i = [
            MarketInput::BookClear,
            MarketInput::Book {
                side: BookSide::Bid,
                price: Price(bid),
                qty: Qty(500),
            },
            MarketInput::Book {
                side: BookSide::Ask,
                price: Price(bid + 25),
                qty: Qty(500),
            },
        ];
        self.core.market(self.now(), c, &i);
        self.collect();
    }
    fn long_es(&mut self, q: i64) {
        self.book(es(), 600_000);
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Buy,
            qty: Qty(q),
            kind: OrderKind::Market,
        }))
        .unwrap();
        self.wait(LAT + 1);
        assert_eq!(self.core.position(&acct(), es()), q);
    }
    fn es_stop(&mut self, q: i64) -> OrderId {
        let id = self
            .cmd(Command::PlaceExit(OrderSpec {
                account: acct(),
                contract: es(),
                side: Side::Sell,
                qty: Qty(q),
                kind: OrderKind::Stop {
                    trigger: Price(590_000),
                },
            }))
            .unwrap()[0];
        self.wait(LAT + 1);
        id
    }
    fn flatten(&mut self, c: ContractKey) {
        self.cmd(Command::Flatten {
            account: acct(),
            contract: Some(c),
        })
        .unwrap();
    }
    /// Cancel the Flatten's market order still working on the contract (it found no bid).
    fn cancel_flatten(&mut self, c: ContractKey) {
        let id = self
            .core
            .open_orders(&acct())
            .find(|o| o.contract == c && matches!(o.kind, OrderKind::Market))
            .map(|o| o.id)
            .expect("the Flatten works on");
        self.cmd(Command::CancelOrder {
            account: acct(),
            order: id,
        })
        .unwrap();
        self.wait(LAT + 1);
    }
    fn quote(&mut self, bid: Option<i64>, ask: Option<i64>) {
        self.t += 1;
        let time = self.now();
        self.core.market(
            self.now(),
            call(),
            &[MarketInput::OptionQuote {
                bid: bid.map(Price),
                ask: ask.map(Price),
                last: None,
                time,
            }],
        );
        self.collect();
    }
    fn print(&mut self, cents: i64) {
        self.t += 1;
        let time = self.now();
        self.core.market(
            self.now(),
            spy(),
            &[MarketInput::Trade {
                price: Price(cents),
                qty: Qty(1),
                time,
            }],
        );
        self.collect();
    }
    fn long_call(&mut self, q: i64) {
        self.print(68_300);
        self.quote(Some(240), Some(242));
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: call(),
            side: Side::Buy,
            qty: Qty(q),
            kind: OrderKind::Limit { price: Price(242) },
        }))
        .unwrap();
        self.wait(LAT + 1);
        assert_eq!(self.core.position(&acct(), call()), q);
    }
    fn arm(&mut self, kind: TriggerKind, level: i64) {
        self.cmd(Command::ArmTrigger(TriggerSpec {
            account: acct(),
            contract: call(),
            kind,
            level: Price(level),
        }))
        .unwrap();
    }
    fn state(&self, id: OrderId) -> OrderState {
        self.core.order(id).unwrap().state.clone()
    }
    fn fills(&self, c: ContractKey) -> Vec<(Side, i64, i64)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Fill(f) if f.contract == c => Some((f.side, f.price.0, f.qty.0)),
                _ => None,
            })
            .collect()
    }
}

/// Long 1 with a Stop, the bid gone, Flatten: the position stays, so the Stop does. The Flatten's
/// market order works on and sells when a bid comes back; cancelled, the Stop still fires when
/// the level is reached.
#[test]
fn a_flatten_with_no_bid_keeps_the_stop() {
    let mut r = Rig::new(true, RTH);
    r.long_call(1);
    r.arm(TriggerKind::Stop, 68_200);
    r.quote(None, Some(242));
    r.flatten(call());
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), call()), 1, "no bid to fill on");
    assert_eq!(r.core.triggers().count(), 1, "still protected");
    r.cancel_flatten(call());
    assert_eq!(r.core.triggers().count(), 1, "still protected");
    r.quote(Some(100), Some(102));
    r.print(68_100);
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), call()), 0, "the Stop closed it");
    let reason = r.events.iter().rev().find_map(|e| match e {
        Event::Trigger(t) if t.state == TriggerState::Ended => t.reason.clone(),
        _ => None,
    });
    assert_eq!(
        reason.as_deref(),
        Some("closed"),
        "the Stop's doing, so it's news"
    );
    assert_eq!(r.fills(call()).last(), Some(&(Side::Sell, 100, 1)));
    assert_eq!(r.core.triggers().count(), 0);
}

/// A Flatten that closes: the exits end by themselves once the position is flat.
#[test]
fn a_flatten_that_closes_ends_the_exits() {
    let mut r = Rig::new(true, RTH);
    r.long_call(2);
    r.arm(TriggerKind::Stop, 68_200);
    r.arm(TriggerKind::Target, 68_500);
    r.flatten(call());
    assert_eq!(r.core.triggers().count(), 2, "not until it's flat");
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), call()), 0);
    assert_eq!(r.fills(call()).last(), Some(&(Side::Sell, 240, 2)));
    assert_eq!(r.core.triggers().count(), 0, "closed, so they're over");
    // It's what the person asked for, so no news: the reason the app stays quiet on.
    let reasons: Vec<_> = r
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Trigger(t) if t.state == TriggerState::Ended => t.reason.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(reasons, ["you flattened", "you flattened"]);
    // A later print through a level does nothing.
    r.print(68_100);
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), call()), 0);
}

/// A Flatten ends an armed entry at once, closing or not: it mustn't reopen what's being closed.
#[test]
fn a_flatten_ends_entries_whatever_happens() {
    let mut r = Rig::new(true, RTH);
    r.long_call(1);
    r.arm(TriggerKind::Stop, 68_200);
    r.arm(
        TriggerKind::Entry {
            qty: Qty(1),
            max_price: Price(300),
        },
        68_400,
    );
    r.quote(None, Some(242));
    r.flatten(call());
    let left: Vec<_> = r.core.triggers().map(|t| t.kind).collect();
    assert_eq!(left, [TriggerKind::Stop], "the entry went, the Stop didn't");
    r.wait(LAT + 1);
    r.cancel_flatten(call());
    r.quote(Some(240), Some(242));
    r.print(68_400);
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), call()), 1, "nothing bought");
}

/// A Flatten with no bid isn't over: its market order works on, and the bid's return closes the
/// position at it, the Flatten's doing.
#[test]
fn a_flatten_with_no_bid_sells_when_the_bid_returns() {
    let mut r = Rig::new(true, RTH);
    r.long_call(1);
    r.arm(TriggerKind::Stop, 68_200);
    r.quote(None, Some(242));
    r.flatten(call());
    r.wait(LAT + 1);
    let market = r
        .core
        .open_orders(&acct())
        .find(|o| matches!(o.kind, OrderKind::Market))
        .map(|o| (o.id, o.state.clone()))
        .expect("working on, visible");
    assert_eq!(market.1, OrderState::Working);
    r.quote(Some(150), Some(152));
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), call()), 0);
    assert_eq!(r.fills(call()).last(), Some(&(Side::Sell, 150, 1)));
    assert_eq!(r.state(market.0), OrderState::Filled);
    assert_eq!(r.core.triggers().count(), 0);
}

/// Cancel All keeps its rule: it ends a position's Stop, and leaves the position.
#[test]
fn cancel_all_still_ends_the_stop_and_leaves_the_position() {
    let mut r = Rig::new(true, RTH);
    r.long_call(1);
    r.arm(TriggerKind::Stop, 68_200);
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: Some(call()),
    })
    .unwrap();
    assert_eq!(r.core.triggers().count(), 0);
    let reason = r.events.iter().rev().find_map(|e| match e {
        Event::Trigger(t) if t.state == TriggerState::Ended => t.reason.clone(),
        _ => None,
    });
    assert_eq!(reason.as_deref(), Some("you canceled all"));
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), call()), 1);

    // On futures too: its stop goes, the position stays.
    let mut r = Rig::new(true, CME_OPEN);
    r.long_es(1);
    let stop = r.es_stop(1);
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: Some(es()),
    })
    .unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.state(stop), OrderState::Cancelled);
    assert_eq!(r.core.position(&acct(), es()), 1);
}

/// Flatten in the CME's daily halt: the market order is refused, the stop stays.
#[test]
fn a_flatten_in_a_halt_keeps_the_stop() {
    let mut r = Rig::new(true, CME_OPEN);
    r.long_es(2);
    let stop = r.es_stop(2);
    r.t = UtcMillis::parse_iso(CME_HALT).unwrap().0;
    r.flatten(es());
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), es()), 2);
    assert!(r.core.order(stop).unwrap().is_open(), "{:?}", r.state(stop));
    assert_eq!(r.core.open_orders(&acct()).count(), 1, "the stop alone");
}

/// A Flatten that closes futures: the market order goes first, fills, and the stop is cleaned up.
/// A plain working order goes at once: it isn't protection, and could reopen the position.
#[test]
fn a_flatten_that_closes_cleans_up_after_itself() {
    let mut r = Rig::new(true, CME_OPEN);
    r.long_es(2);
    let stop = r.es_stop(2);
    r.book(es(), 600_000);
    let plain = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(601_000),
            },
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    r.flatten(es());
    assert!(
        r.core.order(stop).unwrap().is_open(),
        "not before it's flat"
    );
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), es()), 0);
    assert_eq!(r.state(plain), OrderState::Cancelled);
    assert!(!r.core.order(stop).unwrap().is_open(), "flat: cleaned up");
    assert_eq!(r.core.open_orders(&acct()).count(), 0);
    assert_eq!(
        r.markets.load(Ordering::Relaxed),
        2,
        "one to open, one to close"
    );
}

/// An exit older than the Flatten doesn't hold the position's budget against it, and the Flatten
/// doesn't take it from the exit: the market order fills, and until then the exit still covers.
#[test]
fn a_flatten_and_an_exit_dont_compete_for_the_position() {
    let mut r = Rig::new(true, CME_OPEN);
    r.long_es(2);
    let target = r
        .cmd(Command::PlaceExit(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(2),
            kind: OrderKind::Limit {
                price: Price(610_000),
            },
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    r.flatten(es());
    r.wait(LAT + 1);
    assert_eq!(
        r.core.position(&acct(), es()),
        0,
        "the older exit didn't block it"
    );
    assert_eq!(r.state(target), OrderState::Cancelled);
    let sold: i64 = r
        .fills(es())
        .iter()
        .filter(|f| f.0 == Side::Sell)
        .map(|f| f.2)
        .sum();
    assert_eq!(sold, 2, "closed once");
}

/// A broker that doesn't honor reduce-only: orders go first, and the market order only after the
/// answers, so a working exit can't fill too and reverse the position.
#[test]
fn without_reduce_only_the_market_order_waits_for_the_cancels() {
    let mut r = Rig::new(false, CME_OPEN);
    r.long_es(2);
    // `PlaceExit` needs OCO: a plain working sell stands in for what could fill too.
    r.book(es(), 600_000);
    let plain = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(2),
            kind: OrderKind::Limit {
                price: Price(601_000),
            },
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    let before = r.markets.load(Ordering::Relaxed);
    r.flatten(es());
    assert_eq!(r.markets.load(Ordering::Relaxed), before, "not yet");
    // Asked again while it waits: still one.
    r.flatten(es());
    r.wait(LAT + 1);
    assert_eq!(r.state(plain), OrderState::Cancelled);
    assert_eq!(r.markets.load(Ordering::Relaxed), before + 1, "now");
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), es()), 0, "closed, not reversed");
}

/// With nothing else working, it doesn't wait for anything.
#[test]
fn without_reduce_only_and_nothing_working_it_goes_at_once() {
    let mut r = Rig::new(false, CME_OPEN);
    r.long_es(1);
    let before = r.markets.load(Ordering::Relaxed);
    r.flatten(es());
    assert_eq!(r.markets.load(Ordering::Relaxed), before + 1);
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), es()), 0);
}

/// A venue that never answers a cancel can't hold the exit back forever.
#[test]
fn a_lost_cancel_doesnt_hold_the_flatten_back() {
    let mut r = Rig::new(false, CME_OPEN);
    r.long_es(1);
    r.book(es(), 600_000);
    r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Sell,
        qty: Qty(1),
        kind: OrderKind::Limit {
            price: Price(601_000),
        },
    }))
    .unwrap();
    r.wait(LAT + 1);
    // Lost every time it's sent: its resends go by, and the wait still ends.
    r.lose_cancels.store(99, Ordering::Relaxed);
    let before = r.markets.load(Ordering::Relaxed);
    r.flatten(es());
    let asked = r.t;
    r.wait(LAT + 1);
    assert_eq!(r.markets.load(Ordering::Relaxed), before, "still waiting");
    let wake = r.core.next_wake().expect("a time to stop waiting");
    assert!(wake.0 > r.t && wake.0 - r.t <= 5_000);
    r.t = asked + 4_999;
    r.wait(0);
    assert_eq!(r.markets.load(Ordering::Relaxed), before, "until its 5 s");
    assert!(
        r.cancels.load(Ordering::Relaxed) > 1,
        "sent again meanwhile"
    );
    r.wait(1);
    assert_eq!(r.markets.load(Ordering::Relaxed), before + 1);
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), es()), 0);
}

/// A rule's Flatten arriving just as a waiting one's cancels are answered: one market order, not
/// two, which would reverse the position on a broker that doesn't honor reduce-only.
#[test]
fn a_rule_flatten_and_a_waiting_one_send_one_order() {
    use lines_trading::guardrails::Guardrails;
    use lines_trading::news::{NewsAction, NewsBlackout};
    const SCHEDULE: &str = "\
format 1
version 2026-11-01
zone America/New_York
release cpi
name CPI
agency BLS
importance high
affects ES MES NQ MNQ ZN
coverage 2026-03-01 2026-12-31 verified
source https://www.bls.gov/schedule/news_release/cpi.htm
2026-11-10 08:30 verified October 2026
";
    // The news window opens two minutes before 08:30 ET.
    let window = UtcMillis::parse_iso("2026-11-10T13:28:00Z").unwrap().0;
    let mut r = Rig::new(false, "2026-11-10T13:00:00Z");
    r.core.set_econ_schedule(Some(Arc::new(
        lines_core::econ::EconSchedule::parse(SCHEDULE).unwrap(),
    )));
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails {
            news: Some(NewsBlackout::new(NewsAction::Flatten)),
            ..Default::default()
        },
    })
    .unwrap();
    r.long_es(1);
    r.book(es(), 600_000);
    r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Sell,
        qty: Qty(1),
        kind: OrderKind::Limit {
            price: Price(601_000),
        },
    }))
    .unwrap();
    r.wait(LAT + 1);
    let before = r.markets.load(Ordering::Relaxed);
    r.t = window - 10;
    r.flatten(es());
    assert_eq!(r.markets.load(Ordering::Relaxed), before, "it waits");
    // The cancel's answer and the window's start in one tick.
    r.wait(LAT);
    assert_eq!(r.markets.load(Ordering::Relaxed), before + 1, "one");
    r.wait(LAT + 1);
    r.wait(6_000);
    assert_eq!(r.markets.load(Ordering::Relaxed), before + 1, "still one");
    assert_eq!(r.core.position(&acct(), es()), 0);
}

/// A DOM's stop is a plain order the broker doesn't cap: it could reverse the position in the
/// moment after the fill, so a Flatten cancels it at once, as a plain limit (decisions 0025, 0027).
/// In a halt that leaves the position without it: `docs/known-gaps.md`.
#[test]
fn a_flatten_cancels_a_plain_stop_at_once() {
    let mut r = Rig::new(true, CME_OPEN);
    r.long_es(2);
    let id = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(2),
            kind: OrderKind::Stop {
                trigger: Price(590_000),
            },
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    r.flatten(es());
    r.wait(LAT + 1);
    assert_eq!(r.state(id), OrderState::Cancelled);
    assert_eq!(r.core.position(&acct(), es()), 0);
    // The market falls through it: nothing opens.
    r.book(es(), 580_000);
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), es()), 0);

    // In the halt: it's gone, and the position stays. The stated limit, pinned.
    let mut r = Rig::new(true, CME_OPEN);
    r.long_es(2);
    let id = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(2),
            kind: OrderKind::Stop {
                trigger: Price(590_000),
            },
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    r.t = UtcMillis::parse_iso(CME_HALT).unwrap().0;
    r.flatten(es());
    r.wait(LAT + 1);
    assert_eq!(r.state(id), OrderState::Cancelled);
    assert_eq!(r.core.position(&acct(), es()), 2);
}

/// A Flatten that failed (its market order cancelled before a bid came back) gets no credit for
/// what closes the position later: the reason says what happened, and the app reports it.
#[test]
fn a_failed_flatten_doesnt_take_credit_for_a_later_close() {
    let last_reason = |r: &Rig| {
        r.events.iter().rev().find_map(|e| match e {
            Event::Trigger(t) if t.state == TriggerState::Ended => t.reason.clone(),
            _ => None,
        })
    };
    // A Stop already waiting for a bid, Flatten fails, the bid returns: the Stop closes it.
    let mut r = Rig::new(true, RTH);
    r.long_call(1);
    r.arm(TriggerKind::Stop, 68_200);
    r.quote(None, Some(242));
    r.print(68_100);
    r.flatten(call());
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), call()), 1);
    r.cancel_flatten(call());
    r.quote(Some(150), Some(152));
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), call()), 0);
    assert_eq!(last_reason(&r).as_deref(), Some("closed"));

    // Armed, Flatten fails, and a resting close fills later.
    let mut r = Rig::new(true, RTH);
    r.long_call(1);
    r.arm(TriggerKind::Stop, 68_000);
    r.cmd(Command::PlaceExit(OrderSpec {
        account: acct(),
        contract: call(),
        side: Side::Sell,
        qty: Qty(1),
        kind: OrderKind::Limit { price: Price(245) },
    }))
    .unwrap();
    r.wait(LAT + 1);
    r.quote(None, Some(242));
    r.flatten(call());
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), call()), 1);
    r.cancel_flatten(call());
    r.quote(Some(250), Some(252));
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), call()), 0);
    assert_eq!(last_reason(&r).as_deref(), Some("the position closed"));
}

/// A person's Cancel All doesn't take back a rule's waiting Flatten, which asks only once; and a
/// rule asking while a person's waits makes it the rule's.
#[test]
fn cancel_all_leaves_a_rules_waiting_flatten() {
    use lines_trading::guardrails::Guardrails;
    use lines_trading::news::{NewsAction, NewsBlackout};
    const SCHEDULE: &str = "\
format 1
version 2026-11-01
zone America/New_York
release cpi
name CPI
agency BLS
importance high
affects ES MES NQ MNQ ZN
coverage 2026-03-01 2026-12-31 verified
source https://www.bls.gov/schedule/news_release/cpi.htm
2026-11-10 08:30 verified October 2026
";
    let window = UtcMillis::parse_iso("2026-11-10T13:28:00Z").unwrap().0;
    for person_first in [false, true] {
        let mut r = Rig::new(false, "2026-11-10T13:00:00Z");
        r.core.set_econ_schedule(Some(Arc::new(
            lines_core::econ::EconSchedule::parse(SCHEDULE).unwrap(),
        )));
        r.cmd(Command::SetGuardrails {
            account: acct(),
            guardrails: Guardrails {
                news: Some(NewsBlackout::new(NewsAction::Flatten)),
                ..Default::default()
            },
        })
        .unwrap();
        r.long_es(1);
        r.book(es(), 600_000);
        r.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(601_000),
            },
        }))
        .unwrap();
        r.wait(LAT + 1);
        let before = r.markets.load(Ordering::Relaxed);
        r.t = window;
        if person_first {
            r.flatten(es());
        }
        // The window's start: the rule's Flatten waits for the cancel.
        r.wait(0);
        assert_eq!(r.markets.load(Ordering::Relaxed), before, "it waits");
        r.cmd(Command::CancelAll {
            account: acct(),
            contract: Some(es()),
        })
        .unwrap();
        r.wait(LAT + 1);
        assert_eq!(
            r.markets.load(Ordering::Relaxed),
            before + 1,
            "still goes (person first: {person_first})"
        );
        r.wait(LAT + 1);
        assert_eq!(r.core.position(&acct(), es()), 0);
    }
}

/// An order placed while a Flatten waits isn't waited for, but it goes when the market order does.
#[test]
fn an_order_placed_while_waiting_doesnt_hold_it_and_goes_too() {
    let mut r = Rig::new(false, CME_OPEN);
    r.long_es(1);
    r.book(es(), 600_000);
    let first = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(601_000),
            },
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    let before = r.markets.load(Ordering::Relaxed);
    r.flatten(es());
    let late = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(602_000),
            },
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    assert_eq!(r.state(first), OrderState::Cancelled);
    assert_eq!(
        r.markets.load(Ordering::Relaxed),
        before + 1,
        "not held for 5 s"
    );
    r.wait(LAT + 1);
    assert_eq!(r.state(late), OrderState::Cancelled);
    assert_eq!(r.core.position(&acct(), es()), 0);
}

/// A person's Cancel All takes back a Flatten still waiting: it leaves the position.
#[test]
fn cancel_all_takes_back_a_waiting_flatten() {
    let mut r = Rig::new(false, CME_OPEN);
    r.long_es(1);
    r.book(es(), 600_000);
    r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Sell,
        qty: Qty(1),
        kind: OrderKind::Limit {
            price: Price(601_000),
        },
    }))
    .unwrap();
    r.wait(LAT + 1);
    let before = r.markets.load(Ordering::Relaxed);
    r.flatten(es());
    r.cmd(Command::CancelAll {
        account: acct(),
        contract: Some(es()),
    })
    .unwrap();
    r.wait(LAT + 1);
    r.wait(6_000);
    assert_eq!(r.markets.load(Ordering::Relaxed), before, "taken back");
    assert_eq!(r.core.position(&acct(), es()), 1);
}

/// On a broker honoring reduce-only, a Flatten order silent past its answer time is cancelled and
/// replaced as any other order: it isn't kept as if it were an exit.
#[test]
fn a_silent_flatten_order_is_replaced_not_kept() {
    let mut r = Rig::new(true, RTH);
    r.long_call(1);
    // Asked with no bid and its answer never reported: the order stays open.
    r.hold_places.store(true, Ordering::Relaxed);
    r.flatten(call());
    let first = *r
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Command {
                result: Ok(ids), ..
            } => Some(ids),
            _ => None,
        })
        .unwrap()
        .last()
        .unwrap();
    r.wait(1);
    assert!(r.core.order(first).unwrap().is_open());
    r.hold_places.store(false, Ordering::Relaxed);
    r.t += 5_001;
    r.flatten(call());
    assert!(
        r.cancels.load(Ordering::Relaxed) >= 1,
        "the silent one is canceled"
    );
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), call()), 0);
}

/// Where the double-Flatten guard meets the wait: on a broker that could fill both, a Flatten a
/// rebuild left unresolved may still fill. Inside 5 s it stands and no second order goes, however
/// often it's asked. Past that it gets a cancel and is replaced, once, as `tests/cancels.rs` holds.
#[test]
fn without_reduce_only_an_unresolved_flatten_holds_the_next_one() {
    let mut r = Rig::new(false, CME_OPEN);
    r.long_es(2);
    let before = r.markets.load(Ordering::Relaxed);
    r.hold_places.store(true, Ordering::Relaxed);
    r.flatten(es());
    assert_eq!(r.markets.load(Ordering::Relaxed), before + 1);
    let now = r.now();
    r.core.rebuild(now);
    r.collect();
    let first = r
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Order(o) if o.state == OrderState::Unresolved => Some(o.id),
            _ => None,
        })
        .expect("the rebuild left it unresolved");
    r.hold_places.store(false, Ordering::Relaxed);

    // Inside 5 s: nothing goes and it isn't cancelled; nothing waits to go either.
    for _ in 0..2 {
        r.wait(1_000);
        r.flatten(es());
    }
    r.wait(LAT + 1);
    assert_eq!(r.markets.load(Ordering::Relaxed), before + 1);
    assert_eq!(r.cancels.load(Ordering::Relaxed), 0);
    assert_eq!(r.core.position(&acct(), es()), 2);

    // Past 5 s: its cancel and the replacement, and a third asking adds neither.
    r.wait(5_001);
    r.flatten(es());
    r.flatten(es());
    assert_eq!(r.cancels.load(Ordering::Relaxed), 1);
    assert_eq!(r.markets.load(Ordering::Relaxed), before + 2);
    r.wait(LAT + 1);
    assert_eq!(r.core.order(first).unwrap().state, OrderState::Cancelled);
    assert_eq!(r.core.position(&acct(), es()), 0, "flat, not reversed");
}

/// A working order waiting on its cancel and an unresolved Flatten at once: the close waits for
/// that order's answer, not the Flatten's, and goes once.
#[test]
fn without_reduce_only_an_unresolved_flatten_and_a_working_order_send_one_close() {
    let mut r = Rig::new(false, CME_OPEN);
    r.long_es(2);
    let before = r.markets.load(Ordering::Relaxed);
    r.hold_places.store(true, Ordering::Relaxed);
    r.flatten(es());
    let now = r.now();
    r.core.rebuild(now);
    r.collect();
    r.hold_places.store(false, Ordering::Relaxed);
    let stop = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(2),
            kind: OrderKind::Limit {
                price: Price(601_000),
            },
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    r.lose_ids.lock().unwrap().push(stop);
    r.wait(5_001);
    r.flatten(es());
    r.wait(1_000);
    assert_eq!(
        r.markets.load(Ordering::Relaxed),
        before + 1,
        "that order's cancel is unanswered"
    );
    r.lose_ids.lock().unwrap().clear();
    r.wait(1_000);
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(r.core.order(stop).unwrap().state, OrderState::Cancelled);
    assert_eq!(r.markets.load(Ordering::Relaxed), before + 2);
    assert_eq!(r.core.position(&acct(), es()), 0, "flat, not reversed");
}

/// The first Flatten speaks up while the next waits: when the wait ends nothing goes beside it,
/// as a press wouldn't. After 5 s of silence again, it's replaced.
#[test]
fn a_waiting_flatten_doesnt_go_beside_one_that_spoke_up() {
    let mut r = Rig::new(false, CME_OPEN);
    r.long_es(2);
    let before = r.markets.load(Ordering::Relaxed);
    r.hold_places.store(true, Ordering::Relaxed);
    r.flatten(es());
    let now = r.now();
    r.core.rebuild(now);
    r.collect();
    let first = r
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Order(o) if o.state == OrderState::Unresolved => Some(o.id),
            _ => None,
        })
        .expect("the rebuild left it unresolved");
    r.hold_places.store(false, Ordering::Relaxed);
    let other = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(2),
            kind: OrderKind::Limit {
                price: Price(601_000),
            },
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    // Past 5 s: both get a cancel, and the other's is lost, so the next one waits.
    *r.lose_ids.lock().unwrap() = vec![first, other];
    r.wait(5_100);
    r.flatten(es());
    assert_eq!(r.markets.load(Ordering::Relaxed), before + 1);
    // The first speaks up, and the other's cancel is answered: the wait is over.
    r.wait(500);
    r.inject.lock().unwrap().push(BrokerEvent::Working {
        id: first,
        at: r.now(),
    });
    r.wait(1);
    *r.lose_ids.lock().unwrap() = vec![first];
    r.wait(1_600);
    r.wait(LAT + 1);
    assert_eq!(r.core.order(other).unwrap().state, OrderState::Cancelled);
    assert_eq!(
        r.markets.load(Ordering::Relaxed),
        before + 1,
        "not beside the first"
    );
    // Silent for 5 s again: the wake comes on time, and it's replaced.
    let spoke = r.core.order(first).unwrap().updated.0;
    let wake = r.core.next_wake().expect("it looks again");
    assert!(wake.0 > r.t, "not in a loop: {} vs {}", wake.0, r.t);
    assert!(wake.0 <= spoke + 5_000, "{} vs {spoke}", wake.0);
    r.t = spoke + 5_000;
    r.wait(0);
    r.wait(LAT + 1);
    assert_eq!(r.markets.load(Ordering::Relaxed), before + 2);
    assert_eq!(r.core.position(&acct(), es()), 0);
}

/// A rebuild comes before the venue lists an order being cancelled: it reads unresolved, its cancel
/// is still unanswered, and it may still fill, so the close still waits.
#[test]
fn a_rebuild_doesnt_end_the_wait_for_a_cancel() {
    let mut r = Rig::new(false, CME_OPEN);
    r.long_es(2);
    let other = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(2),
            kind: OrderKind::Limit {
                price: Price(601_000),
            },
        }))
        .unwrap()[0];
    let before = r.markets.load(Ordering::Relaxed);
    r.lose_ids.lock().unwrap().push(other);
    r.flatten(es());
    let now = r.now();
    r.core.rebuild(now);
    r.collect();
    assert_eq!(r.core.order(other).unwrap().state, OrderState::Unresolved);
    r.wait(1_000);
    assert_eq!(
        r.markets.load(Ordering::Relaxed),
        before,
        "its cancel is unanswered"
    );
    // Answered on the resend: the close goes, and only that closes the position.
    r.lose_ids.lock().unwrap().clear();
    r.wait(1_000);
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(r.markets.load(Ordering::Relaxed), before + 1);
    assert_eq!(r.core.position(&acct(), es()), 0, "flat, not reversed");
}

/// A Flatten waiting when the broker drops: its market order can't go, and what the position is
/// when it's back isn't known yet, so it ends as a rejected order saying so. Nothing goes later,
/// and nothing spins; the next Flatten is a person's to ask.
#[test]
fn a_waiting_flatten_ends_on_a_disconnect_and_says_so() {
    let mut r = Rig::new(false, CME_OPEN);
    r.long_es(2);
    r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Sell,
        qty: Qty(2),
        kind: OrderKind::Limit {
            price: Price(601_000),
        },
    }))
    .unwrap();
    r.wait(LAT + 1);
    let before = r.markets.load(Ordering::Relaxed);
    r.lose_cancels.store(99, Ordering::Relaxed);
    r.flatten(es());
    r.down.store(true, Ordering::Relaxed);
    r.wait(1);
    assert_eq!(r.markets.load(Ordering::Relaxed), before);
    let reason = r.events.iter().rev().find_map(|e| match e {
        Event::Order(o) => match &o.state {
            OrderState::Rejected { reason } => Some(reason.clone()),
            _ => None,
        },
        _ => None,
    });
    assert!(
        reason
            .as_deref()
            .is_some_and(|s| s.contains("disconnected")),
        "{reason:?}"
    );
    r.wait(6_000);
    r.down.store(false, Ordering::Relaxed);
    r.lose_cancels.store(0, Ordering::Relaxed);
    r.wait(1);
    r.wait(LAT + 1);
    assert_eq!(
        r.markets.load(Ordering::Relaxed),
        before,
        "nothing goes later"
    );
    assert_eq!(r.core.position(&acct(), es()), 2);
    // Asked again, it closes.
    r.flatten(es());
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), es()), 0);
}

/// An order placed during the wait goes when the position closes, as when the Flatten does: it
/// could open a new one.
#[test]
fn an_order_placed_while_waiting_goes_when_the_position_closes() {
    let mut r = Rig::new(false, CME_OPEN);
    r.long_es(1);
    r.book(es(), 600_000);
    r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Sell,
        qty: Qty(1),
        kind: OrderKind::Limit {
            price: Price(600_000),
        },
    }))
    .unwrap();
    r.lose_cancels.store(1, Ordering::Relaxed);
    r.flatten(es());
    let buy = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Buy,
            qty: Qty(1),
            kind: OrderKind::Limit {
                price: Price(590_000),
            },
        }))
        .unwrap()[0];
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), es()), 0, "the sell filled");
    r.wait(LAT + 1);
    assert_eq!(r.core.order(buy).unwrap().state, OrderState::Cancelled);
}

/// While it waits an exit may close the position: nothing more goes.
#[test]
fn a_position_closed_while_waiting_sends_nothing() {
    let mut r = Rig::new(false, CME_OPEN);
    r.long_es(1);
    r.book(es(), 600_000);
    r.cmd(Command::PlaceOrder(OrderSpec {
        account: acct(),
        contract: es(),
        side: Side::Sell,
        qty: Qty(1),
        kind: OrderKind::Limit {
            price: Price(600_000),
        },
    }))
    .unwrap();
    r.lose_cancels.store(1, Ordering::Relaxed);
    r.flatten(es());
    let before = r.markets.load(Ordering::Relaxed);
    r.wait(LAT + 1);
    assert_eq!(r.core.position(&acct(), es()), 0, "the sell filled");
    r.wait(6_000);
    assert_eq!(
        r.markets.load(Ordering::Relaxed),
        before,
        "nothing to close"
    );
    assert_eq!(r.core.position(&acct(), es()), 0);
}
