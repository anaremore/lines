//! One-step Reverse (`Command::Reverse`): cancel the contract's orders, close the old position
//! reduce-only, open the new one as a separate order, never a doubled position. On a broker that
//! doesn't honor reduce-only (`oco: false`) nothing new goes until the cancels are answered.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lines_core::{ContractKey, Month, Price, Qty, Side as BookSide, UtcMillis};
use lines_feed::catalog;
use lines_trading::broker::{BrokerEvent, BrokerRequest, BrokerSnapshot};
use lines_trading::guardrails::Guardrails;
use lines_trading::{
    AccountConfig, AccountId, Broker, BrokerCaps, Command, Event, MarketInput, Money, OrderId,
    OrderKind, OrderSpec, OrderState, PaperBroker, PaperConfig, RejectCode, Role, Side, Source,
    TradingCore,
};

const LAT: i64 = 20;
const CME_OPEN: &str = "2026-11-10T15:00:00Z";

fn es() -> ContractKey {
    ContractKey::monthly(catalog::ES, 2026, Month::new(12).unwrap())
}
fn acct() -> AccountId {
    AccountId::new("paper-1")
}

/// Orders placed: id, side, size, reduce-only.
type Placed = Arc<Mutex<Vec<(OrderId, Side, i64, bool)>>>;

