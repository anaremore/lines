//! Option orders a person didn't mean, and a broker's refusals: an order crossing through flat is
//! refused before it goes; one the broker rejects carries the broker's own words to the order, and
//! to a fired trigger's row, where nobody is watching a ticket. A rejected order leaves nothing
//! working.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lines_core::{CivilDate, ContractKey, Price, Qty, Right, UtcMillis};
use lines_feed::catalog;
use lines_trading::broker::{BrokerEvent, BrokerRequest, BrokerSnapshot};
use lines_trading::trigger::{TriggerKind, TriggerSpec, TriggerState, TriggerView};
use lines_trading::{
    AccountConfig, AccountId, Broker, BrokerCaps, Command, Event, MarketInput, Money, OrderId,
    OrderKind, OrderSpec, OrderState, PaperBroker, PaperConfig, Reject, RejectCode, Side, Source,
    TradingCore,
};

const LAT: i64 = 20;
const RTH: &str = "2026-11-09T15:00:00Z";
/// What a real broker might say: its words, not ours.
const SAYS: &str = "Order rejected: account is not approved for this options strategy (code 4021)";

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

/// The paper broker, rejecting new orders with its own reason while `refuse` is on.
struct Venue {
    inner: PaperBroker,
    refuse: Arc<AtomicBool>,
    out: Arc<Mutex<Vec<BrokerEvent>>>,
}
impl Broker for Venue {
    fn name(&self) -> &str {
        "venue"
    }
    fn capabilities(&self) -> BrokerCaps {
        self.inner.capabilities()
    }
    fn request(&mut self, now: UtcMillis, req: BrokerRequest) {
        if let BrokerRequest::Place(o) = &req
            && self.refuse.load(Ordering::Relaxed)
        {
            self.out.lock().unwrap().push(BrokerEvent::Rejected {
                id: o.id,
                reason: SAYS.to_owned(),
                at: UtcMillis(now.0 + LAT),
            });
            return;
        }
        self.inner.request(now, req)
    }
    fn market(&mut self, now: UtcMillis, contract: ContractKey, input: &MarketInput) {
        self.inner.market(now, contract, input)
    }
    fn poll(&mut self, now: UtcMillis, out: &mut Vec<BrokerEvent>) {
        self.inner.poll(now, out);
        out.append(&mut self.out.lock().unwrap());
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
    refuse: Arc<AtomicBool>,
}
impl Rig {
    fn new() -> Rig {
        let mut inner = PaperBroker::new(PaperConfig {
            latency_ms: LAT,
            ..Default::default()
        });
        inner.open_account(acct(), Money::whole(100_000));
        let refuse = Arc::new(AtomicBool::new(false));
        let mut core = TradingCore::new(Box::new(Venue {
            inner,
            refuse: refuse.clone(),
            out: Arc::new(Mutex::new(Vec::new())),
        }));
        core.add_account(AccountConfig::paper("paper-1", 100_000), None);
        Rig {
            core,
            t: UtcMillis::parse_iso(RTH).unwrap().0,
            events: Vec::new(),
            refuse,
        }
    }
    fn now(&self) -> UtcMillis {
        UtcMillis(self.t)
    }
    fn collect(&mut self) {
        self.events.extend(self.core.drain());
    }
    fn cmd(&mut self, c: Command) -> Result<Vec<OrderId>, Reject> {
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
    fn quote(&mut self, bid: i64, ask: i64) {
        self.t += 1;
        let time = self.now();
        self.core.market(
            time,
            call(),
            &[MarketInput::OptionQuote {
                bid: Some(Price(bid)),
                ask: Some(Price(ask)),
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
            time,
            spy(),
            &[MarketInput::Trade {
                price: Price(cents),
                qty: Qty(1),
                time,
            }],
        );
        self.collect();
    }
    fn order(&mut self, side: Side, qty: i64, cents: i64) -> Result<Vec<OrderId>, Reject> {
        self.cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: call(),
            side,
            qty: Qty(qty),
            kind: OrderKind::Limit {
                price: Price(cents),
            },
        }))
    }
    fn position(&self) -> i64 {
        self.core.position(&acct(), call())
    }
    /// A position of `qty` (negative: short), at the quote 2.40 / 2.42.
    fn holding(qty: i64) -> Rig {
        let mut r = Rig::new();
        r.print(68_300);
        r.quote(240, 242);
        if qty != 0 {
            let (side, at) = if qty > 0 {
                (Side::Buy, 242)
            } else {
                (Side::Sell, 240)
            };
            r.order(side, qty.abs(), at).unwrap();
            r.wait(LAT + 1);
        }
        assert_eq!(r.position(), qty);
        r
    }
    fn trigger(&self) -> TriggerView {
        self.events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Trigger(t) => Some(t.clone()),
                _ => None,
            })
            .expect("a trigger")
    }
    fn working(&self) -> usize {
        self.core
            .open_orders(&acct())
            .chain(self.core.in_flight())
            .filter(|o| o.contract == call())
            .count()
    }
}