/// The paper broker, optionally not honoring reduce-only, able to lose cancels or go down, and
/// recording every order placed.
struct Venue {
    inner: PaperBroker,
    oco: bool,
    lose_ids: Arc<Mutex<Vec<OrderId>>>,
    /// Drop the broker's `Modified` reports, so the core never learns a bracket's legs became
    /// reduce-only (a broker that keeps legs without marking them).
    drop_modified: Arc<AtomicBool>,
    /// Refuse cancels of these orders (too late to cancel), answering at once.
    refuse_ids: Arc<Mutex<Vec<OrderId>>>,
    refusals: Vec<BrokerEvent>,
    lose_cancels: Arc<AtomicUsize>,
    down: Arc<AtomicBool>,
    placed: Placed,
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
            BrokerRequest::Place(o) => {
                self.placed
                    .lock()
                    .unwrap()
                    .push((o.id, o.side, o.qty.0, o.reduce_only));
            }
            BrokerRequest::Bracket { entry, .. } => {
                self.placed.lock().unwrap().push((
                    entry.id,
                    entry.side,
                    entry.qty.0,
                    entry.reduce_only,
                ));
            }
            BrokerRequest::Cancel { id } => {
                if self.lose_ids.lock().unwrap().contains(id) {
                    return;
                }
                if self.refuse_ids.lock().unwrap().contains(id) {
                    self.refusals.push(BrokerEvent::Rejected {
                        id: *id,
                        reason: "too late to cancel".into(),
                        at: now,
                    });
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
        if self.drop_modified.load(Ordering::Relaxed) {
            out.retain(|e| !matches!(e, BrokerEvent::Modified { .. }));
        }
        out.append(&mut self.refusals);
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
    lose_ids: Arc<Mutex<Vec<OrderId>>>,
    refuse_ids: Arc<Mutex<Vec<OrderId>>>,
    drop_modified: Arc<AtomicBool>,
    lose_cancels: Arc<AtomicUsize>,
    down: Arc<AtomicBool>,
    placed: Placed,
}
impl Rig {
    fn with(oco: bool, account: AccountConfig) -> Rig {
        let mut inner = PaperBroker::new(PaperConfig {
            latency_ms: LAT,
            ..Default::default()
        });
        inner.open_account(acct(), Money::whole(100_000));
        let lose_ids = Arc::new(Mutex::new(Vec::new()));
        let refuse_ids = Arc::new(Mutex::new(Vec::new()));
        let drop_modified = Arc::new(AtomicBool::new(false));
        let lose_cancels = Arc::new(AtomicUsize::new(0));
        let down = Arc::new(AtomicBool::new(false));
        let placed = Arc::new(Mutex::new(Vec::new()));
        let mut core = TradingCore::new(Box::new(Venue {
            inner,
            oco,
            lose_ids: lose_ids.clone(),
            refuse_ids: refuse_ids.clone(),
            drop_modified: drop_modified.clone(),
            refusals: Vec::new(),
            lose_cancels: lose_cancels.clone(),
            down: down.clone(),
            placed: placed.clone(),
        }));
        core.add_account(account, None);
        let mut r = Rig {
            core,
            t: UtcMillis::parse_iso(CME_OPEN).unwrap().0,
            events: Vec::new(),
            lose_ids,
            refuse_ids,
            drop_modified,
            lose_cancels,
            down,
            placed,
        };
        r.book(600_000);
        r
    }
    fn new(oco: bool) -> Rig {
        Rig::with(oco, AccountConfig::paper("paper-1", 100_000))
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
    fn book(&mut self, bid: i64) {
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
        self.core.market(self.now(), es(), &i);
        self.collect();
    }
    fn pos(&self) -> i64 {
        self.core.position(&acct(), es())
    }
    fn place(&mut self, side: Side, qty: i64, kind: OrderKind) -> OrderId {
        let id = self
            .cmd(Command::PlaceOrder(OrderSpec {
                account: acct(),
                contract: es(),
                side,
                qty: Qty(qty),
                kind,
            }))
            .unwrap()[0];
        self.wait(LAT + 1);
        id
    }
    fn long(&mut self, q: i64) {
        self.place(Side::Buy, q, OrderKind::Market);
        assert_eq!(self.pos(), q);
    }
    fn stop(&mut self, q: i64) -> OrderId {
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
    fn reverse(&mut self) -> Result<Vec<OrderId>, lines_trading::Reject> {
        self.reverse_with(None, None)
    }
    fn reverse_with(
        &mut self,
        take_profit: Option<i64>,
        stop_loss: Option<i64>,
    ) -> Result<Vec<OrderId>, lines_trading::Reject> {
        self.cmd(Command::Reverse {
            account: acct(),
            contract: es(),
            take_profit: take_profit.map(Price),
            stop_loss: stop_loss.map(Price),
        })
    }
    fn state(&self, id: OrderId) -> OrderState {
        self.core.order(id).unwrap().state.clone()
    }
    fn open_orders(&self) -> Vec<lines_trading::Order> {
        self.core
            .open_orders(&acct())
            .filter(|o| o.contract == es())
            .cloned()
            .collect()
    }
    /// Orders placed since `from`: side, size, reduce-only.
    fn placed_since(&self, from: usize) -> Vec<(Side, i64, bool)> {
        self.placed.lock().unwrap()[from..]
            .iter()
            .map(|p| (p.1, p.2, p.3))
            .collect()
    }
    fn placed_count(&self) -> usize {
        self.placed.lock().unwrap().len()
    }
}

/// Long 2: two orders, the close reduce-only for 2 and the opening for 2, never one of 4. Short 2,
/// and the old side's orders are gone.
#[test]
fn reverse_closes_reduce_only_and_opens_separately() {
    let mut r = Rig::new(true);
    r.long(2);
    let limit = r.place(
        Side::Sell,
        1,
        OrderKind::Limit {
            price: Price(610_000),
        },
    );
    let from = r.placed_count();
    r.reverse().unwrap();
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(r.pos(), -2);
    assert_eq!(
        r.placed_since(from),
        [(Side::Sell, 2, true), (Side::Sell, 2, false)],
        "the close reduce-only, the opening its own order"
    );
    assert_eq!(r.state(limit), OrderState::Cancelled);
    assert!(r.open_orders().is_empty(), "nothing of the old side left");
}

/// Where reduce-only is honored, a protective exit stays until the position is flat, then goes:
/// it can never add to the new side.
#[test]
fn an_exit_protects_until_flat_and_never_joins_the_new_side() {
    let mut r = Rig::new(true);
    r.long(2);
    let stop = r.stop(2);
    r.reverse().unwrap();
    // Before the broker answers: the stop is still there.
    assert!(r.core.order(stop).unwrap().is_open());
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(r.pos(), -2);
    assert_eq!(r.state(stop), OrderState::Cancelled);
    // The market falls through the old stop's price: nothing more sells.
    r.book(585_000);
    r.wait(LAT + 1);
    assert_eq!(r.pos(), -2);
}

/// On `oco: false` the orders are cancelled first and nothing new goes until the broker answers;
/// then the close and the opening.
#[test]
fn without_reduce_only_it_waits_for_the_cancels() {
    let mut r = Rig::new(false);
    r.long(2);
    // Such a broker holds no exits: a plain sell rests.
    let limit = r.place(
        Side::Sell,
        2,
        OrderKind::Limit {
            price: Price(610_000),
        },
    );
    let from = r.placed_count();
    r.reverse().unwrap();
    assert!(
        r.placed_since(from).is_empty(),
        "nothing before the cancel's answer"
    );
    r.wait(LAT + 1);
    assert_eq!(r.state(limit), OrderState::Cancelled);
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(
        r.placed_since(from),
        [(Side::Sell, 2, true), (Side::Sell, 2, false)]
    );
    assert_eq!(r.pos(), -2);
}

/// On `oco: false` with a cancel that never comes back: after the wait the close goes (as a
/// Flatten's would) but the opening is refused, saying why, since the old order could still fill
/// beside it. Flat, never doubled.
#[test]
fn without_reduce_only_a_lost_cancel_closes_but_doesnt_open() {
    let mut r = Rig::new(false);
    r.long(2);
    let limit = r.place(
        Side::Sell,
        2,
        OrderKind::Limit {
            price: Price(610_000),
        },
    );
    r.lose_ids.lock().unwrap().push(limit);
    let from = r.placed_count();
    r.reverse().unwrap();
    r.wait(4_000);
    assert!(r.placed_since(from).is_empty(), "still waiting");
    let wake = r.core.next_wake().expect("a time to stop waiting");
    assert!(wake.0 > r.t);
    r.wait(1_100);
    r.wait(LAT + 1);
    assert_eq!(
        r.placed_since(from),
        [(Side::Sell, 2, true)],
        "the close only"
    );
    assert_eq!(r.pos(), 0);
    let refused = r.events.iter().rev().find_map(|e| match e {
        Event::Order(o) if o.role != Role::Flatten => match &o.state {
            OrderState::Rejected { reason } => Some(reason.clone()),
            _ => None,
        },
        _ => None,
    });
    let why = refused.expect("the opening is refused as an order");
    assert!(
        why.contains("didn't open the new one") && why.contains("never answered its cancel"),
        "{why}"
    );
}

/// On a thin book (one small level, as a top-of-book feed gives) both orders work on, and the
/// opening stays an entry: when liquidity comes the close fills first and the opening still opens
/// the new side, never shrunk away as a second close.
#[test]
fn on_a_thin_book_the_opening_still_opens() {
    let mut r = Rig::new(true);
    r.long(4);
    let thin = |r: &mut Rig, bid: i64| {
        r.t += 1;
        let i = [
            MarketInput::BookClear,
            MarketInput::Book {
                side: BookSide::Bid,
                price: Price(600_000),
                qty: Qty(bid),
            },
            MarketInput::Book {
                side: BookSide::Ask,
                price: Price(600_025),
                qty: Qty(500),
            },
        ];
        r.core.market(r.now(), es(), &i);
        r.collect();
    };
    // The one lot bid is taken by an earlier sell: the close finds nothing yet.
    thin(&mut r, 1);
    r.place(Side::Sell, 1, OrderKind::Market);
    assert_eq!(r.pos(), 3);
    r.reverse().unwrap();
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(r.pos(), 3, "nothing to take yet: both work on");
    thin(&mut r, 20);
    r.wait(1);
    assert_eq!(r.pos(), -3, "closed, then opened");
    assert!(r.open_orders().is_empty());
}

/// While a Flatten is closing the position a Reverse is refused: it would open beside the close.
#[test]
fn a_reverse_while_a_flatten_closes_is_refused() {
    let mut r = Rig::new(true);
    r.long(2);
    r.cmd(Command::Flatten {
        account: acct(),
        contract: Some(es()),
    })
    .unwrap();
    let e = r.reverse().unwrap_err();
    assert!(e.message.contains("a Flatten is closing"), "{}", e.message);
    r.wait(LAT + 1);
    assert_eq!(r.pos(), 0);
}

/// On `oco: false` the old side's resting sell fills while the Reverse waits: flat, so nothing
/// new opens, and the refusal says why.
#[test]
fn a_position_closed_during_the_wait_opens_nothing() {
    let mut r = Rig::new(false);
    r.long(2);
    let limit = r.place(
        Side::Sell,
        2,
        OrderKind::Limit {
            price: Price(600_100),
        },
    );
    // Its cancel is lost: the Reverse waits, and the sell fills meanwhile.
    r.lose_ids.lock().unwrap().push(limit);
    r.reverse().unwrap();
    // A trade through it fills it.
    r.t += 1;
    let time = r.now();
    r.core.market(
        time,
        es(),
        &[MarketInput::Trade {
            price: Price(600_200),
            qty: Qty(5),
            time,
        }],
    );
    r.collect();
    r.wait(1);
    assert_eq!(r.pos(), 0, "the resting sell filled");
    for _ in 0..3 {
        r.wait(2_000);
    }
    assert_eq!(r.pos(), 0, "nothing new opened");
    let why = r
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Order(o) => match &o.state {
                OrderState::Rejected { reason } => Some(reason.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("said as a refused order");
    assert!(
        why.starts_with("the Reverse didn't open the new position: the position closed"),
        "{why}"
    );
}

/// One at a time: a second Reverse while the first is on its way is refused, so a double press
/// never flips twice.
#[test]
fn a_second_reverse_while_one_is_in_flight_is_refused() {
    let mut r = Rig::new(true);
    r.long(2);
    r.reverse().unwrap();
    let e = r.reverse().unwrap_err();
    assert!(e.message.contains("already on its way"), "{}", e.message);
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(r.pos(), -2);
    // Done: a new one may go, and flips back.
    r.reverse().unwrap();
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(r.pos(), 2);
}

/// A strategy or rule can't Reverse: it's a person's command.
#[test]
fn reverse_is_a_persons_command() {
    let mut r = Rig::new(true);
    r.long(1);
    let id = r.core.submit(
        r.now(),
        Source::Rule,
        Instant::now(),
        Command::Reverse {
            account: acct(),
            contract: es(),
            take_profit: None,
            stop_loss: None,
        },
    );
    r.collect();
    let result = r.events.iter().find_map(|e| match e {
        Event::Command { id: i, result, .. } if *i == id => Some(result.clone()),
        _ => None,
    });
    let e = result.expect("answered").unwrap_err();
    assert!(e.message.contains("a person's command"), "{}", e.message);
    assert_eq!(r.pos(), 1);
}

/// Nothing to reverse, or a broker that can't be reached: refused, nothing sent.
#[test]
fn reverse_with_no_position_or_no_broker_sends_nothing() {
    let mut r = Rig::new(true);
    let e = r.reverse().unwrap_err();
    assert!(e.message.contains("no position"), "{}", e.message);
    r.long(1);
    r.down.store(true, Ordering::Relaxed);
    let from = r.placed_count();
    let e = r.reverse().unwrap_err();
    assert_eq!(e.code, RejectCode::Broker);
    assert!(r.placed_since(from).is_empty());
    assert_eq!(r.pos(), 1);
}

/// The opening is an entry for guardrails: a position limit it would break refuses the whole
/// Reverse, nothing sent and the position and its orders left.
#[test]
fn a_guardrail_refuses_the_whole_reverse() {
    let mut r = Rig::with(true, AccountConfig::paper("paper-1", 100_000));
    r.long(2);
    let stop = r.stop(2);
    r.cmd(Command::SetGuardrails {
        account: acct(),
        guardrails: Guardrails {
            max_position: Some(1),
            ..Default::default()
        },
    })
    .unwrap();
    let from = r.placed_count();
    let e = r.reverse().unwrap_err();
    assert_eq!(e.code, RejectCode::Guardrail, "{}", e.message);
    assert!(
        e.message.contains("can't open the new position"),
        "{}",
        e.message
    );
    r.wait(LAT + 1);
    assert!(r.placed_since(from).is_empty());
    assert_eq!(r.pos(), 2);
    assert!(r.core.order(stop).unwrap().is_open(), "its orders stand");
}

/// The bracket setting applies to the new position: the opening goes as a bracket with its exits
/// on the new side.
#[test]
fn the_new_position_gets_its_bracket() {
    let mut r = Rig::new(true);
    r.long(1);
    r.reverse_with(Some(599_000), Some(601_000)).unwrap();
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(r.pos(), -1);
    let exits: Vec<_> = r
        .open_orders()
        .into_iter()
        .map(|o| (o.role, o.side, o.kind))
        .collect();
    assert!(exits.contains(&(
        Role::TakeProfit,
        Side::Buy,
        OrderKind::Limit {
            price: Price(599_000)
        }
    )));
    assert!(exits.contains(&(
        Role::StopLoss,
        Side::Buy,
        OrderKind::Stop {
            trigger: Price(601_000)
        }
    )));
}

/// A Flatten or a person's Cancel All takes back a Reverse still waiting on `oco: false`: the
/// position is closed (Flatten) or left (Cancel All), never reversed afterwards, and it says so.
#[test]
fn a_flatten_or_cancel_all_takes_back_a_waiting_reverse() {
    for flatten in [true, false] {
        let mut r = Rig::new(false);
        r.long(2);
        r.place(
            Side::Sell,
            2,
            OrderKind::Limit {
                price: Price(610_000),
            },
        );
        r.lose_cancels.store(1, Ordering::Relaxed);
        r.reverse().unwrap();
        let c = if flatten {
            Command::Flatten {
                account: acct(),
                contract: Some(es()),
            }
        } else {
            Command::CancelAll {
                account: acct(),
                contract: Some(es()),
            }
        };
        r.cmd(c).unwrap();
        for _ in 0..4 {
            r.wait(2_000);
        }
        assert_eq!(r.pos(), if flatten { 0 } else { 2 }, "flatten {flatten}");
        let said = r.events.iter().any(|e| {
            matches!(e, Event::Order(o) if matches!(&o.state,
                OrderState::Rejected { reason } if reason.starts_with("the Reverse was taken back")))
        });
        assert!(said, "flatten {flatten}");
    }
}

/// A rule's Cancel All leaves a person's waiting Reverse, and says nothing of taking it back.
#[test]
fn a_rules_cancel_all_leaves_a_waiting_reverse() {
    let mut r = Rig::new(false);
    r.long(2);
    r.place(
        Side::Sell,
        2,
        OrderKind::Limit {
            price: Price(610_000),
        },
    );
    r.lose_cancels.store(1, Ordering::Relaxed);
    r.reverse().unwrap();
    r.core.submit(
        r.now(),
        Source::Rule,
        Instant::now(),
        Command::CancelAll {
            account: acct(),
            contract: Some(es()),
        },
    );
    r.collect();
    for _ in 0..4 {
        r.wait(2_000);
    }
    assert_eq!(r.pos(), -2, "still reversed");
    let taken_back = r.events.iter().any(|e| {
        matches!(e, Event::Order(o) if matches!(&o.state,
            OrderState::Rejected { reason } if reason.starts_with("the Reverse was taken back")))
    });
    assert!(!taken_back);
}

/// On `oco: false` the broker refuses a cancel (too late): that order still works, so the close goes
/// and the opening is refused, saying why, within the cap and with no cancel asked again.
#[test]
fn a_refused_cancel_closes_but_doesnt_open() {
    let mut r = Rig::new(false);
    r.long(2);
    let limit = r.place(
        Side::Sell,
        2,
        OrderKind::Limit {
            price: Price(610_000),
        },
    );
    r.refuse_ids.lock().unwrap().push(limit);
    let from = r.placed_count();
    let cancels = |r: &Rig| {
        r.events
            .iter()
            .filter(|e| matches!(e, Event::CancelFailed { order, .. } if *order == limit))
            .count()
    };
    r.reverse().unwrap();
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(
        r.placed_since(from),
        [(Side::Sell, 2, true)],
        "the close only"
    );
    assert_eq!(r.pos(), 0);
    assert_eq!(cancels(&r), 1, "said once");
    for _ in 0..4 {
        r.wait(2_000);
    }
    assert_eq!(cancels(&r), 1, "not asked again");
    assert!(r.core.order(limit).unwrap().is_open(), "it still works");
    let why = r
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Order(o) if o.role != Role::Flatten => match &o.state {
                OrderState::Rejected { reason } => Some(reason.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("the opening is refused as an order");
    assert!(
        why.contains("refused its cancel or has no known outcome"),
        "{why}"
    );
    // Over: another Reverse isn't held back by it.
    let e = r.reverse().unwrap_err();
    assert!(e.message.contains("no position"), "{}", e.message);
}

/// An order a rebuild left with no known outcome may still be at the venue, and a Reverse can't
/// cancel it: refused at the press, nothing sent, the position and its orders left.
#[test]
fn an_unresolved_order_refuses_the_reverse() {
    for oco in [true, false] {
        let mut r = Rig::new(oco);
        r.long(2);
        let limit = r
            .cmd(Command::PlaceOrder(OrderSpec {
                account: acct(),
                contract: es(),
                side: Side::Sell,
                qty: Qty(2),
                kind: OrderKind::Limit {
                    price: Price(610_000),
                },
            }))
            .unwrap()[0];
        // A rebuild before the venue answered: the limit's outcome is unknown.
        let now = r.now();
        r.core.rebuild(now);
        r.collect();
        assert_eq!(r.state(limit), OrderState::Unresolved);
        let from = r.placed_count();
        let e = r.reverse().unwrap_err();
        assert!(e.message.contains("no known outcome"), "{}", e.message);
        r.wait(LAT + 1);
        assert!(r.placed_since(from).is_empty(), "oco {oco}");
        assert_eq!(r.pos(), 2);
    }
}

/// A bracket's legs the broker keeps but the core doesn't know are reduce-only are cancelled at
/// once, as any plain order: kept through the flip, one could fill against the new side.
#[test]
fn a_bracket_leg_not_known_reduce_only_is_cancelled_at_once() {
    let mut r = Rig::new(true);
    r.drop_modified.store(true, Ordering::Relaxed);
    let ids = r
        .cmd(Command::PlaceBracket(lines_trading::BracketSpec {
            entry: OrderSpec {
                account: acct(),
                contract: es(),
                side: Side::Buy,
                qty: Qty(2),
                kind: OrderKind::Market,
            },
            take_profit: Some(Price(610_000)),
            stop_loss: Some(Price(590_000)),
        }))
        .unwrap();
    r.wait(LAT + 1);
    assert_eq!(r.pos(), 2);
    let legs: Vec<OrderId> = ids[1..].to_vec();
    assert!(
        legs.iter()
            .all(|id| !r.core.order(*id).unwrap().reduce_only)
    );
    // Cancelled at the press, not kept until flat.
    let asked = r.reverse().unwrap();
    assert!(legs.iter().all(|l| asked.contains(l)), "{asked:?}");
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(r.pos(), -2);
    for id in legs {
        assert_eq!(r.state(id), OrderState::Cancelled, "leg {id:?}");
    }
    // The market falls through the old stop's price: nothing more sells.
    r.book(585_000);
    r.wait(LAT + 1);
    assert_eq!(r.pos(), -2);
}

/// On `oco: true` an exit whose earlier cancel the broker refused is kept protecting, as any exit
/// is: reduce-only, it can't add to the new side, so the opening still goes.
#[test]
fn a_kept_exit_with_an_old_refused_cancel_doesnt_stop_the_opening() {
    let mut r = Rig::new(true);
    r.long(2);
    let stop = r.stop(2);
    r.refuse_ids.lock().unwrap().push(stop);
    r.cmd(Command::CancelOrder {
        account: acct(),
        order: stop,
    })
    .unwrap();
    r.wait(LAT + 1);
    assert!(r.core.order(stop).unwrap().is_open(), "the venue refused");
    r.refuse_ids.lock().unwrap().clear();
    r.reverse().unwrap();
    r.wait(LAT + 1);
    r.wait(LAT + 1);
    assert_eq!(r.pos(), -2);
}

/// On `oco: false` an order placed while a Reverse waits is cancelled, and the Reverse waits for
/// that answer too before it closes and opens: never short 4 from a long 2.
#[test]
fn an_order_placed_during_the_wait_is_waited_on_too() {
    let mut r = Rig::new(false);
    r.long(2);
    r.place(
        Side::Sell,
        2,
        OrderKind::Limit {
            price: Price(610_000),
        },
    );
    r.lose_cancels.store(1, Ordering::Relaxed);
    let from = r.placed_count();
    r.reverse().unwrap();
    // During the wait the trader places another sell; its cancel will be lost too.
    let late_sell = r
        .cmd(Command::PlaceOrder(OrderSpec {
            account: acct(),
            contract: es(),
            side: Side::Sell,
            qty: Qty(2),
            kind: OrderKind::Limit {
                price: Price(610_000),
            },
        }))
        .unwrap()[0];
    r.lose_ids.lock().unwrap().push(late_sell);
    // The first cancel goes again at 2 s and is answered; the new order's cancel goes, unanswered.
    r.wait(2_100);
    r.wait(LAT + 1);
    assert_eq!(
        r.placed_since(from),
        [(Side::Sell, 2, false)],
        "only the trader's own sell: nothing beside an unanswered cancel"
    );
    // Past the cap: the close goes, the opening is refused.
    for _ in 0..3 {
        r.wait(1_000);
    }
    r.wait(LAT + 1);
    let mine: Vec<_> = r.placed_since(from + 1);
    assert_eq!(mine, [(Side::Sell, 2, true)], "the close only");
    assert_eq!(r.pos(), 0);
}