/// Long 1, a sell of 5 by mistake: refused before it goes, naming Close. What fits goes.
#[test]
fn an_option_order_through_flat_is_refused() {
    let mut r = Rig::holding(1);
    let e = r.order(Side::Sell, 5, 240).unwrap_err();
    assert_eq!(e.code, RejectCode::Risk);
    assert_eq!(
        e.message,
        "selling 5 would turn your long 1 into short 4: use Close, or sell 1 at most"
    );
    assert_eq!(r.position(), 1);
    r.order(Side::Sell, 1, 240).unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.position(), 0);

    // Short 2, a buy of 3: the mirror.
    let mut r = Rig::holding(-2);
    let e = r.order(Side::Buy, 3, 242).unwrap_err();
    assert_eq!(
        e.message,
        "buying 3 would turn your short 2 into long 1: use Close, or buy 2 at most"
    );
}

/// Working orders on that side count: long 2 with a sell resting, another sell could end short.
/// An exit counts too: if it fills first, the plain sell opens a short.
#[test]
fn working_orders_count_toward_crossing() {
    let mut r = Rig::holding(2);
    r.order(Side::Sell, 1, 300).unwrap();
    r.wait(LAT + 1);
    let e = r.order(Side::Sell, 2, 300).unwrap_err();
    assert_eq!(
        e.message,
        "selling 2 with the 1 already working would turn your long 2 into short 1: use Close, or sell 1 at most"
    );
    r.order(Side::Sell, 1, 300).unwrap();
    r.wait(LAT + 1);
    let e = r.order(Side::Sell, 1, 300).unwrap_err();
    assert!(
        e.message
            .ends_with(": what's working already closes it, or use Close"),
        "{}",
        e.message
    );

    let mut r = Rig::holding(2);
    r.cmd(Command::PlaceExit(OrderSpec {
        account: acct(),
        contract: call(),
        side: Side::Sell,
        qty: Qty(2),
        kind: OrderKind::Limit { price: Price(300) },
    }))
    .unwrap();
    r.wait(LAT + 1);
    // The exit could fill first and leave this one to open a short.
    let e = r.order(Side::Sell, 1, 300).unwrap_err();
    assert!(
        e.message.contains("with the 2 already working"),
        "{}",
        e.message
    );
    assert_eq!(r.position(), 2);
}

/// Growing a working sell is checked the same way: long 2, a sell of 1 resting, grown to 3.
#[test]
fn growing_an_order_through_flat_is_refused() {
    let mut r = Rig::holding(2);
    let id = r.order(Side::Sell, 1, 300).unwrap()[0];
    r.wait(LAT + 1);
    let e = r
        .cmd(Command::ModifyOrder {
            account: acct(),
            order: id,
            qty: Some(Qty(3)),
            price: None,
        })
        .unwrap_err();
    assert_eq!(
        e.message,
        "selling 2 with the 1 already working would turn your long 2 into short 1: use Close, or sell 1 at most"
    );
    r.cmd(Command::ModifyOrder {
        account: acct(),
        order: id,
        qty: Some(Qty(2)),
        price: None,
    })
    .unwrap();
}

/// A Flatten's order working counts, and so does one a rebuild left with no known outcome: either
/// could still close the position first.
#[test]
fn a_flatten_or_an_unknown_order_counts_too() {
    let mut r = Rig::holding(1);
    // Its market order on its way.
    r.cmd(Command::Flatten {
        account: acct(),
        contract: Some(call()),
    })
    .unwrap();
    let e = r.order(Side::Sell, 1, 300).unwrap_err();
    assert!(e.message.contains("already working"), "{}", e.message);
    r.wait(LAT + 1);
    assert_eq!(r.position(), 0);

    let mut r = Rig::holding(1);
    let id = r.order(Side::Sell, 1, 300).unwrap()[0];
    r.core.rebuild(r.now());
    r.collect();
    assert_eq!(r.core.order(id).unwrap().state, OrderState::Unresolved);
    let e = r.order(Side::Sell, 1, 300).unwrap_err();
    assert!(e.message.contains("already working"), "{}", e.message);
}

/// An Entry (a buy) bigger than the short it's armed on would cross it when it fires, unwatched:
/// refused when armed, while the trader is looking.
#[test]
fn an_entry_through_a_short_is_refused_when_armed() {
    let mut r = Rig::holding(-2);
    let arm = |r: &mut Rig, qty| {
        r.cmd(Command::ArmTrigger(TriggerSpec {
            account: acct(),
            contract: call(),
            kind: TriggerKind::Entry {
                qty: Qty(qty),
                max_price: Price(300),
            },
            level: Price(68_400),
        }))
    };
    let e = arm(&mut r, 3).unwrap_err();
    assert_eq!(
        e.message,
        "buying 3 would turn your short 2 into long 1: use Close, or buy 2 at most"
    );
    arm(&mut r, 2).unwrap();
}

/// From flat, a sell opens a short: the owner's ruling allows it (a real broker may reject it;
/// paper doesn't). Adding to a position is no crossing either.
#[test]
fn a_sell_from_flat_and_adding_are_allowed() {
    let mut r = Rig::holding(0);
    r.order(Side::Sell, 1, 240).unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.position(), -1);
    r.order(Side::Sell, 3, 240).unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.position(), -4);
}

/// A broker's rejection keeps the broker's words on the order, and nothing is left working.
#[test]
fn a_brokers_rejection_carries_its_reason_and_leaves_nothing() {
    let mut r = Rig::holding(0);
    r.refuse.store(true, Ordering::Relaxed);
    let id = r.order(Side::Sell, 1, 240).unwrap()[0];
    r.wait(LAT + 1);
    let state = r.core.order(id).unwrap().state.clone();
    assert_eq!(
        state,
        OrderState::Rejected {
            reason: SAYS.to_owned()
        }
    );
    let reported = r.events.iter().any(|e| {
        matches!(e, Event::Order(o) if o.id == id
            && matches!(&o.state, OrderState::Rejected { reason } if reason == SAYS))
    });
    assert!(reported, "the app hears it in the broker's words");
    assert_eq!(r.working(), 0);
    assert_eq!(r.position(), 0);
    // The next order isn't held back by it.
    r.refuse.store(false, Ordering::Relaxed);
    r.order(Side::Sell, 1, 240).unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.position(), -1);
}

/// A fired Stop whose market order the broker rejects: its row says so in the broker's words, the
/// position keeps its Stop, and it tries again on a new quote.
#[test]
fn a_fired_stops_rejection_shows_on_its_row() {
    let mut r = Rig::holding(1);
    r.cmd(Command::ArmTrigger(TriggerSpec {
        account: acct(),
        contract: call(),
        kind: TriggerKind::Stop,
        level: Price(68_200),
    }))
    .unwrap();
    r.refuse.store(true, Ordering::Relaxed);
    r.print(68_100);
    r.wait(LAT + 1);
    r.wait(1);
    let v = r.trigger();
    assert_eq!(v.state, TriggerState::Working);
    let note = v.note.unwrap_or_default();
    assert!(note.contains(SAYS), "{note}");
    assert_eq!(r.position(), 1);
    assert_eq!(r.working(), 0, "the rejected order left nothing");
    // The broker takes it on the next quote: closed.
    r.refuse.store(false, Ordering::Relaxed);
    r.quote(150, 152);
    r.wait(LAT + 1);
    assert_eq!(r.position(), 0);
}

/// A fired Entry whose limit the broker rejects: the Entry is reported ended again, in the broker's
/// words, and nothing is left working.
#[test]
fn a_fired_entrys_rejection_shows_on_its_row() {
    let mut r = Rig::holding(0);
    r.cmd(Command::ArmTrigger(TriggerSpec {
        account: acct(),
        contract: call(),
        kind: TriggerKind::Entry {
            qty: Qty(1),
            max_price: Price(300),
        },
        level: Price(68_400),
    }))
    .unwrap();
    r.refuse.store(true, Ordering::Relaxed);
    r.print(68_400);
    r.wait(LAT + 1);
    let v = r.trigger();
    assert_eq!(v.state, TriggerState::Ended);
    let reason = v.reason.unwrap_or_default();
    assert!(reason.starts_with("the broker rejected order"), "{reason}");
    assert!(reason.ends_with(SAYS), "{reason}");
    assert_eq!(r.working(), 0);
    assert_eq!(r.position(), 0);
}

/// A fired Target the broker rejects: the same, and it's placed again at the next chase.
#[test]
fn a_fired_targets_rejection_shows_on_its_row() {
    let mut r = Rig::holding(1);
    r.cmd(Command::ArmTrigger(TriggerSpec {
        account: acct(),
        contract: call(),
        kind: TriggerKind::Target,
        level: Price(68_500),
    }))
    .unwrap();
    r.refuse.store(true, Ordering::Relaxed);
    r.print(68_500);
    r.wait(LAT + 1);
    r.wait(3_000);
    let note = r.trigger().note.unwrap_or_default();
    assert!(note.contains(SAYS), "{note}");
    assert_eq!(r.position(), 1);
    r.refuse.store(false, Ordering::Relaxed);
    r.wait(3_000);
    r.wait(LAT + 1);
    assert_eq!(r.position(), 0);
}
